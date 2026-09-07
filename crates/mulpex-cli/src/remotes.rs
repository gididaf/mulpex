//! Remote peers: a `claude` running over ssh on another machine, in one of our
//! terminals, that signals back when it is done.
//!
//! `hub_remote_open` (in `mulpex-core::mcp`) does the launching — it opens a
//! terminal, ssh's, and starts a `claude` there carrying peer rules and a token.
//! Everything in this file is the other half: **watching that terminal's
//! transcript for the peer's signal and waking the instance that is driving it.**
//! Without this the remote does its work, prints its marker, and nobody is ever
//! told, because a remote is a terminal and a terminal is not a hub peer — it can
//! never `hub_send`.
//!
//! Two channels are read, both of them, always: the transcript **and** the
//! screen. A row only reaches the transcript once it scrolls off the top, so a
//! remote that answers briefly and then sits there has its marker on screen and
//! nowhere else. That is the same asymmetry `hub_terminal_read` lives with, and
//! for the same reason.
//!
//! ## The silence backstop
//!
//! A remote claude is asked to signal, and sometimes does not — it finishes its
//! turn and simply stops. So silence is a second, weaker signal: if the terminal
//! has been quiet past `IDLE_TURN_END_MS`, the screen still looks like a claude
//! TUI, and its spinner is not animating, the turn is over.
//!
//! It fires **once**, and only while the remote actually owes an answer. The debt
//! is taken on when the driver sends it something and settled the moment anything
//! is delivered — otherwise one quiet remote would wake its driver on every 200 ms
//! tick, forever.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use mulpex_core::remote;

use crate::core::Project;

#[derive(Default)]
pub struct Remotes {
    /// Remote terminals that have been given something and owe an answer.
    awaiting: HashSet<(PathBuf, usize)>,
}

impl Remotes {
    /// The driver has just sent this remote something, so it owes an answer. This
    /// is what arms the silence backstop; sending to an ordinary shell arms
    /// nothing, because a shell is not going to be asked whether its turn ended.
    pub fn expect_reply(&mut self, state_dir: &Path, id: usize) {
        self.awaiting.insert((state_dir.to_path_buf(), id));
    }

    /// One pass over every remote terminal in every project.
    pub fn tick(&mut self, projects: &[Project]) {
        for p in projects {
            for (id, meta) in remote::RemoteMeta::all(&p.state_dir) {
                self.check(p, id, &meta);
            }
        }
    }

    fn check(&mut self, p: &Project, id: usize, meta: &remote::RemoteMeta) {
        let Some(inst) = p.find(id) else {
            // The terminal is gone. Drop its records so a future id can never
            // inherit another remote's token — a wake attributed to the wrong
            // machine is worse than no wake.
            remote::forget_all(&p.state_dir, id);
            self.awaiting.remove(&(p.state_dir.clone(), id));
            return;
        };
        // Nothing to deliver to: the driver has closed. The remote keeps running —
        // it is the user's terminal now — but nobody is listening.
        if !p.instances.iter().any(|i| i.id == meta.opener && i.is_claude() && !i.dead) {
            return;
        }

        let dir = p.state_dir.join("terminals");
        let log_bytes = std::fs::read(dir.join(format!("{id}.log"))).unwrap_or_default();
        let log = String::from_utf8_lossy(
            log_bytes.get(mulpex_core::termlog::HEADER_LEN..).unwrap_or_default(),
        )
        .into_owned();
        let screen = std::fs::read_to_string(dir.join(format!("{id}.screen"))).unwrap_or_default();

        let signal = remote::find_signals(&log, &meta.token)
            .into_iter()
            .last()
            .or_else(|| remote::find_signals(&screen, &meta.token).into_iter().last());

        // Idleness comes from the transcript header the recorder maintains, not
        // from tmux: the recorder is what actually decides when something changed.
        let idle_ms = mulpex_core::termlog::parse_header(&log_bytes)
            .map(|h| now_ms().saturating_sub(h.last_out_ms))
            .unwrap_or(0);
        let quiet = !inst.dead
            && idle_ms >= remote::IDLE_TURN_END_MS
            && remote::looks_like_claude_tui(&screen)
            && !remote::has_spinner(&screen);

        let key = (p.state_dir.clone(), id);
        let to_send = match signal {
            // Deduped on disk, per reader: the marker stays in the transcript
            // forever, so without this the same signal wakes the driver on every
            // tick for the life of the terminal.
            Some(sig) => remote::take_if_new(&p.state_dir, id, "watch", &sig).then_some(sig),
            // Silence only means "your turn" if the remote owes an answer.
            None if quiet && self.awaiting.contains(&key) => Some(remote::Signal {
                kind: remote::Kind::Ended,
                summary: String::new(),
            }),
            _ => None,
        };

        let Some(sig) = to_send else { return };
        // The debt is settled: nothing further is owed until the driver speaks
        // again. This is what stops one quiet period from waking the driver five
        // times a second.
        self.awaiting.remove(&key);
        deliver(&p.state_dir, meta.opener, id, &remote::wake_body(id, &meta.ssh_target, &sig));
    }
}

