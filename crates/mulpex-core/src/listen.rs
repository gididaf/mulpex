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
/// A missing `pids/<id>` is deliberately *not* treated as death: no Mulpex before
/// this shipped wrote one, and listeners armed by those builds are still running
/// on this machine. (`mpx`, deleted 2026-09-17, did not write one either; the
/// older-builds half is what keeps the leniency necessary.)
fn owner_is_gone(w: &Watch) -> bool {
    match read_pid(&w.owner_pid) {
        Some(pid) => !alive(pid),
        None => false,
    }
}

/// Stand down if another listener is already serving this instance.
///
/// Returns false when this process should exit. First one wins: the incumbent is
/// already watching the same inbox, so the newcomer would only double every
/// wake-up — which is precisely what the user reported seeing.
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

    if !claim(&w) {
        // Not an error: the instance is watched, which is all that was being
        // asked for. Said out loud because a Monitor that ends instantly with no
        // explanation reads as a failure.
        println!("mulpex: a hub listener is already running for this instance — standing down");
        return Ok(());
    }

    let mut prev = unread(&w.inbox);
    loop {
        // Mulpex is gone (teardown removes the whole scratch root) or this
        // project was closed. Either way nobody is left to wake.
        if !w.state_dir.is_dir() {
            return Ok(());
        }
        if owner_is_gone(&w) {
            let _ = std::fs::remove_file(&w.lock);
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
        assert!(!owner_is_gone(&w), "an unknown owner is not a dead owner");

        // Our own pid is alive by definition.
        std::fs::write(&w.owner_pid, std::process::id().to_string()).unwrap();
        assert!(!owner_is_gone(&w));

        // pid 1 is launchd; a pid that cannot exist is gone. (`i32::MAX` is above
        // any real pid on macOS.)
        std::fs::write(&w.owner_pid, "1").unwrap();
        assert!(!owner_is_gone(&w), "launchd is alive");
        std::fs::write(&w.owner_pid, i32::MAX.to_string()).unwrap();
        assert!(owner_is_gone(&w), "a pid that does not exist means the claude is gone");

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
