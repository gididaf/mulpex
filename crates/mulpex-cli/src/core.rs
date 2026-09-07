//! The per-project model — read out of tmux, not held in memory.
//!
//! **tmux is the state store.** Each window carries `@mpx_id` and `@mpx_kind`;
//! each session carries `@mpx_project` and `@mpx_state_dir`. One `list-panes -a`
//! returns every instance of every project with its liveness, in one call.
//!
//! That is what makes the daemon restartable. The desktop app's `Core` owns the
//! PTYs, so losing it loses the sessions; here the sessions belong to tmux and the
//! daemon is a bookkeeper that can die and come back knowing nothing. It also
//! removes the "adopt the existing run or wipe it" decision the plan flagged as
//! the sharpest new case: there is no separate registry to disagree with reality,
//! so a restarted daemon simply re-reads.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mulpex_core::registry;

use crate::tmux::Tmux;

/// A window we own. `kind` distinguishes a claude from a shell terminal, which
/// matters because **a terminal is never a hub peer** — excluding shells from the
/// instance list is what keeps every count downstream correct for free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub id: usize,
    pub window: String,
    /// The pane the instance itself runs in. Every window is
    /// `[ sidebar | instance ]`, so "the window" is no longer a single target:
    /// a capture or a keystroke aimed at the window could land on the sidebar.
    /// Everything that talks to the instance addresses this.
    pub pane: String,
    pub kind: String,
    pub window_name: String,
    pub dead: bool,
    pub dead_status: String,
    /// Rows that have scrolled off this pane, ever. The **delta** between two
    /// ticks is exactly what to capture into the transcript.
    pub history: u64,
    /// The pane is showing the alternate screen — a full-screen program is up, so
    /// nothing it draws will ever scroll into the history.
    pub alt: bool,
    /// The pane's foreground command, and where its shell is sitting.
    pub current_command: String,
    pub current_path: String,
    /// Muted: a statement about how loudly the *view* should talk about this row,
    /// and nothing else. The claude keeps running, keeps its inbox, and stays a
    /// peer.
    pub muted: bool,
}

impl Instance {
    pub fn is_claude(&self) -> bool {
        self.kind == "claude"
    }
    pub fn is_shell(&self) -> bool {
        self.kind == "term"
    }
}

#[derive(Debug, Clone)]
pub struct Project {
    pub session: String,
    pub dir: PathBuf,
    pub state_dir: PathBuf,
    pub instances: Vec<Instance>,
    /// Where the keyboard is in this project: the session's current window, and
    /// the active pane inside it. Empty when the scan saw neither.
    ///
    /// Collected here because the scan is already reading every pane on the
    /// server — and because these two are the **only** facts a sidebar needs that
    /// it cannot work out for itself. See `publish_focus`.
    pub active_window: String,
    pub active_pane: String,
}

