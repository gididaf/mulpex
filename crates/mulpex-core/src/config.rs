//! Static config templates written into the per-run scratch dir and handed to
//! each `claude` child via `--settings` / `--mcp-config`.
//!
//! Both key their per-instance identity off the inherited `MULPEX_*` env, so one
//! static file serves every instance. The only substitution is `__MULPEX_BIN__`,
//! which the app replaces with the absolute path of the `mulpex-helper` binary
//! before writing the files (the child `claude` processes invoke it as
//! `<helper> hook <event>` and `<helper> mcp`).

/// Claude Code settings injected per session via `--settings`, wiring lifecycle
/// hooks for both the status dots and the file-locking coordinator.
///
/// **Status dots** — `UserPromptSubmit`/`PostToolUse` → working;
/// `PreToolUse[AskUserQuestion]` → needs; `Stop` → waiting (via the helper, which
/// also releases locks); the `permission_prompt`/`idle_prompt` notifications →
/// needs, via the helper rather than a bare `printf` because `idle_prompt` fires
/// 60 s after every turn end *even when a background agent the instance launched
/// is still running* — see `hook::notification`. `PreCompact` → working and
/// `SessionStart[source=compact]` → back to a real status, because compaction
/// runs for minutes firing nothing else and `/compact` does not even fire
/// `UserPromptSubmit`. The sidebar polls these one-word state files.
///
/// **File-locking coordinator** — the `PreToolUse` matchers for the edit tools
/// (`Read|Write|Edit|MultiEdit|NotebookEdit`) and for `Bash` invoke
/// `<helper> hook pretooluse`, a per-file semaphore that waits then proceeds
/// (never a hard deny). `Stop` runs `<helper> hook stop` to release per-turn
/// locks + write `waiting`. See `hook.rs`.
pub const HOOK_SETTINGS_JSON: &str = r#"{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [ { "type": "command", "command": "\"__MULPEX_BIN__\" hook userpromptsubmit" } ] }
    ],
    "PostToolUse": [
      { "hooks": [ { "type": "command", "command": "\"__MULPEX_BIN__\" hook posttooluse" } ] }
    ],
    "PreToolUse": [
      { "matcher": "AskUserQuestion", "hooks": [ { "type": "command", "command": "printf needs > \"$MULPEX_STATE_DIR/$MULPEX_INSTANCE_ID\"" } ] },
      { "matcher": "Read|Write|Edit|MultiEdit|NotebookEdit", "hooks": [ { "type": "command", "command": "\"__MULPEX_BIN__\" hook pretooluse", "timeout": 280 } ] },
      { "matcher": "Bash", "hooks": [ { "type": "command", "command": "\"__MULPEX_BIN__\" hook pretooluse" } ] }
    ],
    "Notification": [
      { "matcher": "permission_prompt|idle_prompt", "hooks": [ { "type": "command", "command": "\"__MULPEX_BIN__\" hook notification" } ] }
    ],
    "Stop": [
      { "hooks": [ { "type": "command", "command": "\"__MULPEX_BIN__\" hook stop" } ] }
    ],
    "PreCompact": [
      { "hooks": [ { "type": "command", "command": "\"__MULPEX_BIN__\" hook precompact" } ] }
    ],
    "SessionStart": [
      { "hooks": [ { "type": "command", "command": "\"__MULPEX_BIN__\" hook sessionstart" } ] }
    ]
  }
}
"#;

/// `--mcp-config` registering the coordination-hub MCP server (`<helper> mcp`).
/// One static file serves every instance because the server reads its identity
/// from the inherited `MULPEX_*` env. See `mcp.rs`.
pub const MCP_CONFIG_JSON: &str = r#"{
  "mcpServers": {
    "mulpex": { "type": "stdio", "command": "__MULPEX_BIN__", "args": ["mcp"] }
  }
}
"#;
