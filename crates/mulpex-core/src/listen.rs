//! `mulpex-helper listen` — the hub listener, as a program instead of a prompt.
//!
//! An instance has to be woken when a peer messages it while it sits idle between
//! the user's turns, and only the agent itself can arm a watcher on its own
//! inbox. So `HUB_RULES` asks it to start one with the `Monitor` tool, and every
//! line this prints becomes a notification in that instance's chat.
//!
//! **Why this is a binary and not the shell one-liner it replaces.** The command
//! `HUB_RULES` asks for is *transcribed by the model*, and the model copies
//! whichever version is in its context — which is usually its own previous
//! `Monitor` call, not the system prompt. Measured on a live instance
//! (`warweb#65`, 2026-09-16): it armed a superseded copy of the command **71
//! times across two days**, straight through an app update that changed it, and
//! only picked up the current text when `/compact` finally dropped the old call
//! from its context. Its sibling in the same project never recovered at all.
//!
//! That is not a bug in one string, it is the shape of the interface: a ~400
//! character shell one-liner retyped from prose has no way to be verified, and
//! any edit to it silently strands every session already running. One short
//! command naming an absolute path has no such failure mode — the loop's
//! behaviour ships with the app, so changing it never asks a model to retype
//! anything, and a session running last week's binary picks up this week's logic
//! the next time it re-arms.
//!
//! The helper also lives inside the `.app`, which matters: anything written once
//! into `$MULPEX_STATE_DIR` has a three-day fuse (macOS purges untouched files in
//! `$TMPDIR`), so a script in the scratch dir would have been unrunnable for an
//! instance that had been open over a long weekend — at exactly the moment it
//! needed to re-arm.
//!
//! What it prints is a contract in the other direction: `HUB_RULES` tells the
//! instance that a line starting `mulpex:` is peer mail arriving, so the wording
//! here and there must not drift.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One pass per second, matching the shell loop it replaces. Fast enough that a
/// hub message feels immediate, slow enough to be free.
const TICK: Duration = Duration::from_secs(1);

/// Paths this listener watches, resolved once from the environment `claude`
/// inherited from the app.
struct Watch {
    state_dir: PathBuf,
    inbox: PathBuf,
    armed: PathBuf,
    /// `pids/<id>`: the `claude` that owns this listener. See `owner_is_gone`.
    owner_pid: PathBuf,
    /// `listeners/<id>`: the pid of whichever listener is already serving this
    /// instance, so a second one can stand down instead of doubling every event.
    lock: PathBuf,
}

impl Watch {
    fn from_env() -> anyhow::Result<Self> {
        let state_dir = PathBuf::from(
            std::env::var_os("MULPEX_STATE_DIR")
                .ok_or_else(|| anyhow::anyhow!("MULPEX_STATE_DIR is not set"))?,
        );
        let id = std::env::var("MULPEX_INSTANCE_ID")
            .map_err(|_| anyhow::anyhow!("MULPEX_INSTANCE_ID is not set"))?;
        Ok(Self {
            inbox: state_dir.join("inbox").join(&id),
            armed: state_dir.join("armed").join(&id),
            owner_pid: state_dir.join(crate::PIDS_DIR).join(&id),
            lock: state_dir.join(crate::LISTENERS_DIR).join(&id),
            state_dir,
        })
    }
}

/// How many messages are waiting. A missing directory reads as zero rather than
/// as an error: the inbox is created lazily and its absence is not news.
fn unread(inbox: &Path) -> usize {
    std::fs::read_dir(inbox)
        .map(|entries| entries.flatten().count())
        .unwrap_or(0)
}

