//! Waking an idle codex when hub mail arrives (docs/codex.md, "Wake").
//!
//! A claude is woken by its own listener — a plugin monitor Claude Code runs
//! for it. Codex has no monitors, so the APP rings a codex whose inbox grew
//! while it sat `waiting`:
//!
//! 1. With a thread id, `codex queue --thread <id> --message "mulpex: N new hub
//!    message(s)"` — a declared CLI that starts a turn in the idle TUI and
//!    never touches a draft in its box (measured, S12). Not typing into a TUI.
//! 2. Before Codex has minted a thread id (no prompt yet, S10) there is nothing
//!    to queue to, and only then the frontend types the line into the box — if
//!    the box is empty. It answers with `wake_typed`; a refusal is recorded.
//!
//! **A ring counts only when the codex acks it**: its own `UserPromptSubmit`
//! hook touches `wake/<id>.ack` when the prompt is a wake line. `codex queue`
//! exits 0 even with no TUI on the thread, so its exit code proves nothing.
//! An unacked ring is `unanswered`, retried every [`RETRY`], and shown on the
//! row (`StatusEntry.wake`) — the reverted doorbell's failure was a ring that
//! silently did not happen, so here every attempt and outcome is visible, and
//! logged one line each in `wake/<id>.log`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

use crate::snapshot::WakeMark;

/// How long `codex queue` may run before it is killed and the ring counted as
/// failed. It answers in ~0.1 s (measured).
pub const QUEUE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long after a ring the codex's ack may take before the row shows it
/// `unanswered`. A queued message starts a turn in an idle TUI and the hook
/// fires before the model runs — but a codex resumed seconds earlier took
/// 8.2–8.5 s (measured, codex-cli 0.149.1), so 8 s flashed a false warning.
/// An ack that lands even later still turns the mark `delivered`.
pub const ACK_WAIT: Duration = Duration::from_secs(15);
/// How long an unanswered or refused ring waits before it is tried again.
pub const RETRY: Duration = Duration::from_secs(30);
/// How long the frontend may take to answer a `codex-wake` event.
const TYPED_WAIT: Duration = Duration::from_secs(15);

/// What the app knows about one codex row this tick.
pub struct Input {
    pub id: usize,
    /// Unread messages in its inbox.
    pub unread: usize,
    /// Its status is `waiting` — idle at its prompt, not mid-turn.
    pub waiting: bool,
    /// The thread to `codex queue` to, when Codex has minted one and the TUI
    /// is up; `None` means only the typed fallback can reach it.
    pub thread: Option<String>,
}

/// Starts one `codex queue` and reports how it ended. Injected so the state
/// machine can be tested without a codex.
pub type Queuer<'a> = &'a dyn Fn(&str, &str) -> Receiver<Result<(), String>>;

enum Phase {
    Idle,
    /// `codex queue` is running.
    Queueing { rx: Receiver<Result<(), String>>, wall: SystemTime, since: Instant },
    /// Waiting for the frontend's `wake_typed`.
    Typing { since: Instant },
    /// Sent; waiting for `wake/<id>.ack` to be touched after `wall`.
    Acking { wall: SystemTime, since: Instant },
}

struct Row {
    /// The unread count the last ring was for. A ring fires when the inbox
    /// grows past it; it resets when the inbox empties.
    rung_for: usize,
    last_try: Option<Instant>,
    /// When the last ring went out, so an ack that lands after `ACK_WAIT` still
    /// turns `unanswered` into `delivered` (a just-resumed codex took 8.5 s).
    rang_wall: Option<SystemTime>,
    /// How the last ring went out (`codex queue (N unread)` / `typed (…)`).
    how: String,
    phase: Phase,
    mark: Option<WakeMark>,
}

impl Default for Row {
    fn default() -> Self {
        Row { rung_for: 0, last_try: None, rang_wall: None, how: String::new(), phase: Phase::Idle, mark: None }
    }
}

/// Every codex row's wake state, for one project.
#[derive(Default)]
pub struct Wakes {
    rows: HashMap<usize, Row>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn log(state_dir: &Path, id: usize, state: &str, detail: &str) {
    use std::io::Write;
    let p = mulpex_core::wake_log_path(state_dir, id);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(f, "{}\t{state}\t{detail}", now_ms());
    }
}

