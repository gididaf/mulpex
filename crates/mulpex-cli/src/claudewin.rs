//! Starting, re-starting and muting a `claude` window.
//!
//! Everything about how a `claude` is launched lives here, because the three
//! callers have to agree exactly: `mpx up`/`mpx new` (a plain instance), a
//! `hub_spawn` child (the same thing plus a task on its command line), and
//! restart-in-place (the same thing again, `--resume`d into the window it was
//! already in).
//!
//! **The task goes on the command line, never into the TUI.** Typing it capped
//! every task at 1022 characters with no error anywhere — see
//! `mulpex_core::rules::spawn_prompt`. And the argv reaches tmux through a spec
//! file, because a tmux command is capped at ~16 KB and the system prompt alone is
//! 14 KB (`spec.rs`).

use std::path::Path;

use anyhow::{bail, Context, Result};
use mulpex_core::rules::SpawnTask;

use crate::core::{self, Project};
use crate::tmux::Tmux;
use crate::{claude, spec};

/// The uuid `claude` was given, kept on the window so a restart can `--resume`
/// that conversation. tmux is the state store: nothing else records it, and
/// nothing else has to — the window and the conversation live and die together.
const SESSION_ID_OPT: &str = "@mpx_session_id";

/// Whether this instance is muted. Parked on the window for the same reason —
/// it is a property of the row, and the row is the window.
pub const MUTED_OPT: &str = "@mpx_muted";

/// Start one `claude` in its own tmux window, and tag the window so tmux itself
/// remembers which instance it is.
pub fn spawn(t: &Tmux, p: &Project, id: usize, task: Option<SpawnTask>) -> Result<String> {
    let claude_bin = claude::resolve().context("cannot find `claude` — run `mpx doctor`")?;
    // Rewritten before EVERY spawn, not once per project: the config files are the
    // only write-once state in the tree, and that is exactly how a long-running
    // Mulpex once lost the two files `claude --settings` needs.
    mulpex_core::state_dir::write_state_dir(&p.state_dir, &claude::helper_path())?;

    let session_id = mulpex_core::persist::new_uuid();
    let name = match task.as_ref().and_then(|t| name_from_task(&t.task)) {
        Some(label) => format!("claude#{id} ▸ {}", crate::core::sanitize_label(&label)),
        None => format!("claude#{id}"),
    };

    if let Some(t) = &task {
        // Published BEFORE the child exists, because a spawned child's task is
        // never captured by the `UserPromptSubmit` hook — the injected prompt
        // carries the `[mulpex:hub]` sentinel, which that hook skips. Without this
        // every spawned instance reports `task: ""` to `hub_instances` for its
        // whole life, which is exactly the reading that made a merely-slow spawn
        // look like a dropped one.
        let tasks = p.state_dir.join("tasks");
        let _ = std::fs::create_dir_all(&tasks);
        let _ = std::fs::write(tasks.join(id.to_string()), t.task.trim());
        mark_delivery(&p.state_dir, id, "pending");
    }

    let argv = build_argv(&claude_bin, p, &session_id, false, task.as_ref(), id);
    let window = launch(t, p, id, &name, &argv, WindowSlot::New)?;
    t.set_user_option(&window, true, "@mpx_id", &id.to_string())?;
    t.set_user_option(&window, true, "@mpx_kind", "claude")?;
    t.set_user_option(&window, true, SESSION_ID_OPT, &session_id)?;
    Ok(window)
}

/// Quit and relaunch a claude **in the window it is already in**, resuming its
/// conversation.
///
/// The one way to make a running instance re-read its environment — a rotated
/// token, a newly installed skill. What must survive is everything that makes it
/// the same instance: its number, its name, its position in the window bar, and
/// its inbox.
pub fn restart(t: &Tmux, p: &Project, id: usize) -> Result<()> {
    let inst = p.find(id).with_context(|| format!("no instance #{id}"))?;
    if !inst.is_claude() {
        bail!("term#{id} is a terminal — only a claude can be restarted");
    }
    let session_id = t.user_option(&inst.window, SESSION_ID_OPT);
    // `--resume` needs a conversation to resume. The status file is written by the
    // hook on the instance's first turn, so its presence is exactly "this claude
    // has said something" — the same test the desktop app's `worked` set makes.
    let worked = p.state_dir.join(id.to_string()).exists();
    if session_id.is_empty() || !worked {
        bail!(
            "claude#{id} has no conversation to resume yet — send it a prompt first, \
             or close it with `mpx close {id}` and open a new one"
        );
    }

    // The status file is an assertion left by a process that no longer exists. An
    // instance killed while it was waiting on the user would go on telling every
    // peer that it `needs` something. Removing it reads as "booting". Deliberately
    // NOT cleared: the inbox, the task line and the `named` marker — those describe
    // the instance, which is the thing being kept.
    let _ = std::fs::remove_file(p.state_dir.join(id.to_string()));
    // `armed/<id>` tracks a *live* hub-listener Monitor, and that process died with
    // the old child. Left behind, the hook sees the flag, skips the arm nudge for
    // good, and the resumed instance is never woken by hub mail again — with
    // nothing anywhere to say so.
    let _ = std::fs::remove_file(p.state_dir.join("armed").join(id.to_string()));
    // ...and because it was cleared, the resumed child has to re-arm, which with no
    // task on its command line only happens if it takes a turn. The one thing that
    // makes it take one is the "orphaned background task" wake the dead Monitor is
    // about to produce — and the hook swallows that wake by default (it is what
    // made every instance open itself after an app update). This flag is the
    // exception: it says *this* wake was asked for.
    let _ = std::fs::create_dir_all(p.state_dir.join(mulpex_core::RESUMED_DIR));
    let _ = std::fs::write(mulpex_core::resumed_in_place_path(&p.state_dir, id), "");

    let claude_bin = claude::resolve().context("cannot find `claude` — run `mpx doctor`")?;
    mulpex_core::state_dir::write_state_dir(&p.state_dir, &claude::helper_path())?;
    let argv = build_argv(&claude_bin, p, &session_id, true, None, id);
    // The instance's own pane, not its window. `core::scan` skips sidebar panes, so
    // `inst.pane` is always the claude's — and a window target would resolve to
    // whichever pane is active, which from the sidebar is the sidebar.
    launch(t, p, id, "", &argv, WindowSlot::Replace(&inst.pane, &inst.window))?;
    Ok(())
}