/// Is this pid still running? `kill(pid, 0)` signals nothing and only asks.
/// `EPERM` means it exists but belongs to someone else, which still counts as
/// alive — erring toward *not* exiting, since a listener that quits early leaves
/// its instance unwakeable with nothing anywhere to say so.
fn alive(pid: libc::pid_t) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn read_pid(path: &Path) -> Option<libc::pid_t> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Has the `claude` this listener belongs to gone away?
///
/// This is the half of the lifetime the app cannot enforce. `Session::kill`
/// `killpg`s the child's process group and sweeps its controlling terminal, and a
/// listener escapes **both**: Claude Code runs each background command in its own
/// process group with no controlling tty (measured — `PGID == pid`, `SESS 0`,
/// state `Ss`), so it survives ⌘W, a crash, and app teardown alike, and gets
/// reparented to launchd still spinning. Six such orphans were found alive on one
/// machine, the oldest more than a day old.
///
/// `owner` is the pid `pids/<id>` held when this listener started, and it is
/// pinned on purpose. Reading the file afresh every tick is what went wrong on
/// `monorepo#4` (2026-10-05): the instance was restarted, `pids/4` was rewritten
/// with the *new* `claude`'s pid, and the old listener — whose `claude` was dead —
/// read that live pid as its own owner and carried on. It held the lock, so the new
/// plugin monitor stood down; the orphan was reaped later, and the instance sat
/// deaf to its mail. So: the pinned pid dying is death, and so is `pids/<id>` now
/// naming someone else — this slot belongs to a different `claude` now.
///
/// A missing `pids/<id>` is deliberately *not* treated as death: `mpx` does not
/// write one, and neither did any Mulpex before this shipped.
fn owner_is_gone(w: &Watch, owner: Option<libc::pid_t>) -> bool {
    let current = read_pid(&w.owner_pid);
    match owner {
        Some(pinned) => !alive(pinned) || current.is_some_and(|now| now != pinned),
        None => current.is_some_and(|pid| !alive(pid)),
    }
}

/// Take the lock unless another live listener already serves this instance.
///
/// Returns false while an incumbent holds it. First one wins: the incumbent is
/// already watching the same inbox, so the newcomer would only double every
/// wake-up — which is precisely what the user reported seeing. The loser does
/// not exit, though; see `run`.
fn claim(w: &Watch) -> bool {
    if let Some(pid) = read_pid(&w.lock) {
        if pid != std::process::id() as libc::pid_t && alive(pid) {
            return false;
        }
    }
    let _ = std::fs::create_dir_all(w.lock.parent().unwrap_or(&w.state_dir));
    let _ = std::fs::write(&w.lock, std::process::id().to_string());
    true
}

