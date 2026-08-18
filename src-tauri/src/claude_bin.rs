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
//! `PATH` was only the first casualty. The same fact costs the child **every
//! other variable the user's rc files export** — most sharply
//! `CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_API_KEY`, without which every
//! instance opens on *"Not logged in · Please run /login"* even though the
//! user's terminal is perfectly authenticated. (A ⌘⇧T terminal is immune: it
//! runs `$SHELL -l -i`, which sources the rc files itself. Only ⌘T instances
//! break, and never under `tauri dev`.)
//!
//! So we reconstruct the user's real **environment** once per run:
//!   1. ask their login shell for it (`$SHELL -lic '… env -0 …'`), which is the
//!      only way to see rc-file exports at all — and the only way to pick up
//!      version managers (nvm/fnm/asdf/volta) and custom npm prefixes;
//!   2. for `PATH` specifically, append the well-known install dirs as a
//!      fallback, in case the probe fails, times out, or the shell is exotic.
//!
//! Everything from that environment is forwarded to the child except what
//! Mulpex must own itself (see `DENY` / `DENY_PREFIX`), so a Mulpex `claude`
//! behaves like one the user launched from their own terminal — its Bash tool
//! finds `node`/`git`/Homebrew *and* whatever tokens its work needs.
//!
//! **Freshness.** The environment is re-probed on a background thread every
//! `ENV_REFRESH_INTERVAL`, because auth tokens rotate (a refresher daemon
//! rewriting a token file is a common setup) and Mulpex is left open for days.
//! `PATH` deliberately does *not* follow: it is pinned at the first probe,
//! since `resolve_claude` hands out an absolute path derived from it and a
//! `PATH` that disagreed with the binary already chosen is worse than a stale
//! one.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// How long we let the login shell run before giving up on it. Some rc files
/// are slow (or block outright); the fallback list covers us if this trips.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Sentinels around the probed environment, so rc-file chatter (banners, `nvm`
/// noise, motd) can't be mistaken for it.
const MARK_BEGIN: &str = "__MULPEX_ENV_BEGIN__";
const MARK_END: &str = "__MULPEX_ENV_END__";

/// How often the login environment is re-probed in the background. Auth tokens
/// rotate; a Mulpex left open for days must not keep spawning instances with the
/// token that was current at launch.
pub const ENV_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Variables the child must NOT inherit from the login shell, because Mulpex
/// sets them itself or they describe the wrong thing:
///
/// * `PATH` — replaced by [`merged_path`], which is that value *plus* fallbacks.
/// * `TERM`/`COLORTERM` — the child talks to xterm.js, not to whatever terminal
///   (if any) the probe ran under; `pty.rs` declares that emulator explicitly.
/// * the terminal-identity vars — same reason: under `tauri dev` the probe
///   inherits the launching terminal's, and passing them on would describe a
///   window the child isn't in.
/// * `CLAUDE_CODE_CHILD_SESSION`/`ENTRYPOINT` — a Mulpex instance is a genuine
///   top-level session; inheriting the child marker silently disables transcript
///   saving and breaks `--resume` (`pty.rs` strips these too, belt and braces).
/// * `IS_SANDBOX` and everything `MULPEX_*` — hub identity, assigned per session.
///   If Mulpex was itself launched from inside a Mulpex claude, the login shell
///   inherits that instance's id and would hand it to every child.
/// * the shell's own bookkeeping (`_`, `SHLVL`, `PWD`, `OLDPWD`, and the `-c`
///   script text, which contains our own sentinels).
const DENY: &[&str] = &[
    "PATH",
    "TERM",
    "COLORTERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "COLUMNS",
    "LINES",
    "WINDOWID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_ENTRYPOINT",
    "IS_SANDBOX",
    "_",
    "SHLVL",
    "PWD",
    "OLDPWD",
    "ZSH_EXECUTION_STRING",
    "BASH_EXECUTION_STRING",
];

/// Prefix form of [`DENY`], for families rather than names.
const DENY_PREFIX: &[&str] = &["MULPEX_", "ITERM_", "KITTY_"];

fn forwardable(key: &str) -> bool {
    !DENY.contains(&key) && !DENY_PREFIX.iter().any(|p| key.starts_with(p))
}

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

/// One `KEY=VALUE` pair as the login shell reported it.
pub type EnvPair = (String, String);

/// The login shell's environment, re-probed in the background. Held behind an
/// `RwLock` rather than a `OnceLock` precisely so it *can* be replaced —
/// see the freshness note at the top of this file.
static LOGIN_ENV: RwLock<Option<Arc<Vec<EnvPair>>>> = RwLock::new(None);

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

