//! Headless coordination core shared by the Mulpex desktop app and the
//! `mulpex-helper` binary.
//!
//! This crate carries everything that is UI-independent and was ported verbatim
//! from the original terminal-UI mulpex: the file-locking hook coordinator
//! (`hook`), the inner MCP coordination-hub server (`mcp`), per-project session
//! persistence (`persist`), the shell-terminal transcript format the app writes
//! and the helper reads (`termlog`), the workspace registry + address grammar
//! that let an instance message one in another open project (`registry`), and the
//! static `--settings` / `--mcp-config` templates (`config`). It links no
//! GUI/terminal dependencies so the helper binary the child `claude` processes
//! exec stays tiny and fast.

pub mod config;
pub mod hook;
pub mod mcp;
pub mod persist;
pub mod registry;
pub mod remote;
pub mod termlog;

/// Prefix marker on any prompt Mulpex itself injects into a `claude` session's
/// stdin (currently the hub-listener bootstrap). The `UserPromptSubmit` hook keys
/// off it to avoid overwriting the sidebar task with our plumbing text. The
/// injected prompt in `src-tauri`'s `pty.rs` must begin with this exact string.
pub const MULPEX_SENTINEL: &str = "[mulpex:hub]";

/// Where an instance asks Mulpex to name its own sidebar row (`hub_set_name`):
/// one file per instance under `<state_dir>/namereq/`, holding the label. Written
/// by the helper, consumed by the app's poll loop (`Core::process_name_requests`).
pub fn name_request_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(NAMEREQ_DIR).join(id.to_string())
}

/// The flag that records "this row has a name, stop asking" — read by the
/// `UserPromptSubmit` hook (`hook::instance_named`), written by whichever side
/// settled it: the instance naming itself, or the app when the *user* renames the
/// row (⌘R) or a restored session comes back already named.
pub fn named_flag_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(NAMED_DIR).join(id.to_string())
}

/// These two live here, next to `MULPEX_SENTINEL`, for the same reason: they are
/// a contract between two *processes*, so a copy in each would be a contract that
/// can silently drift out of agreement.
pub const NAMEREQ_DIR: &str = "namereq";
pub const NAMED_DIR: &str = "named";
