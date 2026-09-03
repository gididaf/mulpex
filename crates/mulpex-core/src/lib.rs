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

/// Mulpex's persistent home: recents/open-project lists and the per-project
/// session stores. A **debug build uses `~/.mulpex-dev`** so `tauri dev` never
/// sees (or rewrites) the live app's projects and sessions — the two builds run
/// side by side without contaminating each other. `MULPEX_HOME` overrides both
/// ways (point a dev build at the real data, or a release build at a sandbox).
/// Release-script assets (`~/.mulpex/signing`, `updater.key`) are not read by
/// the app and deliberately stay on the literal path.
pub fn mulpex_home() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("MULPEX_HOME") {
        if !dir.is_empty() {
            return std::path::PathBuf::from(dir);
        }
    }
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(if cfg!(debug_assertions) { ".mulpex-dev" } else { ".mulpex" })
}

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

/// Where the `Stop` hook hands a finished turn to the Explainer: one file per
/// instance under `<state_dir>/explainreq/`, holding the session's transcript
/// path (from the Stop payload's `transcript_path` — measured present,
/// 2026-08-30). Written by the helper, consumed-and-deleted by the app's poll
/// loop (`Core::take_explain_requests`); overwriting between polls is the
/// latest-wins coalescing.
pub fn explain_request_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(EXPLAINREQ_DIR).join(id.to_string())
}

/// Where the `AskUserQuestion` PreToolUse hook hands the pending questions to
/// the Explainer: one file per instance under `<state_dir>/explainq/`, holding
/// the tool's `tool_input` JSON (`questions` array). Written by the helper
/// (`hook askq`), consumed-and-deleted by the app's poll loop.
pub fn question_request_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(EXPLAINQ_DIR).join(id.to_string())
}

/// Where the `ExitPlanMode` PreToolUse hook hands the pending plan to the
/// Explainer: one file per instance under `<state_dir>/explainplan/`, holding
/// the tool's `tool_input` JSON (its `plan` field is the plan as markdown).
/// Written by the helper (`hook plan`), consumed-and-deleted by the app's poll
/// loop. Measured 2026-09-01: `PreToolUse[ExitPlanMode]` fires ~6 s *before*
/// the approval dialog paints, so the explanation is already in flight while
/// the user is still reading the plan.
pub fn plan_request_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(EXPLAINPLAN_DIR).join(id.to_string())
}

/// A spawned child's task-delivery verdict: `<state_dir>/spawning/<id>`, holding
/// `pending` (created, not started yet), `failed` (never began a turn) or
/// `partial` (began a turn, but on text that is not what Mulpex sent). Absent
/// means delivered and verified. Written by the app's watchdog and by the child's
/// own `UserPromptSubmit` hook; read by `mcp::delivery_of` to answer `hub_spawn`.
pub fn spawn_delivery_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(SPAWNING_DIR).join(id.to_string())
}

/// The exact prompt Mulpex put on a spawned child's command line, so the child's
/// own hook can check what it ACTUALLY received against it.
///
/// This exists because the previous delivery mechanism failed silently. The task
/// was typed into the child's TUI, `claude` capped the paste at one tty read
/// (measured: 1022 characters, whatever the input size), and every signal said
/// success — a turn had genuinely started, just on the first kilobyte of the
/// brief. The `UserPromptSubmit` hook is the ONLY place in the system that sees
/// what the child really got, so it is the only place that can tell delivery from
/// the appearance of delivery. Delivery is argv now and cannot truncate, but the
/// check stays: it is what turns a future silent corruption into a loud one.
pub fn spawn_expected_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(SPAWNING_DIR).join(format!("{id}.expected"))
}

/// The flag that says "this child was resumed in place (⌘⇧R), so the orphaned
/// background task it is about to be told about is *expected*". Written by the app
/// in `Core::restart_instance`, consumed-and-deleted by the `UserPromptSubmit`
/// hook (`hook::take_resumed_in_place`).
///
/// It exists because the two ways a `claude` gets `--resume`d need opposite
/// answers to the same notification. An app launch builds a **fresh** `state_dir`,
/// so the inbox is empty and the wake carries nothing to act on — it is pure
/// restart noise and is blocked. ⌘⇧R reuses the **same** `state_dir` and
/// deliberately keeps the inbox while clearing `armed/<id>`, so that same wake is
/// the only thing that re-arms the listener and drains any mail waiting for it.
/// Blocking it there would leave the restarted instance unarmed and its mail
/// undelivered, with nothing anywhere to say so.
pub fn resumed_in_place_path(state_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    state_dir.join(RESUMED_DIR).join(id.to_string())
}

/// These live here, next to `MULPEX_SENTINEL`, for the same reason: they are
/// a contract between two *processes*, so a copy in each would be a contract that
/// can silently drift out of agreement.
pub const NAMEREQ_DIR: &str = "namereq";
pub const RESUMED_DIR: &str = "resumed";
pub const SPAWNING_DIR: &str = "spawning";
pub const NAMED_DIR: &str = "named";
pub const EXPLAINREQ_DIR: &str = "explainreq";
pub const EXPLAINQ_DIR: &str = "explainq";
pub const EXPLAINPLAN_DIR: &str = "explainplan";