/// Ask the user's login+interactive shell for its whole environment.
///
/// Login *and* interactive matters: zsh reads `.zprofile` on login but only
/// reads `.zshrc` — where most people actually export tokens and extend
/// `PATH` — when interactive. Killed at `PROBE_TIMEOUT` so a hanging rc file
/// can't wedge startup.
///
/// `env -0` rather than `env`, so a value containing a newline (or anything
/// else) survives the round trip; the output is parsed as **bytes** and only
/// the individual entries are required to be UTF-8, so one exotic variable
/// can't discard the rest. The end sentinel is matched with `rfind` because a
/// shell that exports its own `-c` script text (`ZSH_EXECUTION_STRING`) would
/// otherwise plant a copy of it in the middle of the dump.
pub(crate) fn probe_login_shell_env() -> Option<Vec<EnvPair>> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let script =
        format!("printf '%s' '{MARK_BEGIN}'; /usr/bin/env -0; printf '%s' '{MARK_END}'");

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
    let begin = find(&out.stdout, MARK_BEGIN.as_bytes())? + MARK_BEGIN.len();
    let rest = &out.stdout[begin..];
    let body = &rest[..rfind(rest, MARK_END.as_bytes())?];

    let mut vars: Vec<EnvPair> = Vec::new();
    for entry in body.split(|b| *b == 0) {
        let Ok(text) = std::str::from_utf8(entry) else {
            continue;
        };
        let Some((k, v)) = text.split_once('=') else {
            continue;
        };
        if !k.is_empty() {
            vars.push((k.to_string(), v.to_string()));
        }
    }
    // An empty result means the probe didn't really work (no shell has an empty
    // environment), and must not be cached as if it had.
    (!vars.is_empty()).then_some(vars)
}

/// The login shell's environment, probing on first use.
///
/// A failed probe caches an empty vec rather than retrying on every call: the
/// fallbacks below cover `PATH`, and re-spawning a 5-second shell probe per
/// session would be worse than the thing it's recovering from. The background
/// refresher gets the next chance.
fn login_env() -> Arc<Vec<EnvPair>> {
    if let Some(v) = LOGIN_ENV.read().unwrap().as_ref() {
        return v.clone();
    }
    let probed = Arc::new(probe_login_shell_env().unwrap_or_default());
    let mut w = LOGIN_ENV.write().unwrap();
    // Another thread may have won the race while we probed; either answer is
    // equally good, so keep whichever landed first.
    w.get_or_insert(probed).clone()
}

/// Re-probe and replace the cached environment. A failed probe **keeps the last
/// good one** — a stale token beats no token, and beats a transient rc-file
/// hiccup silently logging every future instance out.
pub fn refresh_login_env() {
    if let Some(vars) = probe_login_shell_env() {
        *LOGIN_ENV.write().unwrap() = Some(Arc::new(vars));
    }
}

/// The login-shell variables a PTY child should inherit — everything except what
/// Mulpex owns itself (`DENY`/`DENY_PREFIX`).
pub fn forwarded_env() -> Vec<EnvPair> {
    login_env()
        .iter()
        .filter(|(k, _)| forwardable(k))
        .cloned()
        .collect()
}

/// Prime the environment + `claude` lookup, then keep the environment fresh.
///
/// Called once from `setup()` on its own thread: the first pass is what stops
/// the first project open (and the `claude_status` probe) from paying the shell
/// spawn inline, and the loop is what keeps a rotating auth token live in a
/// Mulpex that stays open for days.
pub fn warm_and_refresh() {
    std::thread::spawn(|| {
        let _ = resolve_claude();
        loop {
            std::thread::sleep(ENV_REFRESH_INTERVAL);
            refresh_login_env();
        }
    });
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

        if let Some(probed) = login_env().iter().find(|(k, _)| k == "PATH") {
            for d in probed.1.split(':') {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The denylist is the whole safety story of forwarding a foreign
    /// environment wholesale, so pin both directions: the thing this exists for
    /// gets through, and the things Mulpex must own do not. `MULPEX_INSTANCE_ID`
    /// is the sharpest — Mulpex launched from inside a Mulpex claude would
    /// otherwise hand that instance's hub identity to every child it spawns.
    #[test]
    fn the_denylist_keeps_mulpex_owned_variables_out_and_lets_auth_through() {
        for k in [
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "GH_TOKEN",
            "LANG",
            "SSH_AUTH_SOCK",
        ] {
            assert!(forwardable(k), "{k} should reach the child");
        }
        for k in [
            "PATH",
            "TERM",
            "COLORTERM",
            "IS_SANDBOX",
            "MULPEX_INSTANCE_ID",
            "MULPEX_STATE_DIR",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_ENTRYPOINT",
            "SHLVL",
            "_",
        ] {
            assert!(!forwardable(k), "{k} must not be inherited");
        }
    }
}
