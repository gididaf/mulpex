//! The spawn fingerprint: whether a running claude has what this build gives a
//! new one.
//!
//! A claude keeps, for its whole life, what it got at spawn: the appended
//! system prompt, the settings/mcp/plugin files it read once, and its
//! long-lived `helper mcp` and `helper listen` processes (the old inode, after
//! an update renamed a new helper over the path). An update the daemon survives
//! does not touch any of that, so `build.rs` hashes the code that defines it and
//! bakes it in as `MULPEX_SPAWNPRINT`. Each claude records the value it was
//! spawned with in its daemon meta; a mismatch is the sidebar's ↻ badge
//! (docs/ptyd.md).
//!
//! Add a file here when it changes something a running claude froze at spawn.
//! Hooks and the statusline are not frozen: each call execs the current helper.
//!
//! This file is shared with `build.rs` (`#[path]`), so it must stay plain std.

/// Relative to `src-tauri/`, like `wire::FILES`.
pub const FILES: &[&str] = &[
    "../crates/mulpex-core/src/rules.rs",
    "../crates/mulpex-core/src/config.rs",
    "../crates/mulpex-core/src/mcp.rs",
    "../crates/mulpex-core/src/listen.rs",
    "../crates/mulpex-core/src/registry.rs",
];
