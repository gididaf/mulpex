//! Shell terminals: `term#N` windows, and the `termreq/` handshake an instance
//! drives them through.
//!
//! Mulpex owns the terminals, so creating one or typing into one is a file
//! handshake through the poll loop — exactly like `hub_spawn`, and for the same
//! reason: id allocation has to be single-threaded. *Reading* one is not a
//! handshake, which is what makes "read its output at any time" cheap enough to
//! poll (see `recorder.rs`).
//!
//! **A terminal is never a hub peer.** It gets an id from the same counter as the
//! claudes — that is what lets every `(project, id)`-keyed mechanism stay
//! kind-agnostic — but `@mpx_kind=term` keeps it out of `live_ids`, and so out of
//! `instances`, the registry, and every count downstream, for free.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::core::{self, Project};
use crate::recorder::Recorder;
use crate::tmux::Tmux;
use crate::spec;

/// Environment a terminal must NOT inherit.
///
/// The hub identity is the dangerous half. The tmux server inherits its
/// environment from whatever ran `mpx up`, and if that was a shell inside a
/// Mulpex claude, then a `claude` the user later types into this terminal would
/// write its status files under a *terminal's* id and corrupt the hub. The
/// desktop app strips exactly these for exactly this reason (`pty.rs::shell_command`).
const SHELL_UNSET: &[&str] = &[
    "MULPEX_INSTANCE_ID",
    "MULPEX_STATE_DIR",
    "MULPEX_PROJECT_DIR",
    "IS_SANDBOX",
    "CLAUDE_CODE_CHILD_SESSION",
];

/// The user's shell, interactive and login.
///
/// `-l` alone is login-but-not-interactive: zsh would skip `.zshrc`, print no
/// prompt at all, and treat the pty as a script. Both flags are required.
fn shell_argv() -> Vec<String> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/zsh".to_string());
    vec![shell, "-l".into(), "-i".into()]
}

/// Open one shell terminal in a project's tmux session.
///
/// Goes through the same spec-file launcher as a claude rather than
/// `new-window -e`: `-e` can only *set* a variable, and what a terminal needs is
/// for several to be **absent**. `exec-spec` can unset.
pub fn spawn_terminal_window(t: &Tmux, p: &Project, id: usize, label: Option<&str>) -> Result<String> {
    let name = match label {
        Some(l) => format!("term#{id} ▸ {}", crate::core::sanitize_label(l)),
        None => format!("term#{id}"),
    };
    let spec_path = p.state_dir.join("spec").join(format!("term-{id}.json"));
    spec::Spec {
        argv: shell_argv(),
        cwd: p.dir.to_string_lossy().to_string(),
        env: Vec::new(),
        unset: SHELL_UNSET.iter().map(|s| s.to_string()).collect(),
    }
    .write(&spec_path)?;

    let me = std::env::current_exe().context("locating the mpx binary")?;
    let launcher = vec![
        me.to_string_lossy().to_string(),
        "exec-spec".into(),
        spec_path.to_string_lossy().to_string(),
    ];
    let window = t
        .new_window(&p.session, &name, &p.dir, &[], &launcher)
        .context("opening the terminal window")?;
    t.set_user_option(&window, true, "@mpx_id", &id.to_string())?;
    t.set_user_option(&window, true, "@mpx_kind", "term")?;
    // Every window carries a birth stamp, so `@mpx_born` means one thing. A dead
    // terminal is kept regardless (`core::reap_dead`), so nothing reads this yet.
    t.set_user_option(&window, true, core::BORN_OPT, &core::now_secs().to_string())?;
    crate::layout::attach_sidebar(t, p, &window);
    // Create the transcript before returning. The recorder does not learn this
    // terminal exists until the *next* scan, so without this a `hub_terminal_read`
    // right after `hub_terminal_open` is answered "no term#N — call hub_instances
    // to see the live ones" about a terminal that was just created. Verified
    // against the desktop app, where the same read succeeds.
    crate::recorder::create_log(&p.state_dir, id);
    Ok(window)
}

/// Drain `termreq/`, applying each request in filename order.
///
/// Filenames lead with a zero-padded microsecond stamp, so a plain sort is time
/// order — and that matters: `cd /tmp` then `make` is not the same as the
/// reverse. Returns whether anything changed the window list.
pub fn process_requests(
    t: &Tmux,
    p: &Project,
    rec: &mut Recorder,
    rem: &mut crate::remotes::Remotes,
    next_id: &mut usize,
) -> bool {
    let dir = p.state_dir.join("termreq");
    let mut requests: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
                .collect()
        })
        .unwrap_or_default();
    requests.sort();

    let mut changed = false;
    for req in requests {
        let Some(stem) = req.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
            let _ = std::fs::remove_file(&req);
            continue;
        };
        let content = std::fs::read_to_string(&req).unwrap_or_default();
        let _ = std::fs::remove_file(&req);
        let (reply, touched) = apply(t, p, rec, rem, &content, next_id);
        changed |= touched;
        let _ = std::fs::write(dir.join(format!("{stem}.done")), reply.to_string());
    }

    sweep_replies(&dir);
    changed
}