/// Mute or unmute an instance. Nothing else changes: the `claude` keeps running,
/// keeps its inbox and stays in the peer list — mute is entirely a statement about
/// how loudly the *view* should talk about it.
///
/// Terminals produce none of the signals mute silences, so muting one would be a
/// flag with no effect. Refused rather than half-honoured.
pub fn set_muted(t: &Tmux, p: &Project, id: usize, muted: bool) -> Result<()> {
    let inst = p.find(id).with_context(|| format!("no instance #{id}"))?;
    if !inst.is_claude() {
        bail!("term#{id} is a terminal — there is nothing about it to mute");
    }
    t.set_user_option(&inst.window, true, MUTED_OPT, if muted { "1" } else { "" })
}

/// Where a window goes: a new one, or on top of an existing pane.
enum WindowSlot<'a> {
    New,
    /// The pane to relaunch into, and the window it belongs to. Both, because the
    /// pane is what `respawn-pane` must target and the window is what carries the
    /// `@mpx_*` options.
    Replace(&'a str, &'a str),
}

/// Write the spec and hand tmux the tiny launcher that `exec`s it.
fn launch(
    t: &Tmux,
    p: &Project,
    id: usize,
    name: &str,
    argv: &[String],
    slot: WindowSlot,
) -> Result<String> {
    let spec_path = p.state_dir.join("spec").join(format!("{id}.json"));
    spec::Spec {
        argv: argv.to_vec(),
        cwd: p.dir.to_string_lossy().to_string(),
        env: instance_env(p, id),
        unset: claude::UNSET.iter().map(|s| s.to_string()).collect(),
    }
    .write(&spec_path)?;

    let me = std::env::current_exe().context("locating the mpx binary")?;
    let launcher = vec![
        me.to_string_lossy().to_string(),
        "exec-spec".into(),
        spec_path.to_string_lossy().to_string(),
    ];
    let window = match slot {
        WindowSlot::New => {
            let window = t
                .new_window(&p.session, name, &p.dir, &instance_env(p, id), &launcher)
                .context("spawning the claude window")?;
            crate::layout::attach_sidebar(t, p, &window);
            window
        }
        WindowSlot::Replace(pane, window) => {
            // `respawn-pane` inherits the window's environment, which is where the
            // hub identity already is — and the spec sets it again anyway.
            t.respawn_pane(pane, &p.dir, &launcher)
                .context("restarting the claude in place")?;
            window.to_string()
        }
    };
    // Stamp the birth, here rather than at either call site, because a **restart**
    // is a birth too: the relaunched process gets its own grace period, so a claude
    // that comes back and immediately dies is a failed start and keeps its row.
    // See `core::reap_dead`.
    let _ = t.set_user_option(&window, true, core::BORN_OPT, &core::now_secs().to_string());
    Ok(window)
}

fn instance_env(p: &Project, id: usize) -> Vec<(String, String)> {
    vec![
        ("MULPEX_INSTANCE_ID".into(), id.to_string()),
        ("MULPEX_STATE_DIR".into(), p.state_dir.to_string_lossy().to_string()),
        ("MULPEX_PROJECT_DIR".into(), p.dir.to_string_lossy().to_string()),
        ("IS_SANDBOX".into(), "1".into()),
    ]
}

