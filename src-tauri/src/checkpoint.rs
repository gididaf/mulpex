//! ⌘⇧Q Checkpoint & Quit, and the "drive again" offer on the next launch.
//!
//! Quitting with several projects of working claudes used to mean stopping each
//! one by hand and, after relaunch, waking each one by hand. This asks every live
//! claude in every project to stop at a safe checkpoint, lets the frontend watch
//! them go idle (`progress`), and records who was asked (`checkpoint.json` under
//! the Mulpex home) right before the quit. The next launch offers to wake exactly
//! those claudes.
//!
//! Every message goes through the hub inbox as the app itself (sender "Mulpex",
//! `Core::send_app_message`) — never typed into a TUI. A working claude gets it
//! through the Stop hook's unread-mail block, an idle one through its listener.
//!
//! The wake is the one subtle part: the listener only announces mail that
//! arrives *after* it starts, and right after a relaunch the restored claudes
//! are still booting. So a wake is queued and the poll loop delivers each one
//! once that claude's listener is up (`drain_wake_queue`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::snapshot::ProjectHandle;
use crate::state::Workspace;

const STOP_BODY: &str = "Mulpex is about to quit (the user is closing it for a while, or \
updating it). Stop at a safe checkpoint now: finish only the step you are on — leave no file \
half-edited — and start nothing new. Stop any dev servers, watchers or long-running jobs you \
started in the background; Mulpex quitting would kill them anyway. Then end your turn with a \
short note: what is done, what is next, which background jobs you stopped, and which monitors \
you had armed (agentalk, an artifact watch, anything else — but not the Mulpex hub listener, \
which comes back by itself). If you were idle with nothing in progress, one line saying so is \
enough. After that stay idle: if anything wakes you before Mulpex quits, start no new work. Do \
NOT hub_send a reply — going idle is your confirmation. When Mulpex is back you will get a \
message telling you to continue.";

const CANCEL_BODY: &str = "Never mind — Mulpex is not quitting after all. Carry on with what \
you were doing before the checkpoint request: restart any jobs you stopped and re-arm any \
monitors you disarmed, if they are still needed. If you were idle, stay idle. Do NOT hub_send a \
reply.";

const WAKE_BODY: &str = "Mulpex is back. Before it quit, you stopped at a checkpoint and wrote a \
note on where you were — find it in your conversation above. Re-arm the monitors you noted \
(agentalk, artifact watches, anything else; the Mulpex hub listener is already running — do not \
arm it), restart any dev servers or jobs you still need, and continue the work if anything is \
left to do. If you were idle with nothing in progress, stay idle. Do NOT hub_send a reply.";

/// How long a queued wake waits for a listener before it is delivered anyway, so
/// it at least sits in the inbox for the claude's next turn.
const WAKE_WAIT: Duration = Duration::from_secs(180);

/// A stop message that went out, and the inbox file that proves it is unread.
struct Sent {
    handle: ProjectHandle,
    id: usize,
    file: Option<PathBuf>,
}

/// The in-memory half: who was asked to stop this run, and wakes not yet
/// delivered. Lives on `Workspace`.
#[derive(Default)]
pub struct Checkpoint {
    sent: Vec<Sent>,
    wake: Vec<(ProjectHandle, usize, Instant)>,
}

/// One claude in the stop (or wake) list, as the frontend lists it.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub handle: ProjectHandle,
    pub project: String,
    pub id: usize,
    pub name: Option<String>,
}

/// What the backend alone can tell about a claude asked to stop. Its status
/// word the frontend already has, from the hub snapshot.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub handle: ProjectHandle,
    pub id: usize,
    /// The claude is still running. One that exited has stopped, trivially.
    pub alive: bool,
    /// The stop message is still in its inbox.
    pub unread: bool,
    /// Its hub listener is running — without one, an idle claude never reads it.
    pub listener: bool,
}

/// `checkpoint.json`: the claudes asked to stop, by project dir (handles do not
/// survive a relaunch; instance ids do).
#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Saved {
    ts: u64,
    entries: Vec<SavedEntry>,
    /// Wake them on the next launch without asking: the restart was ordered
    /// from another Mulpex (`remote_update.rs`) and nobody is at this screen
    /// to click "Wake all".
    #[serde(default)]
    auto_wake: bool,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct SavedEntry {
    project_dir: String,
    id: usize,
}