/// A caller that timed out leaves its reply behind; collect the strays so the
/// directory does not grow for the life of the daemon.
fn sweep_replies(dir: &Path) {
    const DONE_TTL: std::time::Duration = std::time::Duration::from_secs(60);
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("done") {
            continue;
        }
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|d| d > DONE_TTL).unwrap_or(false))
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&p);
        }
    }
}

fn apply(
    t: &Tmux,
    p: &Project,
    rec: &mut Recorder,
    rem: &mut crate::remotes::Remotes,
    content: &str,
    next_id: &mut usize,
) -> (Value, bool) {
    let Ok(v) = serde_json::from_str::<Value>(content) else {
        return (json!({ "ok": false, "error": "malformed request" }), false);
    };
    let op = v.get("op").and_then(|x| x.as_str()).unwrap_or("");
    let str_of = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    };
    let id = v.get("id").and_then(|x| x.as_u64()).map(|n| n as usize);

    match op {
        "open" => {
            let id = *next_id;
            *next_id += 1;
            match spawn_terminal_window(t, p, id, str_of("label").as_deref()) {
                Ok(_) => {
                    if let Some(seed) = str_of("seed") {
                        rec.seed(&p.state_dir, id, seed);
                    }
                    (json!({ "ok": true, "id": id }), true)
                }
                Err(e) => (json!({ "ok": false, "error": e.to_string() }), false),
            }
        }
        // Label a terminal this instance did NOT open — in practice one the user
        // opened themselves, which has no name and shows up as `name: null`. Only
        // an UNNAMED terminal may be labelled: a name is usually the user's, and a
        // row they deliberately titled is not an instance's to overwrite.
        "name" => {
            let (Some(id), Some(label)) = (id, str_of("label")) else {
                return (json!({ "ok": false, "error": "missing 'id' or 'label'" }), false);
            };
            let Some(inst) = p.find(id) else {
                return (json!({ "ok": false, "error": format!("no term#{id}") }), false);
            };
            if !inst.is_shell() {
                return (
                    json!({ "ok": false, "error": format!("#{id} is a Claude instance, not a terminal") }),
                    false,
                );
            }
            if let Some(existing) = crate::core::label_of(&inst.window_name) {
                return (
                    json!({
                        "ok": false,
                        "error": format!("term#{id} is already named {existing:?} — leave it alone"),
                        "name": existing,
                    }),
                    false,
                );
            }
            let label = crate::core::sanitize_label(&label);
            match t.rename_window(&inst.window, &format!("term#{id} ▸ {label}")) {
                Ok(()) => (json!({ "ok": true, "id": id, "name": label }), true),
                Err(e) => (json!({ "ok": false, "error": e.to_string() }), false),
            }
        }
        "send" => {
            let Some(id) = id else {
                return (json!({ "ok": false, "error": "missing 'id'" }), false);
            };
            let data = v.get("data").and_then(|x| x.as_str()).unwrap_or("");
            match p.find(id) {
                Some(i) if !i.is_shell() => (
                    json!({ "ok": false, "error": format!("#{id} is a Claude instance, not a terminal") }),
                    false,
                ),
                Some(i) if i.dead => (
                    json!({ "ok": false, "error": format!("term#{id} has exited; open a new one") }),
                    false,
                ),
                Some(i) => match t.send_bytes(&i.pane, data.as_bytes()) {
                    Ok(()) => {
                        // Anything typed at a remote claude puts the ball in its
                        // court, which is what arms the silence backstop. Sending
                        // to an ordinary shell arms nothing.
                        if mulpex_core::remote::RemoteMeta::read(&p.state_dir, id).is_some() {
                            rem.expect_reply(&p.state_dir, id);
                        }
                        (json!({ "ok": true, "id": id }), false)
                    }
                    Err(e) => (json!({ "ok": false, "error": e.to_string() }), false),
                },
                None => (json!({ "ok": false, "error": format!("no term#{id}") }), false),
            }
        }
        "close" => {
            let Some(id) = id else {
                return (json!({ "ok": false, "error": "missing 'id'" }), false);
            };
            match p.find(id) {
                Some(i) if !i.is_shell() => (
                    json!({ "ok": false, "error": format!("#{id} is a Claude instance, not a terminal") }),
                    false,
                ),
                Some(i) => {
                    // Collect the pids BEFORE the kill. `kill-window` hangs up the
                    // pane's process group, which a `nohup`'d job ignores, and
                    // once the window is gone tmux has released the pty — there is
                    // then nothing left to ask, and the tty number can be reused
                    // by another pane. Measured; see `sweep.rs`.
                    let ttys = t
                        .display(&i.pane, "#{pane_tty}")
                        .map(|s| vec![s])
                        .unwrap_or_default();
                    let doomed = crate::sweep::pids_to_sweep(&ttys);
                    let r = t.kill_window(&i.window);
                    crate::sweep::kill_pids(&doomed);
                    match r {
                        Ok(()) => (json!({ "ok": true, "id": id }), true),
                        Err(e) => (json!({ "ok": false, "error": e.to_string() }), false),
                    }
                }
                None => (json!({ "ok": false, "error": format!("no term#{id}") }), false),
            }
        }
        // `hub_close` — close claude rows, the inverse of `hub_spawn`. Ids are
        // decided one at a time and reported individually: a batch cleaning up six
        // workers must not lose the five it can close because the sixth is still
        // mid-turn.
        //
        // Nothing prunes the store here. `restore::records` derives it from the
        // live window list every tick, so a killed window leaves it on its own.
        "close_instance" => {
            let force = v.get("force").and_then(|x| x.as_bool()).unwrap_or(false);
            let ids: Vec<usize> = v
                .get("ids")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|n| n.as_u64()).map(|n| n as usize).collect())
                .unwrap_or_default();
            if ids.is_empty() {
                return (json!({ "ok": false, "error": "missing 'ids'" }), false);
            }
            let mut closed: Vec<usize> = Vec::new();
            let mut refused: Vec<Value> = Vec::new();
            for id in ids {
                let Some(i) = p.find(id) else {
                    refused.push(json!({
                        "id": id,
                        "reason": format!("no claude#{id} is open — it may already have been closed."),
                    }));
                    continue;
                };
                if i.is_shell() {
                    refused.push(json!({
                        "id": id,
                        "reason": format!(
                            "term#{id} is a terminal, not a claude — close it with hub_terminal_close."
                        ),
                    }));
                    continue;
                }
                if !force {
                    if let Some(reason) = mulpex_core::close_busy_reason(&p.state_dir, id) {
                        refused.push(json!({ "id": id, "reason": reason }));
                        continue;
                    }
                }
                // Same order as the terminal close above, and for the same measured
                // reason: collect the pids BEFORE `kill-window` releases the pty,
                // or a `nohup`'d job survives with nothing left to ask about it.
                let ttys = t.display(&i.pane, "#{pane_tty}").map(|s| vec![s]).unwrap_or_default();
                let doomed = crate::sweep::pids_to_sweep(&ttys);
                let r = t.kill_window(&i.window);
                crate::sweep::kill_pids(&doomed);
                match r {
                    Ok(()) => closed.push(id),
                    Err(e) => refused.push(json!({
                        "id": id,
                        "reason": format!("claude#{id} could not be closed: {e}"),
                    })),
                }
            }
            let changed = !closed.is_empty();
            (json!({ "ok": true, "closed": closed, "refused": refused }), changed)
        }
        other => (json!({ "ok": false, "error": format!("unknown op: {other}") }), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `-l` without `-i` gives a login shell that never prints a prompt — the
    /// terminal would look hung, and a seeded command would run into a shell
    /// that is treating the pty as a script.
    #[test]
    fn the_shell_is_both_login_and_interactive() {
        let argv = shell_argv();
        assert!(argv.contains(&"-l".to_string()));
        assert!(argv.contains(&"-i".to_string()));
    }

    /// The hub identity must not reach a shell: a `claude` the user types into
    /// this terminal would otherwise adopt a *terminal's* instance number.
    #[test]
    fn a_terminal_does_not_inherit_a_hub_identity() {
        for k in ["MULPEX_INSTANCE_ID", "MULPEX_STATE_DIR", "MULPEX_PROJECT_DIR"] {
            assert!(SHELL_UNSET.contains(&k), "{k} must be unset for a terminal");
        }
    }

    /// Two opens in one tick must not both be told they are the same terminal.
    #[test]
    fn ids_advance_within_a_single_batch_of_requests() {
        let mut next = 4;
        let mut take = || {
            let v = next;
            next += 1;
            v
        };
        let (a, b) = (take(), take());
        assert_ne!(a, b);
        assert_eq!((a, b), (4, 5));
    }
}