/// When the codex last acked a wake line, if ever.
fn acked_at(state_dir: &Path, id: usize) -> Option<SystemTime> {
    std::fs::metadata(mulpex_core::wake_ack_path(state_dir, id)).and_then(|m| m.modified()).ok()
}

impl Wakes {
    /// What the row shows: the last attempt and how it went.
    pub fn mark(&self, id: usize) -> Option<WakeMark> {
        self.rows.get(&id).and_then(|r| r.mark.clone())
    }

    /// The row's process was replaced (⌘⇧R, a handoff) or closed.
    pub fn forget(&mut self, id: usize) {
        self.rows.remove(&id);
    }

    /// Drop rows that are no longer codexes here.
    pub fn retain(&mut self, keep: impl Fn(usize) -> bool) {
        self.rows.retain(|id, _| keep(*id));
    }

    fn set(row: &mut Row, state_dir: &Path, id: usize, at_ms: u64, state: &str, detail: String) {
        log(state_dir, id, state, &detail);
        row.mark = Some(WakeMark { at_ms, state: state.to_string(), detail });
    }

    /// Advance every codex row one tick. Returns the rows the frontend must
    /// type a wake line into (`codex-wake`): `(id, line)`.
    pub fn tick(&mut self, state_dir: &Path, inputs: &[Input], queue: Queuer) -> Vec<(usize, String)> {
        let mut typed = Vec::new();
        for i in inputs {
            let row = self.rows.entry(i.id).or_default();
            let at_ms = row.mark.as_ref().map_or(0, |m| m.at_ms);

            // Finish whatever is in flight.
            match &row.phase {
                Phase::Idle => {}
                Phase::Queueing { rx, wall, since } => match rx.try_recv() {
                    Ok(Ok(())) => row.phase = Phase::Acking { wall: *wall, since: *since },
                    Ok(Err(e)) => {
                        row.phase = Phase::Idle;
                        Self::set(row, state_dir, i.id, at_ms, "unanswered", format!("codex queue failed: {e}"));
                    }
                    Err(TryRecvError::Disconnected) => {
                        row.phase = Phase::Idle;
                        Self::set(row, state_dir, i.id, at_ms, "unanswered", "codex queue vanished".into());
                    }
                    Err(TryRecvError::Empty) => {}
                },
                Phase::Typing { since } => {
                    if since.elapsed() >= TYPED_WAIT {
                        row.phase = Phase::Idle;
                        Self::set(row, state_dir, i.id, at_ms, "unanswered", "the window never typed the wake line".into());
                    }
                }
                Phase::Acking { .. } => {}
            }
            if let Phase::Acking { wall, since } = &row.phase {
                let how = row.how.clone();
                if acked_at(state_dir, i.id).is_some_and(|t| t >= *wall) {
                    row.phase = Phase::Idle;
                    Self::set(row, state_dir, i.id, at_ms, "delivered", how);
                } else if since.elapsed() >= ACK_WAIT {
                    row.phase = Phase::Idle;
                    Self::set(
                        row,
                        state_dir,
                        i.id,
                        at_ms,
                        "unanswered",
                        format!("{how}: no ack from the codex within {} s", ACK_WAIT.as_secs()),
                    );
                }
            }

            // A late ack for the last ring: it did arrive, just slowly.
            if matches!(row.phase, Phase::Idle)
                && row.mark.as_ref().is_some_and(|m| m.state == "unanswered")
                && row.rang_wall.is_some_and(|w| acked_at(state_dir, i.id).is_some_and(|t| t >= w))
            {
                let how = format!("{} (acked late)", row.how);
                Self::set(row, state_dir, i.id, at_ms, "delivered", how);
            }

            if i.unread == 0 {
                // The mail was read; a warning about ringing for it is moot.
                row.rung_for = 0;
                if matches!(row.phase, Phase::Idle)
                    && row.mark.as_ref().is_some_and(|m| m.state == "unanswered" || m.state == "refused")
                {
                    row.mark = None;
                }
                continue;
            }
            if !matches!(row.phase, Phase::Idle) || !i.waiting {
                continue;
            }
            let failed = row.mark.as_ref().is_some_and(|m| m.state == "unanswered" || m.state == "refused");
            let grew = i.unread > row.rung_for;
            let retry = failed && row.last_try.is_none_or(|t| t.elapsed() >= RETRY);
            if !(grew || retry) {
                continue;
            }

            row.rung_for = i.unread;
            row.last_try = Some(Instant::now());
            let line = mulpex_core::codex::wake_line(i.unread);
            let at_ms = now_ms();
            row.rang_wall = Some(SystemTime::now());
            match &i.thread {
                Some(thread) => {
                    row.phase = Phase::Queueing { rx: queue(thread, &line), wall: SystemTime::now(), since: Instant::now() };
                    row.how = format!("codex queue ({} unread)", i.unread);
                    Self::set(row, state_dir, i.id, at_ms, "rang", row.how.clone());
                }
                None => {
                    row.phase = Phase::Typing { since: Instant::now() };
                    row.how = format!("typed ({} unread)", i.unread);
                    Self::set(row, state_dir, i.id, at_ms, "rang", row.how.clone());
                    typed.push((i.id, line));
                }
            }
        }
        typed
    }