fn saved_path() -> PathBuf {
    mulpex_core::mulpex_home().join("checkpoint.json")
}

fn read_saved() -> Option<Saved> {
    serde_json::from_str(&std::fs::read_to_string(saved_path()).ok()?).ok()
}

/// Forget the checkpoint — after a wake, or "Not now".
pub fn dismiss() {
    let _ = std::fs::remove_file(saved_path());
}

impl Workspace {
    /// Ask every live claude in every project to stop at a checkpoint. Replaces
    /// any earlier round: a second ⌘⇧Q re-asks everyone, including claudes
    /// started since.
    pub fn checkpoint_stop(&mut self) -> Vec<Target> {
        let mut sent = Vec::new();
        let mut targets = Vec::new();
        for core in &self.projects {
            for id in core.live_claude_ids() {
                sent.push(Sent {
                    handle: core.handle,
                    id,
                    file: core.send_app_message(id, STOP_BODY),
                });
                targets.push(Target {
                    handle: core.handle,
                    project: core.project_name.clone(),
                    id,
                    name: core.label_of(id),
                });
            }
        }
        self.checkpoint.sent = sent;
        targets
    }

    pub fn checkpoint_progress(&self) -> Vec<Progress> {
        self.checkpoint
            .sent
            .iter()
            .map(|s| {
                let core = self.project(s.handle);
                let alive = core.is_some_and(|c| c.live_claude_ids().contains(&s.id));
                Progress {
                    handle: s.handle,
                    id: s.id,
                    alive,
                    unread: s.file.as_ref().is_some_and(|f| f.exists()),
                    listener: core.is_some_and(|c| c.listener_ready(s.id)),
                }
            })
            .collect()
    }

    /// Call the round off. A claude that never read the stop message just has it
    /// taken back; one that did is told to carry on.
    pub fn checkpoint_cancel(&mut self) {
        for s in std::mem::take(&mut self.checkpoint.sent) {
            let Some(core) = self.project(s.handle) else {
                continue;
            };
            if let Some(f) = &s.file {
                if std::fs::remove_file(f).is_ok() {
                    continue;
                }
            }
            if core.live_claude_ids().contains(&s.id) {
                core.send_app_message(s.id, CANCEL_BODY);
            }
        }
    }

    /// Record who was asked to stop, right before the quit (or the update's
    /// restart), so the next launch can offer to wake them.
    pub fn checkpoint_commit(&self, auto_wake: bool) -> anyhow::Result<()> {
        let entries = self
            .checkpoint
            .sent
            .iter()
            .filter_map(|s| {
                let core = self.project(s.handle)?;
                core.live_claude_ids().contains(&s.id).then(|| SavedEntry {
                    project_dir: core.project_dir.display().to_string(),
                    id: s.id,
                })
            })
            .collect::<Vec<_>>();
        if entries.is_empty() {
            dismiss();
            return Ok(());
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = saved_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string(&Saved { ts, entries, auto_wake })?)?;
        Ok(())
    }

    /// The claudes from the last checkpoint that are back (their project is open
    /// and they were restored). Empty when there is nothing to offer — and then
    /// the stale file is dropped, so it can never surface on a later launch.
    pub fn checkpoint_pending(&self) -> Vec<Target> {
        let Some(saved) = read_saved() else {
            return Vec::new();
        };
        let targets = self.match_saved(&saved);
        if targets.is_empty() {
            dismiss();
        }
        targets
    }

    fn match_saved(&self, saved: &Saved) -> Vec<Target> {
        let mut out = Vec::new();
        for e in &saved.entries {
            let canon = std::fs::canonicalize(&e.project_dir)
                .unwrap_or_else(|_| PathBuf::from(&e.project_dir));
            let Some(core) = self.projects.iter().find(|c| c.project_dir == canon) else {
                continue;
            };
            if core.live_claude_ids().contains(&e.id) {
                out.push(Target {
                    handle: core.handle,
                    project: core.project_name.clone(),
                    id: e.id,
                    name: core.label_of(e.id),
                });
            }
        }
        out
    }

