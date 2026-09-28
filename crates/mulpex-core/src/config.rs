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
/// `PreToolUse[AskUserQuestion]` → needs (via `<helper> hook askq`);
/// `PreToolUse[ExitPlanMode]` → needs the same way (via `<helper> hook plan`);
/// `Stop` → waiting (via the helper, which also releases locks); the
/// `permission_prompt`/`idle_prompt`
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

/// The manifest of the one-purpose plugin every `claude` is launched with
/// (`--plugin-dir <state_dir>/plugin`).
///
/// Nothing is installed and nothing is fetched: this is a folder shape Claude
/// Code reads, generated into the scratch dir beside `settings.json` and
/// `mcp.json` and rewritten before every spawn for the same three-day-fuse
/// reason. The plugin carries the monitor below and the `explain` skill.
///
/// The name is also the skill's namespace (`/mulpex:explain`), which is why it
/// is `mulpex` and not something longer.
pub const PLUGIN_MANIFEST_JSON: &str = r#"{
  "name": "mulpex",
  "description": "Mulpex: the hub inbox listener, armed by the host, and /explain.",
  "version": "1.0.0"
}
"#;

/// `skills/explain/SKILL.md`: `/explain` makes the claude *itself* explain the
/// conversation in very simple Hebrew — in the pane, in its own turn, with the
/// whole context it already has. It replaced the Explainer panel's side
/// conversation. `disable-model-invocation` keeps it user-only: a claude that
/// decided on its own to explain itself would be noise.
///
/// The formatting rules are rendering rules, not taste. The pane picks each
/// row's direction from its first strong character (`unicode-bidi: plaintext`,
/// docs/rendering.md), so a row that opens with an English word flips to LTR,
/// and English mixed into a Hebrew row reorders the words around it.
pub const EXPLAIN_SKILL_MD: &str = r#"---
name: explain
description: Explain the conversation so far to the user in very simple Hebrew, with no technical words.
disable-model-invocation: true
---

המשתמש ביקש שתסביר לו מה קורה עכשיו. הוא לא מתכנת. דבר אליו כמו אל חבר שלא מבין בתכנות.

אל תפעיל שום כלי. אל תקרא קבצים. תסביר רק ממה שכבר קרה בשיחה הזאת.
אם המשתמש כתב מילים אחרי הפקודה, הן אומרות לך על מה להתמקד. ענה על זה, באותו מבנה.

כתוב בדיוק ארבעה חלקים, בסדר הזה, כל אחד עם הכותרת המודגשת שלו:

**מה עשיתי**
משפט אחד או שניים על מה שעשיתי בתשובה האחרונה שלי.

**למה**
משפט אחד: למה עשיתי את זה.

**מה אני צריך ממך**
בדיוק מה אני מחכה לו ממך: החלטה, תשובה או בדיקה. אם אני לא צריך כלום, כתוב: כלום כרגע.

**איפה אנחנו**
משפט אחד על התמונה הגדולה: על מה אנחנו עובדים, מה כבר גמור ומה הלאה.

כללים:
- עברית פשוטה מאוד. משפטים קצרים. רעיון אחד בכל משפט.
- בלי שום מילה באנגלית. בלי שמות קבצים, פקודות, קוד או מונחים טכניים. אם צריך לדבר על משהו כזה, תאר מה הוא עושה במילים של כל יום.
- כל שורה מתחילה במילה בעברית, אף פעם לא במספר, סימן או מילה לועזית.
- אם משהו מופשט, תן דוגמה קצרה מהחיים.
- גוף ראשון: עשיתי, בדקתי, אני צריך. "אתה" ו"ממך" הם תמיד המשתמש.
- מה, לא איך. בלי להסביר איך משהו עובד מבפנים.
- אם נכשלתי או נתקעתי, תגיד את זה ישר ב"מה עשיתי".
- שום דבר לפני החלק הראשון ושום דבר אחרי האחרון.
"#;

/// `monitors/monitors.json`: the hub inbox listener, armed by **Claude Code
/// itself** at session start instead of by the model.
///
/// This is the same `"<helper>" listen` command `HUB_RULES` asks an instance to
/// arm by hand, and `hook::command_is_hub_listener` matches it identically — so
/// the watcher exemption, `armed/<id>`'s heartbeat and the orphan reaper all
/// keep working unchanged. What changes is who arms it, and for how long.
///
/// **Why this exists.** A model-armed `Monitor` is capped at 30 minutes and has
/// no `persistent` option (server-side flag `tengu_breezy_crescent`, live since
/// 2026-09-14; `anthropics/claude-code#94553`, #94393, both open). Every
/// instance therefore woke twice an hour purely to re-arm — a turn, a tool call
/// and a line of pane noise each time, for nothing. A plugin monitor is armed on
/// a different path and is not capped: measured on a real `claude` 2.1.278 PTY
/// on 2026-09-20, a once-a-minute heartbeat delivered **36/36 ticks over 35
/// minutes with zero expiry notices and the same pid throughout**, while this
/// session's own tool-armed listener died at exactly 30:00 in the same half
/// hour. It also arms on `--resume`, and the monitor process inherits the full
/// spawn env (`MULPEX_STATE_DIR`, `MULPEX_INSTANCE_ID`), which is what lets one
/// static file serve every instance exactly like the two above.
///
/// The event reaches the model as the same `<task-notification>` a tool-armed
/// Monitor produced (`promptSource: system`), so every hook that keys on that
/// shape is unaffected.
///
/// **Two teeth.** Monitors are an *experimental* plugin component, so the schema
/// may move under us; and a dead monitor is never restarted. Both are why the
/// arm nudge stays as a crash-only fallback rather than being deleted.
pub const PLUGIN_MONITORS_JSON: &str = r#"[
  {
    "name": "hub-listener",
    "command": "\"__MULPEX_BIN__\" listen",
    "description": "Mulpex hub inbox listener"
  }
]
"#;