    /// The frontend's answer to a `codex-wake`: it typed the line (`ok`), or
    /// refused (a draft in the box) and says why.
    pub fn typed(&mut self, state_dir: &Path, id: usize, ok: bool, reason: &str) {
        let Some(row) = self.rows.get_mut(&id) else { return };
        if !matches!(row.phase, Phase::Typing { .. }) {
            return;
        }
        let at_ms = row.mark.as_ref().map_or_else(now_ms, |m| m.at_ms);
        if ok {
            let wall = SystemTime::now() - Duration::from_millis(500);
            row.rang_wall = Some(wall);
            row.phase = Phase::Acking { wall, since: Instant::now() };
        } else {
            row.phase = Phase::Idle;
            let reason = if reason.trim().is_empty() { "the box was not empty" } else { reason.trim() };
            Self::set(row, state_dir, id, at_ms, "refused", reason.to_string());
        }
    }
}

/// Run `codex queue` off-thread, killed after [`QUEUE_TIMEOUT`], stdin closed
/// (a codex subcommand otherwise may wait on it). The child gets the same
/// environment an agent row does, minus every hub identity: it is not an
/// instance.
pub fn spawn_queue(bin: PathBuf, cwd: PathBuf, thread: &str, line: &str) -> Receiver<Result<(), String>> {
    use std::process::{Command, Stdio};
    let (tx, rx) = mpsc::channel();
    let (thread, line) = (thread.to_string(), line.to_string());
    std::thread::spawn(move || {
        let mut cmd = Command::new(&bin);
        cmd.args(["queue", "--thread", &thread, "--message", &line])
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in crate::claude_bin::forwarded_env() {
            if !mulpex_core::codex::CHILD_ENV_DENY.contains(&k.as_str()) {
                cmd.env(k, v);
            }
        }
        for k in mulpex_core::codex::CHILD_ENV_DENY {
            cmd.env_remove(k);
        }
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("MULPEX_") {
                cmd.env_remove(k);
            }
        }
        cmd.env("PATH", crate::claude_bin::merged_path());
        let res = match cmd.spawn() {
            Err(e) => Err(e.to_string()),
            Ok(mut child) => {
                let deadline = Instant::now() + QUEUE_TIMEOUT;
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) if Instant::now() >= deadline => {
                            let _ = child.kill();
                            break;
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                        Err(_) => break,
                    }
                }
                match child.wait_with_output() {
                    Ok(out) if out.status.success() => Ok(()),
                    Ok(out) => {
                        // codex prints its reason on either stream.
                        let text = [out.stderr, out.stdout]
                            .iter()
                            .map(|b| String::from_utf8_lossy(b).trim().to_string())
                            .find(|t| !t.is_empty())
                            .unwrap_or_else(|| match out.status.code() {
                                Some(c) => format!("exit {c}"),
                                None => format!("killed after {} s", QUEUE_TIMEOUT.as_secs()),
                            });
                        Err(text)
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
        };
        let _ = tx.send(res);
    });
    rx
}