/// Put a message in an instance's inbox as the hub itself.
///
/// `from: 0` is Mulpex speaking rather than a peer — there is no instance behind
/// a remote terminal to attribute it to, and `from_terminal` is what tells the
/// driver to reply with `hub_terminal_send` and not `hub_send`.
fn deliver(state_dir: &Path, to: usize, from_terminal: usize, body: &str) {
    let dir = state_dir.join("inbox").join(to.to_string());
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ts = now_ms() / 1000;
    let name = format!("{}-{}.json", ts, mulpex_core::persist::new_uuid());
    let _ = std::fs::write(
        dir.join(name),
        serde_json::json!({
            "from": 0,
            "from_terminal": from_terminal,
            "ts": ts,
            "body": body,
        })
        .to_string(),
    );
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mpx-rem-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The wake has to reach the driver's inbox in the shape the hook reads, and
    /// it must say the sender was a TERMINAL — `hub_send` can never reach a
    /// remote, so a driver told to reply to `claude#N` would be stuck.
    #[test]
    fn a_wake_lands_in_the_drivers_inbox_marked_as_coming_from_a_terminal() {
        let d = tmp("deliver");
        deliver(&d, 1, 5, "the remote finished");
        let files: Vec<_> = std::fs::read_dir(d.join("inbox/1")).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(files[0].path()).unwrap()).unwrap();
        assert_eq!(v["from"], 0, "the hub is speaking, not a peer");
        assert_eq!(v["from_terminal"], 5);
        assert_eq!(v["body"], "the remote finished");
    }

    /// The backstop is armed by the driver speaking and disarmed by anything being
    /// delivered. Without the second half, one quiet remote wakes its driver on
    /// every tick — five times a second, forever.
    #[test]
    fn the_silence_backstop_is_owed_once_and_settled_once() {
        let d = tmp("await");
        let mut r = Remotes::default();
        let key = (d.clone(), 3);
        assert!(!r.awaiting.contains(&key), "nothing is owed before the driver speaks");
        r.expect_reply(&d, 3);
        assert!(r.awaiting.contains(&key));
        r.awaiting.remove(&key);
        assert!(!r.awaiting.contains(&key), "settled: silence must not fire again");
    }

    /// A real signal is deduped on disk, because the marker stays in the
    /// transcript for the life of the terminal.
    #[test]
    fn the_same_signal_wakes_the_driver_only_once() {
        let d = tmp("dedupe");
        let sig = remote::Signal { kind: remote::Kind::Ended, summary: "done".into() };
        assert!(remote::take_if_new(&d, 4, "watch", &sig), "first sighting is news");
        assert!(!remote::take_if_new(&d, 4, "watch", &sig), "the second is the same marker");
    }
}