impl Project {
    /// Ids the hub should consider live: claudes that are still running.
    ///
    /// A dead claude drops out here but its **window stays** — `remain-on-exit`
    /// keeps its error on screen. The two facts are separate on purpose: the hub
    /// must stop routing to it, the user must still be able to read why it died.
    pub fn live_ids(&self) -> Vec<usize> {
        let mut ids: Vec<usize> = self
            .instances
            .iter()
            .filter(|i| i.is_claude() && !i.dead)
            .map(|i| i.id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// The next free instance number. Numbers are an **identity, not a position**:
    /// a closed instance's number is not reused, so `claude#3` always means the
    /// same conversation and gaps are expected.
    pub fn next_id(&self) -> usize {
        self.instances.iter().map(|i| i.id).max().unwrap_or(0) + 1
    }

    pub fn find(&self, id: usize) -> Option<&Instance> {
        self.instances.iter().find(|i| i.id == id)
    }
}

/// Field separator for the tmux format string. Unit Separator, because a window
/// name is arbitrary text (it carries whatever `hub_set_name` was given) and a
/// tab in it would silently shift every field after it.
const SEP: char = '\u{1f}';

/// Whether a tmux session is a Mulpex project the daemon may act on.
///
/// Both tags, not just one. They are written by two separate tmux calls with a
/// 200 ms poll running against them, so a session is briefly half-tagged — and a
/// missing state dir is not an error anywhere downstream, because
/// `PathBuf::from("")` joins to a **relative** path. That tick then writes
/// `instances` and `terminals/` into whatever directory the daemon was started in.
/// Observed doing exactly that in this repo's root (2026-09-06). `up()` also
/// writes `@mpx_project` last so the gate closes on its own; this is the belt to
/// that pair of braces, and covers a session tagged by an older build.
fn is_ours(project: &str, state_dir: &str) -> bool {
    !project.is_empty() && !state_dir.is_empty()
}

/// One `list-panes -a` for the whole workspace.
pub fn scan(t: &Tmux) -> Result<Vec<Project>> {
    let fmt = format!(
        "#{{session_name}}{SEP}#{{@mpx_project}}{SEP}#{{@mpx_state_dir}}{SEP}\
         #{{window_id}}{SEP}#{{@mpx_id}}{SEP}#{{@mpx_kind}}{SEP}#{{window_name}}{SEP}\
         #{{pane_dead}}{SEP}#{{pane_dead_status}}{SEP}#{{history_size}}{SEP}\
         #{{alternate_on}}{SEP}#{{pane_current_command}}{SEP}#{{pane_current_path}}{SEP}\
         #{{@mpx_muted}}{SEP}#{{pane_id}}{SEP}#{{@mpx_side}}{SEP}\
         #{{window_active}}{SEP}#{{pane_active}}"
    );
    let lines = match t.list_panes_all(&fmt) {
        Ok(l) => l,
        // No server running is the normal empty state, not an error.
        Err(_) => return Ok(Vec::new()),
    };

    let mut by_session: BTreeMap<String, Project> = BTreeMap::new();
    for line in lines {
        let f: Vec<&str> = line.split(SEP).collect();
        if f.len() < 18 {
            continue;
        }
        let (session, proj, sdir, win, id, kind, name, dead, status) =
            (f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7], f[8]);
        // Not one of ours — or not one of ours *yet*. A session is tagged by two
        // separate tmux calls, so a poll can land between them and see a project
        // with no state dir. `PathBuf::from("")` is not an error: it joins to a
        // **relative** path, so that tick writes `instances` and `terminals/` into
        // whatever directory the daemon happens to be running in. Observed doing
        // exactly that in this repo's root (2026-09-06). An absent state dir is
        // ignorance, not a location.
        if !is_ours(proj, sdir) {
            continue;
        }
        let entry = by_session.entry(session.to_string()).or_insert_with(|| Project {
            session: session.to_string(),
            dir: PathBuf::from(proj),
            state_dir: PathBuf::from(sdir),
            instances: Vec::new(),
            active_window: String::new(),
            active_pane: String::new(),
        });
        // Recorded **before** the sidebar skip below, because the active pane very
        // often *is* a sidebar — and a sidebar asking "do I have the keyboard?"
        // is the one caller that needs the answer.
        if f[16] == "1" {
            entry.active_window = win.to_string();
            if f[17] == "1" {
                entry.active_pane = f[14].to_string();
            }
        }
        // A window with no `@mpx_id` is one the user made with tmux's own
        // `new-window` binding. It is a real shell they are using, so it is left
        // strictly alone: not adopted, not renamed, not reaped.
        // The sidebar is a pane in the instance's own window and inherits its
        // window options, so without this every instance is listed twice — once
        // for itself and once for the strip that draws it.
        if f[15] == "1" {
            continue;
        }
        let Ok(id) = id.parse::<usize>() else { continue };
        entry.instances.push(Instance {
            id,
            window: win.to_string(),
            kind: if kind.is_empty() { "claude".into() } else { kind.to_string() },
            window_name: name.to_string(),
            dead: dead == "1",
            dead_status: status.to_string(),
            history: f[9].parse().unwrap_or(0),
            alt: f[10] == "1",
            current_command: f[11].to_string(),
            current_path: f[12].to_string(),
            muted: f[13] == "1",
            pane: f[14].to_string(),
        });
    }
    for p in by_session.values_mut() {
        p.instances.sort_by_key(|i| i.id);
    }
    Ok(by_session.into_values().collect())
}

/// Mark a window whose process has exited, once.
///
/// The rename is the only signal a detached user gets that something died, so it
/// has to be visible in the window bar — but it must be idempotent, or the poll
/// loop renames the same window forever (the failure mode `reap_dead`'s latching
/// failure mark exists to prevent in the desktop app).
pub fn mark_dead(t: &Tmux, inst: &Instance) -> Result<bool> {
    if inst.window_name.starts_with('✗') {
        return Ok(false);
    }
    let status = if inst.dead_status.is_empty() {
        String::new()
    } else {
        format!(" (exit {})", inst.dead_status)
    };
    t.rename_window(&inst.window, &format!("✗ {}{}", inst.window_name, status))?;
    Ok(true)
}

/// How long a claude must survive to count as having started. Ported from the
/// desktop app's `EARLY_DEATH_GRACE`, and the number matters less than the fact
/// that there is one.
const EARLY_DEATH_GRACE: u64 = 10;

/// Set when a window is created, read only when its process has died.
pub const BORN_OPT: &str = "@mpx_born";

pub enum Reaped {
    /// Nothing to do — already handled on an earlier tick.
    Nothing,
    /// Kept and labelled `✗`, because the row itself is the only record of why.
    Marked,
    /// The window is gone.
    Removed,
}

/// Decide what a dead instance's row is for, and act on it.
///
/// The whole policy in one place, ported from `Workspace::reap_dead`:
///
/// - **A terminal is kept.** Its output is the reason it existed, and a shell that
///   exits has usually just printed the thing you wanted to read.
/// - **A claude that died young AND died badly is kept**, marked `✗` with its exit
///   code. It never started, and this row is the only place the reason is ever shown
///   — a spawn that fails and vanishes shows you nothing at all.
/// - **Anything else is removed.** You exited it; the row has done its job.
///
/// The `✗` prefix **latches** the decision. Without that, a row kept for dying at
/// 2s would become removable the moment `EARLY_DEATH_GRACE` elapsed and be silently
/// reaped ten seconds later — the same bug, delayed. The desktop hit exactly this.
pub fn reap_dead(t: &Tmux, inst: &Instance) -> Result<Reaped> {
    if inst.window_name.starts_with('✗') {
        return Ok(Reaped::Nothing); // decided on an earlier tick
    }
    if keeps_its_row(inst.is_shell(), &inst.dead_status, age_secs(t, inst)) {
        mark_dead(t, inst)?;
        return Ok(Reaped::Marked);
    }
    t.kill_window(&inst.window)?;
    Ok(Reaped::Removed)
}

/// The policy itself, with no tmux in it so it can be tested.
///
/// `age` is seconds since the window was created, or `None` when that is unknown.
///
/// **The exit status is what makes this honest.** Age alone said "died before it
/// started" about a claude the user had just quit — with `exit 0` on the same row,
/// in a project whose only other content was the black rectangle a claude leaves
/// when it drops the alternate screen. Nothing was wrong, nothing was shown, and
/// the row that exists to explain a failure stayed on screen explaining nothing.
/// `#{pane_dead_status}` gives the code for free and `Instance` already carries it.
///
/// Both halves are needed. Age alone keeps a clean quit; status alone keeps an
/// hour-old claude you interrupted (SIGINT exits 130). A row survives only when it
/// is the sole record of something that actually went wrong.
fn keeps_its_row(is_shell: bool, dead_status: &str, age: Option<u64>) -> bool {
    if is_shell {
        return true;
    }
    // A clean exit is never a failed start, however soon it came — you asked for it.
    if dead_status == "0" {
        return false;
    }
    match age {
        // No stamp (a window from an older build): assume the worse case and keep
        // it. Losing an error silently is the expensive direction.
        None => true,
        Some(age) => age <= EARLY_DEATH_GRACE,
    }
}

/// Seconds since the window was created, read off its birth stamp. `None` when the
/// stamp is missing or unreadable — a window from an older build.
///
/// Only ever asked about a dead instance, so the extra tmux call costs nothing in
/// the steady state — which is why this is not a field on the scan every tick pays
/// for.
fn age_secs(t: &Tmux, inst: &Instance) -> Option<u64> {
    let born: u64 = t.user_option(&inst.window, BORN_OPT).parse().ok()?;
    Some(now_secs().saturating_sub(born))
}

/// The instance id in a daemon reply like `claude#3` or `term#4`.
///
/// The reply is already the daemon's own account of what it made, so reading the
/// id back out of it beats re-deriving one — two answers to "which instance was
/// just created" is one more than there should be.
pub fn id_in_reply(reply: &str) -> Option<usize> {
    let digits: String = reply
        .rsplit('#')
        .next()?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Apply any `hub_set_name` requests: rename the tmux window and write the flag
/// the naming nudge gates on.
///
/// `named/<id>` is a two-process contract — `hook.rs::instance_named` reads it,
/// and until it exists the hook keeps asking the instance to name itself. Writing
/// the flag is therefore not bookkeeping; it is what stops the nudge.
pub fn process_name_requests(t: &Tmux, p: &Project) -> Result<bool> {
    let dir = p.state_dir.join(mulpex_core::NAMEREQ_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(false);
    };
    let mut changed = false;
    for e in entries.flatten() {
        let path = e.path();
        let id = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<usize>().ok());
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        let (Some(id), Some(inst)) = (id, id.and_then(|i| p.find(i))) else { continue };
        let name = sanitize_label(&raw);
        if name.is_empty() {
            continue;
        }
        t.rename_window(&inst.window, &format!("{}#{} ▸ {}", inst.kind, id, name))?;
        let _ = std::fs::write(mulpex_core::named_flag_path(&p.state_dir, id), &name);
        changed = true;
    }
    Ok(changed)
}

/// A window name is arbitrary text from `hub_set_name`. Control characters would
/// corrupt the `list-panes` parse (SEP above) and the status line; a very long
/// name would push every other window off the bar.
pub fn sanitize_label(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(40).collect()
}

/// Publish this project to `registry.json`, which is what makes cross-project
/// `<project>#<n>` addressing resolve.
pub fn registry_entry(p: &Project, handle: u64) -> registry::ProjectEntry {
    let instances = p
        .live_ids()
        .into_iter()
        .map(|id| registry::InstanceEntry {
            id,
            status: read_status(&p.state_dir, id),
            task: std::fs::read_to_string(p.state_dir.join("tasks").join(id.to_string()))
                .unwrap_or_default()
                .trim()
                .to_string(),
            name: p.find(id).and_then(|i| label_of(&i.window_name)),
        })
        .collect();
    registry::ProjectEntry {
        handle,
        name: p
            .dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        dir: p.dir.to_string_lossy().into_owned(),
        state_dir: p.state_dir.to_string_lossy().into_owned(),
        instances,
    }
}

/// The label half of `claude#3 ▸ fixing the parser`, if it has one.
///
/// `mark_dead` decorates the whole window name (`✗ term#2 ▸ probe (exit 7)`)
/// because that name *is* the window bar. The label is a different thing — it is
/// what the sidebar and `hub_instances` show, and what `hub_terminal_name` refuses
/// to overwrite — so the decoration has to come back off here. Without this a
/// terminal's name silently became `probe (exit 7)` the moment its shell exited.
pub fn label_of(window_name: &str) -> Option<String> {
    let name = window_name.trim_start_matches('✗').trim_start();
    let (_, label) = name.split_once(" ▸ ")?;
    let label = match label.rsplit_once(" (exit ") {
        Some((head, tail)) if tail.ends_with(')') => head,
        _ => label,
    };
    let label = label.trim();
    (!label.is_empty()).then(|| label.to_string())
}

/// One-word status, straight off disk.
///
/// A **missing** file is not idleness: `mcp::status_of` returns `waiting` for it,
/// which reports ignorance in the same word it reports being idle. Here a claude
/// that has never taken a turn reads `starting`, because we know the difference.
fn read_status(state_dir: &Path, id: usize) -> String {
    match std::fs::read_to_string(state_dir.join(id.to_string())) {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => "starting".to_string(),
    }
}

/// Write `<state_dir>/instances` so the MCP server lists exactly what is running.
pub fn publish_instances(p: &Project) -> Result<()> {
    mulpex_core::state_dir::write_live_instances(&p.state_dir, &p.live_ids())
        .with_context(|| format!("writing instances for {}", p.session))
}

/// The file the daemon publishes "where the keyboard is" into, for the sidebars.
pub const FOCUS_FILE: &str = "focus";

/// Write `<active window> <active pane>` for this project.
///
/// **Why a file and not a tmux query.** Every window carries its own sidebar, and
/// each one has to know whether it is the one on screen. Asking tmux costs a
/// process: measured at 10 ms wall and ~3.4 ms of CPU per `display-message`, so N
/// sidebars polling fast enough to feel instant burn real CPU forever — seven
/// hidden ones at 150 ms is about a sixth of a core, spent discovering that
/// nothing has changed. Sleeping longer instead is what made a new claude's
/// sidebar sit blank for 679 ms and a closed row linger for 1.3 s (measured
/// 2026-09-06).
///
/// The daemon already asks tmux this, every tick, for every project. Publishing
/// the answer turns N polls into one, and turns each sidebar's check into a file
/// read — microseconds, so it can afford to look often.
///
/// Best effort: a sidebar that finds no file falls back to asking tmux itself, so
/// a daemon that is down costs latency and nothing else.
pub fn publish_focus(p: &Project) {
    if p.state_dir.as_os_str().is_empty() {
        return; // see `is_ours` — an absent state dir is ignorance, not a location
    }
    let path = p.state_dir.join(FOCUS_FILE);
    let body = format!("{} {}", p.active_window, p.active_pane);
    // Only when it changed. This runs five times a second per project, and a
    // rewrite every tick would be a pointless write to disk forever.
    if std::fs::read_to_string(&path).ok().as_deref() == Some(body.as_str()) {
        return;
    }
    let _ = std::fs::write(path, body);
}

/// Read back what `publish_focus` wrote: `(active window, active pane)`.
///
/// `None` means the daemon has not published for this project — not that nothing
/// is focused. The caller falls back to asking tmux.
pub fn read_focus(state_dir: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(state_dir.join(FOCUS_FILE)).ok()?;
    let mut parts = text.split(' ');
    let window = parts.next()?.trim().to_string();
    let pane = parts.next().unwrap_or("").trim().to_string();
    if window.is_empty() {
        return None;
    }
    Some((window, pane))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row survives only when it is the sole record of something that went
    /// wrong. Observed 2026-09-06: a claude the user quit seconds after opening a
    /// project was kept and logged as "died before it started", with `exit 0` on
    /// the same row — and the pane it preserved was blank, because a claude drops
    /// the alternate screen on the way out. The row that exists to explain a
    /// failure sat there explaining nothing.
    #[test]
    fn a_row_is_kept_only_for_something_that_actually_went_wrong() {
        // The bug: a clean exit is not a failed start, however soon it came.
        assert!(!keeps_its_row(false, "0", Some(2)), "exit 0 at 2s is a quit, not a crash");
        assert!(!keeps_its_row(false, "0", Some(9999)));

        // Still kept: died young AND died badly. This is the whole point of the row
        // — a spawn that fails and vanishes shows you nothing at all.
        assert!(keeps_its_row(false, "1", Some(2)));
        assert!(keeps_its_row(false, "127", Some(0)), "exec failure");

        // Status alone is not enough either: SIGINT on an hour-old claude exits
        // 130, and you meant that.
        assert!(!keeps_its_row(false, "130", Some(3600)));

        // A terminal is always kept: its output is the reason it existed.
        assert!(keeps_its_row(true, "0", Some(3600)));

        // No birth stamp — a window from an older build. Keep it: losing an error
        // silently is the expensive direction.
        assert!(keeps_its_row(false, "", None));
        assert!(!keeps_its_row(false, "0", None), "...but exit 0 still is not one");
    }

    /// Every window runs a sidebar and each must know whether it is the one on
    /// screen. Asking tmux costs a process (~3.4 ms of CPU, measured), so the
    /// daemon — which already asks, once, for every project — publishes the answer
    /// and they read it. A missing file is ignorance, not "nothing is focused":
    /// the caller has to be able to tell those apart and fall back.
    #[test]
    fn focus_survives_the_round_trip_and_absence_is_not_an_answer() {
        let dir = std::env::temp_dir().join(format!("mpx-focus-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert!(read_focus(&dir).is_none(), "no file yet: ask tmux instead");

        let mut p = proj(Vec::new());
        p.state_dir = dir.clone();
        p.active_window = "@7".into();
        p.active_pane = "%12".into();
        publish_focus(&p);
        assert_eq!(read_focus(&dir), Some(("@7".into(), "%12".into())));

        // The keyboard is in the window but on a pane the scan skipped — the
        // window is still on screen and the sidebar still has to repaint.
        p.active_pane = String::new();
        publish_focus(&p);
        assert_eq!(read_focus(&dir), Some(("@7".into(), String::new())));

        // Nothing active at all is indistinguishable from not knowing, so it must
        // not be published as an answer.
        p.active_window = String::new();
        publish_focus(&p);
        assert!(read_focus(&dir).is_none());

        // An empty state dir joins to a *relative* path — the bug that once wrote
        // files into the repo root. It must write nothing at all.
        let mut nowhere = proj(Vec::new());
        nowhere.state_dir = PathBuf::new();
        nowhere.active_window = "@1".into();
        publish_focus(&nowhere);
        assert!(!Path::new(FOCUS_FILE).exists(), "wrote into the working directory");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The daemon's reply is the only account of what it just made, so reading the
    /// id back out of it is what lets a key press land in the right instance.
    #[test]
    fn an_id_is_read_back_out_of_the_reply() {
        assert_eq!(id_in_reply("claude#3"), Some(3));
        assert_eq!(id_in_reply("term#12"), Some(12));
        assert_eq!(id_in_reply("closed #2"), Some(2));
        assert_eq!(id_in_reply("restarted claude#7 (resuming its conversation)"), Some(7));
        assert_eq!(id_in_reply("ERR no claude"), None);
        assert_eq!(id_in_reply(""), None);
        assert_eq!(id_in_reply("claude#"), None);
    }

    /// A half-tagged session must not be adopted. An empty `@mpx_state_dir` is not
    /// a location and not an error — it is a **relative** path, which is how a
    /// mid-creation poll came to write `instances` and `terminals/` into the
    /// daemon's own working directory.
    #[test]
    fn a_session_is_ours_only_once_both_tags_are_written() {
        assert!(is_ours("/p", "/run/p"));
        assert!(!is_ours("/p", ""), "tagged mid-creation: project set, state dir not yet");
        assert!(!is_ours("", "/run/p"), "not a Mulpex session at all");
        assert!(!is_ours("", ""));
        // The failure this guards is that the empty one is silently usable.
        assert_eq!(PathBuf::from("").join("terminals"), PathBuf::from("terminals"));
    }

    fn inst(id: usize, kind: &str, dead: bool) -> Instance {
        Instance {
            id,
            window: format!("@{id}"),
            pane: format!("%{id}"),
            kind: kind.into(),
            window_name: format!("{kind}#{id}"),
            dead,
            dead_status: String::new(),
            history: 0,
            alt: false,
            current_command: "zsh".into(),
            current_path: "/p".into(),
            muted: false,
        }
    }
    fn proj(instances: Vec<Instance>) -> Project {
        Project {
            session: "p".into(),
            dir: "/p".into(),
            state_dir: "/tmp/p".into(),
            instances,
            active_window: String::new(),
            active_pane: String::new(),
        }
    }

    /// A terminal is never a hub peer, and a dead claude is not one either — but
    /// for different reasons, so both exclusions are tested.
    #[test]
    fn live_ids_exclude_terminals_and_dead_claudes() {
        let p = proj(vec![
            inst(1, "claude", false),
            inst(2, "term", false),
            inst(3, "claude", true),
            inst(4, "claude", false),
        ]);
        assert_eq!(p.live_ids(), vec![1, 4]);
    }

    /// Numbers are identity, not position: closing claude#2 must not make the
    /// next spawn claude#2 again, or two conversations share an address.
    #[test]
    fn instance_numbers_are_never_reused() {
        let p = proj(vec![inst(1, "claude", false), inst(3, "claude", true)]);
        assert_eq!(p.next_id(), 4, "gaps stay; the max is what advances");
    }

    #[test]
    fn a_label_is_extracted_from_a_named_window() {
        assert_eq!(label_of("claude#3 ▸ fixing the parser").as_deref(), Some("fixing the parser"));
        assert_eq!(label_of("claude#3"), None);
    }

    /// A window that died is renamed in place, because its name is the window
    /// bar. The label must not absorb that: a terminal called "probe" became
    /// "probe (exit 7)" in the sidebar and in `hub_instances`.
    #[test]
    fn a_dead_windows_decoration_is_not_part_of_its_label() {
        assert_eq!(label_of("✗ term#2 ▸ probe (exit 7)").as_deref(), Some("probe"));
        assert_eq!(label_of("✗ claude#1 ▸ the parser").as_deref(), Some("the parser"));
        assert_eq!(label_of("✗ term#2"), None);
        // A name that genuinely ends that way is left alone — it has no `)`.
        assert_eq!(label_of("term#2 ▸ check (exit codes").as_deref(), Some("check (exit codes"));
    }

    /// The name comes from `hub_set_name`, i.e. arbitrary model text. A newline
    /// in it would break the `list-panes` field split.
    #[test]
    fn a_label_is_stripped_of_control_characters_and_capped() {
        assert_eq!(sanitize_label("fix\tthe\nparser  now"), "fix the parser now");
        assert_eq!(sanitize_label(&"x".repeat(80)).chars().count(), 40);
        assert_eq!(sanitize_label("   "), "");
    }

    /// Renaming a dead window must happen once, not on every 200 ms tick.
    #[test]
    fn marking_a_dead_window_is_idempotent_by_name() {
        let already = Instance { window_name: "✗ claude#1 (exit 3)".into(), ..inst(1, "claude", true) };
        assert!(already.window_name.starts_with('✗'), "guard is the name prefix");
    }
}