/// Whether a codex rollout's tail says its last turn is over: the newest
/// turn-lifecycle event is `task_complete` (a finished turn — and also how a
/// FAILED one ends, with an `error` field: a 429 "exceeded retry limit" and a
/// bad model both measured that way, codex-cli 0.149.1) or `turn_aborted`
/// (Esc, `reason: "interrupted"`), not a `task_started` after it.
///
/// Exists because Codex fires no `Stop` hook on a failed turn: the row stayed
/// `working` forever, so it looked busy and was never woken for mail.
pub fn rollout_turn_over(tail: &str) -> bool {
    for line in tail.lines().rev() {
        // The first line of a tail is usually cut; it simply fails to parse.
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v["type"] != "event_msg" {
            continue;
        }
        match v["payload"]["type"].as_str() {
            Some("task_complete" | "turn_aborted") => return true,
            Some("task_started") => return false,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mulpex-wake-{}", mulpex_core::persist::new_uuid()));
        std::fs::create_dir_all(d.join(mulpex_core::WAKE_DIR)).unwrap();
        d
    }

    fn input(unread: usize, waiting: bool, thread: Option<&str>) -> Vec<Input> {
        vec![Input { id: 4, unread, waiting, thread: thread.map(str::to_string) }]
    }

    /// A queuer that records its calls and answers `ok` at once.
    fn queuer(calls: &RefCell<Vec<(String, String)>>, ok: bool) -> impl Fn(&str, &str) -> Receiver<Result<(), String>> + '_ {
        move |t: &str, l: &str| {
            calls.borrow_mut().push((t.to_string(), l.to_string()));
            let (tx, rx) = mpsc::channel();
            tx.send(if ok { Ok(()) } else { Err("boom".into()) }).unwrap();
            rx
        }
    }

    fn state(w: &Wakes) -> Option<String> {
        w.mark(4).map(|m| m.state)
    }

    #[test]
    fn rings_an_idle_codex_once_per_growth_and_counts_only_an_ack() {
        let sd = dir();
        let calls = RefCell::new(Vec::new());
        let q = queuer(&calls, true);
        let mut w = Wakes::default();
        // Busy: no ring.
        assert!(w.tick(&sd, &input(1, false, Some("T")), &q).is_empty());
        assert!(calls.borrow().is_empty());
        // Idle with mail: rings via codex queue with the wake line.
        w.tick(&sd, &input(1, true, Some("T")), &q);
        assert_eq!(calls.borrow().as_slice(), &[("T".to_string(), mulpex_core::codex::wake_line(1))]);
        assert_eq!(state(&w).as_deref(), Some("rang"));
        // Same count again: no second ring, still waiting for the ack.
        w.tick(&sd, &input(1, true, Some("T")), &q);
        assert_eq!(calls.borrow().len(), 1);
        assert_eq!(state(&w).as_deref(), Some("rang"));
        // The codex's hook acks: delivered.
        std::fs::write(mulpex_core::wake_ack_path(&sd, 4), "").unwrap();
        w.tick(&sd, &input(1, true, Some("T")), &q);
        assert_eq!(state(&w).as_deref(), Some("delivered"));
        // More mail: rings again, for the new count.
        w.tick(&sd, &input(3, true, Some("T")), &q);
        assert_eq!(calls.borrow().len(), 2);
        assert_eq!(calls.borrow()[1].1, mulpex_core::codex::wake_line(3));
        let log = std::fs::read_to_string(mulpex_core::wake_log_path(&sd, 4)).unwrap();
        assert_eq!(log.lines().count(), 3, "{log}");
        let _ = std::fs::remove_dir_all(sd);
    }

    #[test]
    fn a_failed_queue_is_unanswered_and_not_retried_before_the_interval() {
        let sd = dir();
        let calls = RefCell::new(Vec::new());
        let q = queuer(&calls, false);
        let mut w = Wakes::default();
        w.tick(&sd, &input(2, true, Some("T")), &q);
        w.tick(&sd, &input(2, true, Some("T")), &q);
        assert_eq!(state(&w).as_deref(), Some("unanswered"));
        assert!(w.mark(4).unwrap().detail.contains("boom"));
        w.tick(&sd, &input(2, true, Some("T")), &q);
        assert_eq!(calls.borrow().len(), 1, "retried inside RETRY");
        // The mail is read some other way: the warning goes.
        w.tick(&sd, &input(0, true, Some("T")), &q);
        assert_eq!(state(&w), None);
        let _ = std::fs::remove_dir_all(sd);
    }

    /// Shapes copied from real rollouts (trimmed): a turn that failed on a
    /// 429, one interrupted with Esc, and one still running after an earlier
    /// failure.
    #[test]
    fn a_failed_or_aborted_rollout_turn_is_over_and_a_new_one_is_not() {
        let started = r#"{"timestamp":"2026-10-21T09:41:08Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1","started_at":1792565268,"model_context_window":258400}}"#;
        let user = r#"{"timestamp":"2026-10-21T09:41:08Z","type":"event_msg","payload":{"type":"item_completed","thread_id":"x","turn_id":"t1","item":{"type":"UserMessage"}}}"#;
        let failed = r#"{"timestamp":"2026-10-21T09:41:20Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":null,"error":{"message":"exceeded retry limit, last status: 429 Too Many Requests"}}}"#;
        let aborted = r#"{"timestamp":"2026-08-28T09:58:14Z","type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t1","reason":"interrupted","duration_ms":712}}"#;
        let cut = r#"ted","turn_id":"t0"}}"#;
        assert!(rollout_turn_over(&[cut, started, user, failed].join("\n")));
        assert!(rollout_turn_over(&[started, user, aborted, ""].join("\n")));
        assert!(!rollout_turn_over(&[failed, started, user].join("\n")));
        assert!(!rollout_turn_over(&[cut, user].join("\n")));
    }

    #[test]
    fn an_ack_after_the_wait_still_counts_as_delivered() {
        let sd = dir();
        let calls = RefCell::new(Vec::new());
        let q = queuer(&calls, true);
        let mut w = Wakes::default();
        w.tick(&sd, &input(1, true, Some("T")), &q);
        w.tick(&sd, &input(1, true, Some("T")), &q);
        // Force the wait to have run out.
        if let Some(Row { phase: Phase::Acking { since, .. }, .. }) = w.rows.get_mut(&4) {
            *since = Instant::now() - ACK_WAIT;
        }
        w.tick(&sd, &input(1, true, Some("T")), &q);
        assert_eq!(state(&w).as_deref(), Some("unanswered"));
        std::fs::write(mulpex_core::wake_ack_path(&sd, 4), "").unwrap();
        w.tick(&sd, &input(1, true, Some("T")), &q);
        assert_eq!(state(&w).as_deref(), Some("delivered"));
        assert_eq!(calls.borrow().len(), 1);
        let _ = std::fs::remove_dir_all(sd);
    }

    #[test]
    fn without_a_thread_the_window_types_and_a_refusal_is_recorded() {
        let sd = dir();
        let calls = RefCell::new(Vec::new());
        let q = queuer(&calls, true);
        let mut w = Wakes::default();
        let typed = w.tick(&sd, &input(1, true, None), &q);
        assert_eq!(typed, vec![(4, mulpex_core::codex::wake_line(1))]);
        assert!(calls.borrow().is_empty());
        w.typed(&sd, 4, false, "a draft is in the box");
        assert_eq!(state(&w).as_deref(), Some("refused"));
        assert_eq!(w.mark(4).unwrap().detail, "a draft is in the box");
        // A late answer for a ring that is no longer typing changes nothing.
        w.typed(&sd, 4, true, "");
        assert_eq!(state(&w).as_deref(), Some("refused"));
        // Next growth types again; this time it goes in and is acked.
        let typed = w.tick(&sd, &input(2, true, None), &q);
        assert_eq!(typed.len(), 1);
        w.typed(&sd, 4, true, "");
        std::fs::write(mulpex_core::wake_ack_path(&sd, 4), "").unwrap();
        w.tick(&sd, &input(2, true, None), &q);
        assert_eq!(state(&w).as_deref(), Some("delivered"));
        let _ = std::fs::remove_dir_all(sd);
    }
}
