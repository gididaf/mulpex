//! Finding the user's `claude` binary — and a usable `PATH` — from a GUI app.
//!
//! This exists because of one macOS fact: a bundle launched from Finder does
//! **not** inherit a login shell's environment. LaunchServices hands it the
//! bare default `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`), so the standard
//! Claude install location (`~/.local/bin/claude`) is invisible and a bare
//! `CommandBuilder::new("claude")` fails to spawn. Under `tauri dev` the
//! terminal's `PATH` is inherited and everything works, which is exactly why
//! this only ever bit the bundled `.app`.
//!
//! So we reconstruct the user's real `PATH` once per run:
//!   1. ask their login shell for it (`$SHELL -lic 'echo …$PATH…'`), which is
//!      the only way to pick up version managers (nvm/fnm/asdf/volta) and
//!      custom npm prefixes;
//!   2. append the well-known install dirs as a fallback, in case the probe
//!      fails, times out, or the shell is exotic.
//!
//! The merged `PATH` is used both to locate `claude` (we spawn it by absolute
//! path) and as the child's `PATH`, so tools invoked *inside* Claude's Bash
//! sessions (`node`, `git`, Homebrew binaries) resolve the way they do in the
//! user's own terminal.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long we let the login shell run before giving up on it. Some rc files
/// are slow (or block outright); the fallback list covers us if this trips.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Sentinels around the probed `PATH`, so rc-file chatter (banners, `nvm` noise,
/// motd) can't be mistaken for the value.
const MARK_BEGIN: &str = "__MULPEX_PATH_BEGIN__";
const MARK_END: &str = "__MULPEX_PATH_END__";

/// Install locations to try even when the shell probe yields nothing useful.
/// Relative entries are resolved against `$HOME`.
const FALLBACK_DIRS: &[&str] = &[
    "~/.local/bin",       // official Claude Code installer
    "~/.claude/local",    // legacy/local install
    "/opt/homebrew/bin",  // Homebrew (Apple Silicon)
    "/usr/local/bin",     // Homebrew (Intel) / manual installs
    "~/.bun/bin",         // bun global
    "~/.volta/bin",       // volta shim
    "~/.npm-global/bin",  // custom npm prefix
    "~/node_modules/.bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn expand_tilde(dir: &str) -> Option<PathBuf> {
    match dir.strip_prefix("~/") {
        Some(rest) => home().map(|h| h.join(rest)),
        None => Some(PathBuf::from(dir)),
    }
}

/// Ask the user's login+interactive shell for its `PATH`.
///
/// Login *and* interactive matters: zsh reads `.zprofile` on login but only
/// reads `.zshrc` — where most people actually extend `PATH` — when
/// interactive. Killed at `PROBE_TIMEOUT` so a hanging rc file can't wedge
/// startup.
fn probe_login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let script = format!("printf '%s%s%s\\n' '{MARK_BEGIN}' \"$PATH\" '{MARK_END}'");

    let mut child = Command::new(&shell)
        .args(["-lic", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }

    let out = child.wait_with_output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let start = text.find(MARK_BEGIN)? + MARK_BEGIN.len();
    let rest = &text[start..];
    let end = rest.find(MARK_END)?;
    let path = rest[..end].trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// The `PATH` Mulpex should use for `claude` and hand to it: the login shell's
/// `PATH` first (user intent, version managers), then whatever we were launched
/// with, then the known install dirs. Order is preserved and duplicates are
/// dropped, so the user's own precedence wins.
pub fn merged_path() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        let mut dirs: Vec<String> = Vec::new();
        let mut push = |d: String| {
            if !d.is_empty() && !dirs.contains(&d) {
                dirs.push(d);
            }
        };

        if let Some(probed) = probe_login_shell_path() {
            for d in probed.split(':') {
                push(d.to_string());
            }
        }
        if let Ok(current) = std::env::var("PATH") {
            for d in current.split(':') {
                push(d.to_string());
            }
        }
        for d in FALLBACK_DIRS {
            if let Some(p) = expand_tilde(d) {
                push(p.to_string_lossy().into_owned());
            }
        }
        dirs.join(":")
    })
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &std::path::Path) -> bool {
    p.is_file()
}

/// Absolute path of the user's `claude`, or `None` if it isn't installed
/// anywhere we can see. Resolved once — the answer can't change mid-run in a
/// way we'd want to act on, and this keeps the shell probe to a single spawn.
pub fn resolve_claude() -> Option<&'static PathBuf> {
    static RESOLVED: OnceLock<Option<PathBuf>> = OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            merged_path()
                .split(':')
                .map(|d| PathBuf::from(d).join("claude"))
                .find(|c| is_executable(c))
        })
        .as_ref()
}