    /// "Wake all": queue the wake for every claude in the checkpoint and forget
    /// the file. Delivery is `drain_wake_queue`'s, from the poll loop.
    pub fn checkpoint_wake(&mut self) -> usize {
        let targets = self.checkpoint_pending();
        dismiss();
        let now = Instant::now();
        self.checkpoint
            .wake
            .extend(targets.iter().map(|t| (t.handle, t.id, now)));
        targets.len()
    }

    /// At launch: a checkpoint written by a remote-ordered update wakes by
    /// itself, before the frontend could offer it. Returns how many were queued.
    pub fn checkpoint_auto_wake(&mut self) -> usize {
        if !read_saved().is_some_and(|s| s.auto_wake) {
            return 0;
        }
        self.checkpoint_wake()
    }

    /// The claudes asked to stop this round that have not stopped yet: alive,
    /// and either the stop message is unread or the claude is not `waiting`.
    /// The headless twin of `checkpoint.ts`'s "done" rule (minus its settle
    /// delay, which `remote_update.rs` applies itself).
    pub fn checkpoint_not_stopped(&self) -> Vec<Target> {
        self.checkpoint
            .sent
            .iter()
            .filter_map(|s| {
                let core = self.project(s.handle)?;
                if !core.live_claude_ids().contains(&s.id) {
                    return None;
                }
                let unread = s.file.as_ref().is_some_and(|f| f.exists());
                let word = std::fs::read_to_string(core.state_dir.join(s.id.to_string())).unwrap_or_default();
                // A missing file is `waiting` everywhere else in Mulpex too.
                let waiting = matches!(word.trim(), "" | "waiting");
                (unread || !waiting).then(|| Target {
                    handle: core.handle,
                    project: core.project_name.clone(),
                    id: s.id,
                    name: core.label_of(s.id),
                })
            })
            .collect()
    }

    /// Deliver each queued wake once that claude's listener is up. A claude that
    /// is gone is dropped; one whose listener never comes gets the message after
    /// `WAKE_WAIT` anyway. Cheap when the queue is empty, which is nearly always.
    pub fn drain_wake_queue(&mut self) {
        if self.checkpoint.wake.is_empty() {
            return;
        }
        let queue = std::mem::take(&mut self.checkpoint.wake);
        for (handle, id, at) in queue {
            let Some(core) = self.project(handle) else {
                continue;
            };
            if !core.live_claude_ids().contains(&id) {
                continue;
            }
            if core.listener_ready(id) || at.elapsed() >= WAKE_WAIT {
                core.send_app_message(id, WAKE_BODY);
            } else {
                self.checkpoint.wake.push((handle, id, at));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_round_trips() {
        let s = Saved {
            ts: 5,
            entries: vec![SavedEntry {
                project_dir: "/tmp/x".into(),
                id: 3,
            }],
            auto_wake: true,
        };
        let back: Saved = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    /// A `checkpoint.json` written before `auto_wake` existed still reads.
    #[test]
    fn an_old_checkpoint_does_not_auto_wake() {
        let s: Saved = serde_json::from_str(r#"{"ts":1,"entries":[]}"#).unwrap();
        assert!(!s.auto_wake);
    }

    /// Entries for a project that is not open match nothing — that is what keeps
    /// a stale checkpoint from offering to wake claudes that are not there.
    #[test]
    fn a_closed_project_matches_nothing() {
        let ws = Workspace::new();
        let saved = Saved {
            ts: 1,
            entries: vec![SavedEntry {
                project_dir: "/definitely/not/open".into(),
                id: 1,
            }],
            auto_wake: false,
        };
        assert!(ws.match_saved(&saved).is_empty());
    }

    /// The bodies tell the claude the one thing it cannot know: not to reply.
    #[test]
    fn every_body_forbids_a_reply() {
        for b in [STOP_BODY, CANCEL_BODY, WAKE_BODY] {
            assert!(b.contains("Do NOT hub_send a reply"), "{b}");
        }
    }
}