/// The `claude` command line. One function so a restart cannot drift from a spawn
/// — they differ in exactly two things, and both are arguments here.
fn build_argv(
    claude_bin: &Path,
    p: &Project,
    session_id: &str,
    resume: bool,
    task: Option<&SpawnTask>,
    id: usize,
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        claude_bin.to_string_lossy().to_string(),
        "--dangerously-skip-permissions".into(),
        if resume { "--resume".into() } else { "--session-id".into() },
        session_id.to_string(),
        "--settings".into(),
        p.state_dir.join("settings.json").to_string_lossy().to_string(),
        "--mcp-config".into(),
        p.state_dir.join("mcp.json").to_string_lossy().to_string(),
        // ~14 KB, and the reason the argv cannot go on the tmux command line.
        "--append-system-prompt".into(),
        mulpex_core::rules::append_system_prompt(),
    ];
    // A spawned child's task is `claude`'s POSITIONAL prompt argument. It cannot
    // truncate, needs no readiness detection, and cannot race the TUI.
    if let Some(prompt) = mulpex_core::rules::spawn_prompt(task) {
        publish_expected_prompt(&p.state_dir, id, &prompt);
        argv.push(prompt);
    }
    argv
}

/// Publish the exact prompt this child was launched with, for its own
/// `UserPromptSubmit` hook to check against what it actually received.
///
/// Delivery is argv now and cannot truncate, but the check stays: it is what turns
/// a future silent corruption into a loud one. The hook is the only thing in the
/// system that sees what `claude` really got.
fn publish_expected_prompt(state_dir: &Path, id: usize, prompt: &str) {
    let p = mulpex_core::spawn_expected_path(state_dir, id);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, prompt);
}

/// Write a spawned child's delivery verdict: `pending`, `failed` or `partial`.
/// Absent means delivered and verified — the child's own hook removes it.
pub fn mark_delivery(state_dir: &Path, id: usize, verdict: &str) {
    let p = mulpex_core::spawn_delivery_path(state_dir, id);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, verdict);
}

/// Has the delivery question been answered — by the child's hook, either way?
pub fn delivery_settled(state_dir: &Path, id: usize) -> bool {
    match std::fs::read_to_string(mulpex_core::spawn_delivery_path(state_dir, id)) {
        // Absent: the hook removed it, so the task arrived intact.
        Err(_) => true,
        // Anything the hook wrote is a verdict; only our own `pending` is not.
        Ok(s) => s.trim() != "pending",
    }
}

/// Drop both delivery files, so a spawn that never happened leaves no verdict for
/// the number behind.
pub fn clear_delivery(state_dir: &Path, id: usize) {
    let _ = std::fs::remove_file(mulpex_core::spawn_delivery_path(state_dir, id));
    let _ = std::fs::remove_file(mulpex_core::spawn_expected_path(state_dir, id));
}

/// A short sidebar label from the task a child was given, so a fanned-out batch
/// is readable rather than eight rows called `claude#N`.
pub fn name_from_task(task: &str) -> Option<String> {
    let one_line = task.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return None;
    }
    let mut s: String = one_line.chars().take(48).collect();
    if one_line.chars().count() > 48 {
        s.push('…');
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_becomes_a_short_one_line_label() {
        assert_eq!(name_from_task("  fix   the\nparser  ").as_deref(), Some("fix the parser"));
        assert_eq!(name_from_task("   "), None);
        let long = name_from_task(&"x".repeat(80)).unwrap();
        assert_eq!(long.chars().count(), 49, "48 chars plus the ellipsis");
        assert!(long.ends_with('…'));
    }

    /// The two ways a claude starts differ in exactly one flag. Sharing the
    /// builder is what stops a restart from quietly losing the hub rules or the
    /// MCP config — the failure would be a claude that looks fine and cannot
    /// coordinate.
    #[test]
    fn a_restart_resumes_where_a_spawn_starts_a_new_session() {
        let p = Project {
            session: "p".into(),
            dir: "/p".into(),
            state_dir: "/tmp/mpx-test-argv".into(),
            instances: Vec::new(),
            active_window: String::new(),
            active_pane: String::new(),
        };
        let fresh = build_argv(Path::new("/bin/claude"), &p, "uuid-1", false, None, 1);
        let again = build_argv(Path::new("/bin/claude"), &p, "uuid-1", true, None, 1);
        assert!(fresh.contains(&"--session-id".to_string()));
        assert!(again.contains(&"--resume".to_string()));
        assert!(!again.contains(&"--session-id".to_string()));
        // Everything else is identical, including the 14 KB system prompt.
        assert_eq!(fresh.len(), again.len());
        assert_eq!(fresh[4..], again[4..]);
        assert!(fresh.contains(&"--dangerously-skip-permissions".to_string()));
    }

    /// `pending` is Mulpex's own placeholder, not an answer. Treating it as one
    /// would make the watchdog stand down before the child had said anything.
    #[test]
    fn only_the_childs_own_hook_settles_the_delivery_question() {
        let d = std::env::temp_dir().join(format!("mpx-deliv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("spawning")).unwrap();
        assert!(delivery_settled(&d, 1), "absent means the hook cleared it: delivered");
        mark_delivery(&d, 1, "pending");
        assert!(!delivery_settled(&d, 1), "our own placeholder is not a verdict");
        mark_delivery(&d, 1, "partial");
        assert!(delivery_settled(&d, 1), "a verdict from the hook outranks the watchdog");
    }
}
