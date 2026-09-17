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
/// **Status dots** — `UserPromptSubmit` → working; `PostToolUse` → working
/// *unless a dialog is waiting and this is not that dialog's own tool*, because a
/// background agent's tool calls fire `PostToolUse` in the parent session and used
/// to paint over the red (see `hook::write_working_unless_a_dialog_waits`);
/// `PreToolUse[AskUserQuestion]` → needs (via `<helper> hook askq`, which also
/// hands the turn's transcript path to the Explainer so the pending question is
/// explained while it sits on screen — it replaced a bare `printf needs` when
/// the Explainer needed a hook there);
/// `PreToolUse[ExitPlanMode]` → needs the same way (via `<helper> hook plan`),
/// which also hands the transcript path to the Explainer; `Stop` → waiting (via
/// the helper, which also releases locks and hands the finished turn to the
/// Explainer); the `permission_prompt`/`idle_prompt`
/// notifications → waiting, or working while background work or a compaction is
/// outstanding, and **never** needs — red is reserved for the two `PreToolUse`
/// matchers above, the only states where the instance is genuinely holding
/// something up for the user. A notification also never *clears* needs, because
/// the plan dialog fires its own `permission_prompt` ~6 s after `ExitPlanMode`.
/// See `hook::notification`. `PreCompact` → working and
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
      { "matcher": "AskUserQuestion", "hooks": [ { "type": "command", "command": "\"__MULPEX_BIN__\" hook askq" } ] },
      { "matcher": "ExitPlanMode", "hooks": [ { "type": "command", "command": "\"__MULPEX_BIN__\" hook plan" } ] },
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
