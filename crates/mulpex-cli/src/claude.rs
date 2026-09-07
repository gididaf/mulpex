//! Finding the user's `claude` binary, and the environment a child must not get.
//!
//! Almost all of `src-tauri/src/claude_bin.rs` exists for one macOS fact: a
//! Finder-launched bundle inherits LaunchServices' bare environment, with no
//! PATH, no TERM and none of the shell rc exports — above all
//! `CLAUDE_CODE_OAUTH_TOKEN`, whose absence shows up as "Not logged in". That
//! whole login-shell probe is **unnecessary here**: `mpx` is started from a login
//! shell and `claude` inherits the real environment for free, and a rotated token
//! is picked up by any new window with no re-probe.
//!
//! What survives is the opposite direction — the **scrub**. `mpx` may itself be
//! run from inside a Mulpex claude's Bash tool, where `MULPEX_INSTANCE_ID` and
//! `MULPEX_STATE_DIR` are set; handing those to a child would corrupt the hub.

use std::path::PathBuf;

/// Variables removed from every child's inherited environment.
///
/// `CLAUDE_CODE_CHILD_SESSION` is the dangerous one: a child that inherits it
/// silently stops saving its transcript, so nothing looks wrong until the *next*
/// launch cannot restore the session.
pub const UNSET: &[&str] = &[
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_ENTRYPOINT",
    "MULPEX_INSTANCE_ID",
    "MULPEX_STATE_DIR",
    "MULPEX_PROJECT_DIR",
];

/// Resolve `claude` to an absolute path, so the spec is self-describing and a
/// pane's argv says exactly what ran.
///
/// `~/.local/bin` is checked explicitly because it is the default install
/// location and is genuinely missing from some server PATHs.
pub fn resolve() -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join("claude");
            if is_executable(&cand) {
                return Some(cand);
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    for rel in [".local/bin/claude", ".claude/local/claude"] {
        let cand = home.join(rel);
        if is_executable(&cand) {
            return Some(cand);
        }
    }
    None
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The `mulpex-helper` binary, which `claude` execs by absolute path from
/// `settings.json` and `mcp.json`. It sits beside `mpx` because both are
/// workspace members and `cargo` puts them in the same directory.
pub fn helper_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mulpex-helper")))
        .unwrap_or_else(|| PathBuf::from("mulpex-helper"))
}