/// Watch this instance's inbox until its owner, or Mulpex, goes away.
pub fn run(_args: &[String]) -> anyhow::Result<()> {
    let w = Watch::from_env()?;
    let _ = std::fs::create_dir_all(&w.inbox);
    let _ = std::fs::create_dir_all(w.armed.parent().unwrap_or(&w.state_dir));

    let owner = read_pid(&w.owner_pid);

    // Another listener already serves this instance: wait behind it, silently,
    // and take over the moment it dies. This used to print "standing down" and
    // exit, and for the plugin monitor that is permanent — Claude Code never
    // restarts one. The incumbent was then the only listener left, and whenever it
    // went (a 30-minute model-armed Monitor expiring, an orphan being reaped) the
    // instance stopped hearing its mail with nothing anywhere to say so. Silent,
    // because every line printed here wakes the instance.
    let mut prev = unread(&w.inbox);
    while !claim(&w) {
        if !w.state_dir.is_dir() || owner_is_gone(&w, owner) {
            return Ok(());
        }
        // Tracked while waiting so a message landing in the second between the
        // incumbent's death and the takeover is still news.
        prev = unread(&w.inbox);
        std::thread::sleep(TICK);
    }

    loop {
        // Mulpex is gone (teardown removes the whole scratch root) or this
        // project was closed. Either way nobody is left to wake.
        if !w.state_dir.is_dir() {
            return Ok(());
        }
        if owner_is_gone(&w, owner) {
            // Only our own lock: after a restart the slot may already belong to
            // the new `claude`'s listener.
            if read_pid(&w.lock) == Some(std::process::id() as libc::pid_t) {
                let _ = std::fs::remove_file(&w.lock);
            }
            return Ok(());
        }

        let cur = unread(&w.inbox);
        if cur > prev {
            // The wording is load-bearing: `HUB_RULES` tells the instance that a
            // line starting `mulpex:` is peer mail and not something the user
            // typed, and the user reads these lines in the pane too.
            println!("mulpex: {} new hub message(s)", cur - prev);
            let _ = std::io::stdout().flush();
        }
        prev = cur;

        // The heartbeat `hook::listener_armed` reads. Written on every pass, not
        // once at startup: a monitor that expired must go stale within seconds,
        // or the arm nudge — the only thing that ever re-arms one — never returns.
        let _ = std::fs::write(&w.armed, "");
        std::thread::sleep(TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mulpex-listen-{tag}-{}", crate::persist::new_uuid()));
        for sub in ["inbox/4", "armed", crate::PIDS_DIR, crate::LISTENERS_DIR] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        dir
    }

    fn watch(dir: &Path) -> Watch {
        Watch {
            inbox: dir.join("inbox").join("4"),
            armed: dir.join("armed").join("4"),
            owner_pid: dir.join(crate::PIDS_DIR).join("4"),
            lock: dir.join(crate::LISTENERS_DIR).join("4"),
            state_dir: dir.to_path_buf(),
        }
    }

    /// Mail arriving is a *rise* in the count, not a non-zero count: the loop is
    /// re-armed every half hour into an inbox that may already hold messages the
    /// instance has seen, and re-announcing those would wake it for nothing.
    #[test]
    fn only_a_rise_in_the_count_is_news() {
        let dir = scratch("count");
        let w = watch(&dir);
        assert_eq!(unread(&w.inbox), 0);
        std::fs::write(w.inbox.join("m1"), "hi").unwrap();
        std::fs::write(w.inbox.join("m2"), "hi").unwrap();
        assert_eq!(unread(&w.inbox), 2);
        // A missing inbox is zero, not an error — it is created lazily.
        std::fs::remove_dir_all(&w.inbox).unwrap();
        assert_eq!(unread(&w.inbox), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The listener is not in its `claude`'s process group and has no controlling
    /// terminal, so neither `killpg` nor the tty sweep can reach it. Noticing the
    /// owner died is the only thing standing between that and an orphan that
    /// spins `sleep 1` until the machine reboots.
    #[test]
    fn a_listener_outlives_nothing() {
        let dir = scratch("owner");
        let w = watch(&dir);

        // No `pids/<id>` at all: an older Mulpex, or `mpx`. Keep running — a
        // listener that quits early is worse than one that lingers.
        assert!(!owner_is_gone(&w, None), "an unknown owner is not a dead owner");

        // Our own pid is alive by definition.
        std::fs::write(&w.owner_pid, std::process::id().to_string()).unwrap();
        assert!(!owner_is_gone(&w, None));

        // pid 1 is launchd; a pid that cannot exist is gone. (`i32::MAX` is above
        // any real pid on macOS.)
        std::fs::write(&w.owner_pid, "1").unwrap();
        assert!(!owner_is_gone(&w, None), "launchd is alive");
        std::fs::write(&w.owner_pid, i32::MAX.to_string()).unwrap();
        assert!(owner_is_gone(&w, None), "a pid that does not exist means the claude is gone");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `monorepo#4` case: the instance restarts, `pids/<id>` is rewritten with
    /// the new `claude`'s (live) pid, and the old listener must not adopt it as its
    /// own owner — it would hold the lock against the new `claude`'s listener.
    #[test]
    fn a_restarted_instance_retires_the_old_listener() {
        let dir = scratch("restart");
        let w = watch(&dir);
        let me = std::process::id() as libc::pid_t;

        std::fs::write(&w.owner_pid, me.to_string()).unwrap();
        assert!(!owner_is_gone(&w, Some(me)), "pinned owner alive, slot still ours");

        // Restart: a different live `claude` now holds the slot.
        std::fs::write(&w.owner_pid, "1").unwrap();
        assert!(owner_is_gone(&w, Some(me)), "the slot belongs to another claude now");

        // The pinned owner died, whatever the file says.
        std::fs::write(&w.owner_pid, i32::MAX.to_string()).unwrap();
        assert!(owner_is_gone(&w, Some(i32::MAX)));

        // The file vanishing is not news (pinned owner alive).
        std::fs::remove_file(&w.owner_pid).unwrap();
        assert!(!owner_is_gone(&w, Some(me)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two listeners on one inbox is exactly the reported symptom — one hub
    /// message, N wake-ups — so the second one stands down rather than
    /// double-reporting. A lock left behind by a dead listener must not block the
    /// replacement, which is the whole reason it stores a pid rather than a flag.
    #[test]
    fn the_second_listener_stands_down_but_a_dead_lock_does_not_block_one() {
        let dir = scratch("claim");
        let w = watch(&dir);

        assert!(claim(&w), "nothing holds the lock");
        assert_eq!(read_pid(&w.lock), Some(std::process::id() as libc::pid_t));
        assert!(claim(&w), "our own lock is not a rival");

        std::fs::write(&w.lock, "1").unwrap(); // launchd: alive, and not us
        assert!(!claim(&w), "a live incumbent wins");

        std::fs::write(&w.lock, i32::MAX.to_string()).unwrap();
        assert!(claim(&w), "a dead lock must not strand the instance unwatched");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
