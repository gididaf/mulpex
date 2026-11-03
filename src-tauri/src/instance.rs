//! One Mulpex per home. A second launch (a claude running the app binary, a
//! second autostart, a double-click on Linux) used to start a second, full app
//! on the same `MULPEX_HOME`: same PTY daemon, same session stores, same Remote
//! Control identity. On 2026-11-03 one did, from inside a claude's shell, and
//! with that claude's environment: it pointed the daemon's Live Token proxy at
//! the proxy itself (every claude got 502), fought the real app for the relay
//! link (the remote badge flipped every second) and rewrote the session lists.
//!
//! So the first app holds an exclusive `flock` on `<home>/app.lock` for its
//! whole life, and a second launch asks it to come to the front and exits
//! before it touches anything else. The lock dies with the process, so a crash
//! never leaves it stale.
//!
//! A restart (`request_restart`: an update, a host update ordered from the
//! Mac) starts the new app while the old one is still tearing down. The old
//! one says so in `<home>/restarting` ([`mark_restart`]), and the new one
//! waits for the lock instead of giving up.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Written by an app that is about to restart (its pid), so the app it starts
/// waits for the lock instead of exiting.
const RESTART_FILE: &str = "restarting";
/// How long a restarted app waits for the old one to let go.
const RESTART_WAIT: Duration = Duration::from_secs(60);
/// What a second launch touches to bring the running app to the front.
const FOCUS_FILE: &str = "focus-request";

static LOCK: OnceLock<File> = OnceLock::new();

fn lock_path(home: &std::path::Path) -> PathBuf {
    home.join("app.lock")
}

pub fn focus_path() -> PathBuf {
    mulpex_core::mulpex_home().join(FOCUS_FILE)
}

fn try_lock(f: &File) -> bool {
    // SAFETY: flock on a file descriptor we own.
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Take the home's lock, or hand over to the app that holds it and exit.
/// Called first thing in `run()`.
pub fn claim_or_exit() {
    let home = mulpex_core::mulpex_home();
    let _ = std::fs::create_dir_all(&home);
    let path = lock_path(&home);
    let f = match OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path) {
        Ok(f) => f,
        // Can't even open it (a read-only home?): run as before rather than
        // refuse to start.
        Err(e) => {
            eprintln!("mulpex: {}: {e} — not guarding against a second launch", path.display());
            return;
        }
    };
    let started = Instant::now();
    loop {
        if try_lock(&f) {
            break;
        }
        // The holder said it is restarting (and is still the holder): wait
        // for it to finish quitting.
        if restarting(&home, &path) && started.elapsed() < RESTART_WAIT {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let _ = std::fs::write(home.join(FOCUS_FILE), std::process::id().to_string());
        eprintln!(
            "Mulpex is already running for {} — brought it to the front. \
             (`mulpex --help` for the command-line options.)",
            home.display()
        );
        std::process::exit(0);
    }
    // Who holds it: what a restart marker is checked against, and what a
    // person looking at the file wants to know.
    let mut w = &f;
    let _ = f.set_len(0);
    let _ = writeln!(w, "{}", std::process::id());
    let _ = std::fs::remove_file(home.join(RESTART_FILE));
    let _ = LOCK.set(f);
}

/// Whether the lock's holder announced a restart: the marker names the pid
/// written in the lock file, and is recent.
fn restarting(home: &std::path::Path, lock: &std::path::Path) -> bool {
    let read = |p: PathBuf| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string());
    let (Some(marked), Some(holder)) = (read(home.join(RESTART_FILE)), read(lock.to_path_buf())) else {
        return false;
    };
    let fresh = std::fs::metadata(home.join(RESTART_FILE))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < RESTART_WAIT);
    fresh && !marked.is_empty() && marked == holder
}

/// The app is about to restart: the one it starts must wait for this one's
/// lock rather than take itself for a second launch. A file, not the
/// environment: on macOS the new app is started by LaunchServices (`open -n`)
/// and inherits nothing from us.
pub fn mark_restart() {
    let _ = std::fs::write(
        mulpex_core::mulpex_home().join(RESTART_FILE),
        std::process::id().to_string(),
    );
}

/// Bring the window to the front whenever a second launch asks.
pub fn watch_focus(app: tauri::AppHandle) {
    use tauri::Manager;
    // One left over from before this app started is not a request to it.
    let _ = std::fs::remove_file(focus_path());
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(400));
        if std::fs::remove_file(focus_path()).is_err() {
            continue;
        }
        if let Some(w) = app.webview_windows().values().next().cloned() {
            let _ = w.unminimize();
            let _ = w.show();
            let _ = w.set_focus();
        }
    });
}

/// Strip what a Mulpex child's environment carries before anything reads it.
/// An app started from inside a claude's shell (a claude restarting Mulpex,
/// say) inherits that claude's hub identity and its Live Token base URL; the
/// first would give the app a hub identity it is not, and the second became
/// the "upstream" the app handed the daemon's proxy — the proxy then called
/// itself until it ran out of file descriptors.
pub fn scrub_inherited_env() {
    for k in ["MULPEX_INSTANCE_ID", "MULPEX_STATE_DIR", "CLAUDE_CODE_CHILD_SESSION", "CLAUDECODE"] {
        std::env::remove_var(k);
    }
    if std::env::var("ANTHROPIC_BASE_URL").is_ok_and(|u| is_live_token_url(&u)) {
        std::env::remove_var("ANTHROPIC_BASE_URL");
    }
}

/// A Live Token proxy address as Mulpex hands it to a claude:
/// `http://127.0.0.1:<port>/<32 hex secret>[/c/<route>]`.
pub fn is_live_token_url(u: &str) -> bool {
    let Some(rest) = u.trim().strip_prefix("http://127.0.0.1:") else { return false };
    let mut parts = rest.splitn(3, '/');
    let port_ok = parts.next().is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let secret_ok = parts.next().is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()));
    port_ok && secret_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_live_token_url() {
        assert!(is_live_token_url("http://127.0.0.1:34693/43e170caaa5efe2cff20f09709501ade"));
        assert!(is_live_token_url(
            "http://127.0.0.1:34693/43e170caaa5efe2cff20f09709501ade/c/d249d139a1b63e73-2"
        ));
        assert!(!is_live_token_url("https://api.anthropic.com"));
        // A gateway the user runs on loopback is theirs, not ours.
        assert!(!is_live_token_url("http://127.0.0.1:4000"));
        assert!(!is_live_token_url("http://127.0.0.1:4000/v1"));
        assert!(!is_live_token_url("http://localhost:34693/43e170caaa5efe2cff20f09709501ade"));
    }

    #[test]
    fn a_second_lock_on_the_same_file_fails_until_the_first_is_gone() {
        let dir = std::env::temp_dir().join(format!("mulpex-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = lock_path(&dir);
        let open = || OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&p).unwrap();
        let a = open();
        assert!(try_lock(&a));
        let b = open();
        assert!(!try_lock(&b), "a second open file description must not get the lock");
        drop(a);
        assert!(try_lock(&b), "the lock dies with its holder");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
