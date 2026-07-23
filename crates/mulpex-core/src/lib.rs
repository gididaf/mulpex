//! Headless coordination core shared by the Mulpex desktop app and the
//! `mulpex-helper` binary.
//!
//! This crate carries everything that is UI-independent and was ported verbatim
//! from the original terminal-UI mulpex: the file-locking hook coordinator
//! (`hook`), the inner MCP coordination-hub server (`mcp`), per-project session
//! persistence (`persist`), and the static `--settings` / `--mcp-config`
//! templates (`config`). It links no GUI/terminal dependencies so the helper
//! binary the child `claude` processes exec stays tiny and fast.

pub mod config;
pub mod hook;
pub mod mcp;
pub mod persist;

/// Prefix marker on any prompt Mulpex itself injects into a `claude` session's
/// stdin (currently the hub-listener bootstrap). The `UserPromptSubmit` hook keys
/// off it to avoid overwriting the sidebar task with our plumbing text. The
/// injected prompt in `src-tauri`'s `pty.rs` must begin with this exact string.
pub const MULPEX_SENTINEL: &str = "[mulpex:hub]";
