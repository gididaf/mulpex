//! Killing what `kill-session` leaves behind.
//!
//! **Measured, 2026-09-06.** Tearing a project down with `tmux kill-session`
//! sends SIGHUP to each pane's process group. A plain `cmd &` dies with it. A
//! `nohup cmd &` **does not** — it ignores SIGHUP by definition — and was still
//! running after `mpx down` returned. That is precisely the leak
//! `docs/shell-terminals.md` records for the desktop app's own `killpg` path, and
//! it reaches the CLI unchanged, because both are process-group signals and the
//! process opted out of one.
//!
//! The escape is the **controlling terminal**, which a backgrounded or nohup'd
//! process keeps even after it stops caring about the group: everything that was
//! started in a pane shares that pane's tty, so `ps -t <tty>` finds it. This is
//! the same fact the desktop app reaches through `proc_pidinfo`'s `e_tdev`, via a
//! command that exists identically on macOS and Linux.
//!
//! **Known limit, deliberately not chased:** a process that calls `setsid` leaves
//! the session and drops the controlling terminal, so nothing keyed on the tty can
//! find it. The desktop app has the same hole. A true daemon is meant to outlive
//! its launcher, and killing things by name or cwd would be worse than the leak.

use std::process::Command;

/// Collect the pids still attached to `tty`, excluding ourselves.
///
/// `tty` is tmux's `#{pane_tty}`, e.g. `/dev/ttys012`; `ps` wants it without the
/// `/dev/` prefix on macOS and accepts either on Linux.
pub fn pids_on_tty(tty: &str) -> Vec<i32> {
    let short = tty.strip_prefix("/dev/").unwrap_or(tty);
    let me = std::process::id() as i32;
    Command::new("ps")
        .args(["-t", short, "-o", "pid="])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<i32>().ok())
                .filter(|p| *p != me)
                .collect()
        })
        .unwrap_or_default()
}

/// Everything currently attached to any of these ttys.
///
/// **Must be called BEFORE the session is killed.** Once tmux tears the pane
/// down the pty is released, and a survivor's controlling terminal no longer
/// resolves — `ps -t` then matches nothing and the sweep silently does nothing.
/// That is not hypothetical: it is why the first version of this reported
/// "swept 1" while the nohup'd job was still running.
///
/// Our own tty is excluded, so running `mpx down` from inside a pane of the very
/// session being torn down does not kill the shell issuing the command.
pub fn pids_to_sweep(ttys: &[String]) -> Vec<i32> {
    let own = own_tty();
    let mut pids: Vec<i32> = ttys
        .iter()
        .filter(|t| own.as_deref() != Some(t.as_str()))
        .flat_map(|t| pids_on_tty(t))
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// This process's controlling terminal, if it has one.
fn own_tty() -> Option<String> {
    let out = Command::new("ps")
        .args(["-p", &std::process::id().to_string(), "-o", "tty="])
        .output()
        .ok()?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if t.is_empty() || t == "??" || t == "?" {
        None
    } else {
        Some(format!("/dev/{t}"))
    }
}

/// SIGKILL any of `pids` still alive. Returns how many it killed.
///
/// SIGKILL, not SIGTERM: this runs *after* `kill-session` has already delivered a
/// polite hangup that these processes demonstrably ignored, so a second
/// catchable signal would just be a second thing to ignore.
pub fn kill_pids(pids: &[i32]) -> usize {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    let mut killed = 0;
    for pid in pids {
        // SAFETY: SIGKILL to a pid observed on a tty this project owned. An
        // already-exited pid returns ESRCH, which we ignore.
        if unsafe { kill(*pid, SIGKILL) } == 0 {
            killed += 1;
        }
    }
    killed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tty nobody is on must yield nothing rather than, say, every process.
    /// Getting this wrong would make teardown kill the user's whole login.
    #[test]
    fn an_unused_tty_yields_no_pids() {
        assert!(pids_on_tty("/dev/ttys999").is_empty());
        assert!(pids_on_tty("").is_empty());
    }

    #[test]
    fn the_dev_prefix_is_optional() {
        // Both spellings must behave the same; on a tty-less test runner both are
        // empty, which still proves neither panics or shells out wrongly.
        assert_eq!(pids_on_tty("/dev/ttys998"), pids_on_tty("ttys998"));
    }
}

