//! Everything a `codex` (OpenAI Codex CLI) row is launched with, and the few
//! Codex-specific facts the helper reads back. See `docs/codex.md` for the
//! measurements behind each choice; the `S<n>` tags below point into its table.
//!
//! **Nothing is written to `~/.codex`.** Hooks, the hub MCP server and the hub
//! rules all ride the command line as `-c key=value` overrides (S1, S3, S4), so
//! a codex Mulpex did not start never sees any of it. The user's own
//! `~/.codex/hooks.json` hooks still run beside ours.
//!
//! **Every `-c` value is TOML.** Codex parses the part after `=` as a TOML value
//! and falls back to a raw string only when that fails — so a string must be a
//! TOML basic string, quoted and escaped. JSON string escaping is a subset of
//! TOML basic-string escaping (`\"`, `\\`, `\n`, `\t`, `\uXXXX`), which is what
//! [`toml_str`] relies on. A helper path with a space in it (`Mulpex Dev.app`)
//! has to survive two layers: the shell that runs the hook command (hence the
//! inner `"…"`) and the TOML string around it (hence the escaping).

use std::path::Path;

use crate::agent::Agent;

/// Env markers Codex sets in every command it runs (S20). A child Mulpex spawns
/// from inside a codex's environment (a headless fork, a remote peer probe)
/// must not inherit them — the same reason `CLAUDE_CODE_CHILD_SESSION` is
/// stripped for claudes.
pub const CHILD_ENV_DENY: &[&str] = &["CODEX_CI", "CODEX_SESSION_ID", "CODEX_THREAD_ID"];

/// The hook events Mulpex registers for a codex, and the helper subcommand each
/// runs. No `askq`/`plan`/`notification`: Codex has no question or plan dialog,
/// and `needs` comes from the rules' `[waiting for you]` line instead (Stop).
/// `PostCompact` exists in Codex and not in Claude Code; it ends a compaction
/// the way Claude's `SessionStart[compact]` does.
const HOOKS: &[(&str, &str, Option<u32>)] = &[
    ("SessionStart", "sessionstart", None),
    ("UserPromptSubmit", "userpromptsubmit", None),
    // Same budget as Claude's edit matcher: the lock wait (`LOCK_WAIT`, 240 s)
    // must finish before the hook times out, or a timeout would allow the edit.
    ("PreToolUse", "pretooluse", Some(280)),
    ("PostToolUse", "posttooluse", None),
    ("Stop", "stop", None),
    ("PreCompact", "precompact", None),
    ("PostCompact", "postcompact", None),
];

/// The last line a codex prints when its turn ends on a question for the user.
/// The codex rules ask for it (Codex has no `AskUserQuestion`), and the `Stop`
/// hook turns it into the `needs` status word.
pub const WAITING_MARKER: &str = "[waiting for you]";

/// A string as a TOML basic string (quoted, escaped).
pub fn toml_str(s: &str) -> String {
    // serde_json escapes `"`, `\` and every control character (as `\n`, `\t`,
    // `\u00XX`), all of which TOML basic strings read identically.
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// The shell command a hook runs: `"<helper>" hook <event>`, the same spelling
/// `config::HOOK_SETTINGS_JSON` uses for Claude.
fn hook_command(helper: &Path, sub: &str) -> String {
    format!("\"{}\" hook {sub}", helper.display())
}

/// Every option a Mulpex codex is started with, in order. The app appends
/// `resume <uuid>` and/or the positional first prompt (the task rides argv,
/// never the TUI — the same contract as for claudes).
///
/// - `--dangerously-bypass-approvals-and-sandbox`: the codex counterpart of
///   `--dangerously-skip-permissions`.
/// - `--dangerously-bypass-hook-trust`: our per-launch hooks would otherwise
///   wait for a persisted trust grant (S1). Only our own hooks are added here.
/// - `--no-alt-screen`: the pane keeps its scrollback and xterm selection.
/// - `check_for_update_on_startup=false`: otherwise a blocking update picker
///   sits in front of the first prompt.
pub fn launch_args(helper: &Path, state_dir: &Path, project_dir: &Path, id: usize) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--dangerously-bypass-approvals-and-sandbox".into(),
        "--dangerously-bypass-hook-trust".into(),
        "--no-alt-screen".into(),
        "-c".into(),
        "check_for_update_on_startup=false".into(),
        "-c".into(),
        trust_override(project_dir),
    ];
    for (event, sub, timeout) in HOOKS {
        let mut hook = format!(
            "{{type=\"command\",command={}",
            toml_str(&hook_command(helper, sub))
        );
        if let Some(t) = timeout {
            hook.push_str(&format!(",timeout={t}"));
        }
        hook.push('}');
        args.push("-c".into());
        args.push(format!("hooks.{event}=[{{hooks=[{hook}]}}]"));
    }
    // The MCP server does NOT inherit the codex's env (S3), so its identity is
    // spelled out. MULPEX_HOME too, resolved here: a dev build runs on its own
    // home, and the server would otherwise fall back to the release one.
    let env = [
        ("MULPEX_INSTANCE_ID", id.to_string()),
        ("MULPEX_STATE_DIR", state_dir.display().to_string()),
        ("MULPEX_PROJECT_DIR", project_dir.display().to_string()),
        (crate::agent::AGENT_ENV, Agent::Codex.word().to_string()),
        ("MULPEX_HOME", crate::mulpex_home().display().to_string()),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", toml_str(v)))
    .collect::<Vec<_>>()
    .join(",");
    args.push("-c".into());
    args.push(format!("mcp_servers.mulpex.command={}", toml_str(&helper.display().to_string())));
    args.push("-c".into());
    args.push("mcp_servers.mulpex.args=[\"mcp\"]".into());
    args.push("-c".into());
    args.push(format!("mcp_servers.mulpex.env={{{env}}}"));
    args.push("-c".into());
    args.push(format!(
        "developer_instructions={}",
        toml_str(&crate::rules::system_prompt_for(helper, Agent::Codex))
    ));
    args
}

/// `projects={"<dir>"={trust_level="trusted"},…}`: skips Codex's "trust this
/// folder?" dialog for this launch only. Measured (codex 0.149.1): the dotted
/// `projects."<dir>".trust_level` form does NOT skip it, the inline table does;
/// and Codex keys trust by the git ROOT, so a subdir or a worktree needs its
/// root listed too. `~/.codex/config.toml` is never touched.
pub fn trust_override(project_dir: &Path) -> String {
    let mut dirs = vec![project_dir.display().to_string()];
    if let Some(root) = git_root(project_dir) {
        let root = root.display().to_string();
        if !dirs.contains(&root) {
            dirs.push(root);
        }
    }
    let entries = dirs
        .iter()
        .map(|d| format!("{}={{trust_level=\"trusted\"}}", toml_str(d)))
        .collect::<Vec<_>>()
        .join(",");
    format!("projects={{{entries}}}")
}

/// What `git rev-parse --show-toplevel` would say, without running git: the
/// nearest ancestor holding a `.git` (a directory, or a worktree's file).
fn git_root(dir: &Path) -> Option<std::path::PathBuf> {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    dir.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

/// The line Mulpex rings an idle codex with (`codex queue --message`). It
/// starts with the exact text `listen.rs` prints for a claude, and carries the
/// instruction itself: a codex once acked a bare count and never read its inbox.
pub fn wake_line(n: usize) -> String {
    format!("{} {WAKE_TAIL}", wake_count(n))
}

/// `mulpex: N new hub message(s)` — the shared prefix of every wake line.
fn wake_count(n: usize) -> String {
    format!("mulpex: {n} new hub message(s)")
}

/// What follows the count in [`wake_line`].
const WAKE_TAIL: &str = "— call mcp__mulpex__hub_inbox now and act on them";

/// Is this user turn Mulpex ringing (a [`wake_line`], or the bare count an
/// older build sent) rather than the user typing? Exact shapes only, so a user
/// who happens to start a prompt with "mulpex:" is still the user.
pub fn is_wake_line(prompt: &str) -> bool {
    let Some(rest) = prompt.trim().strip_prefix("mulpex: ") else {
        return false;
    };
    let Some((num, tail)) = rest.split_once(" new hub message(s)") else {
        return false;
    };
    let tail = tail.trim();
    !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) && (tail.is_empty() || tail == WAKE_TAIL)
}

/// How full a codex's context window is, in percent, from the last
/// `token_count` event of its rollout file (S17):
/// `info.last_token_usage.total_tokens / info.model_context_window`.
///
/// Reads only the file's tail: a rollout grows to many MB, this runs on every
/// `Stop` and (throttled) `PostToolUse`, and the newest event is at the end. A
/// tail with no `token_count` (a first turn still streaming) is `None`, never 0.
pub fn ctx_pct(rollout: &Path) -> Option<u8> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 512 * 1024;
    let mut f = std::fs::File::open(rollout).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .rev()
        .filter(|l| l.contains("\"token_count\""))
        .find_map(ctx_pct_of_line)
}

fn ctx_pct_of_line(line: &str) -> Option<u8> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let payload = v.get("payload")?;
    if payload.get("type")?.as_str()? != "token_count" {
        return None;
    }
    let info = payload.get("info")?;
    let used = info.get("last_token_usage")?.get("total_tokens")?.as_f64()?;
    let window = info.get("model_context_window")?.as_f64()?;
    if window <= 0.0 {
        return None;
    }
    Some((used / window * 100.0).round().clamp(0.0, 100.0) as u8)
}

/// Does this final assistant message end on the `[waiting for you]` line?
pub fn ends_waiting(message: &str) -> bool {
    message
        .trim_end()
        .lines()
        .last()
        .is_some_and(|l| l.trim() == WAITING_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg_after<'a>(args: &'a [String], prefix: &str) -> &'a str {
        args.iter()
            .find(|a| a.starts_with(prefix))
            .unwrap_or_else(|| panic!("no arg starting {prefix}: {args:?}"))
    }

    #[test]
    fn launch_args_carry_every_hook_the_mcp_env_and_the_rules() {
        let helper = Path::new("/Applications/Mulpex Dev.app/Contents/MacOS/mulpex-helper");
        let args = launch_args(helper, Path::new("/tmp/st ate"), Path::new("/p"), 7);
        assert_eq!(args[0], "--dangerously-bypass-approvals-and-sandbox");
        assert!(args.contains(&"--dangerously-bypass-hook-trust".to_string()));
        assert!(args.contains(&"--no-alt-screen".to_string()));
        assert!(args.contains(&"check_for_update_on_startup=false".to_string()));
        // Every value after `-c` is a key=value pair.
        for w in args.windows(2) {
            if w[0] == "-c" {
                assert!(w[1].contains('='), "{}", w[1]);
            }
        }
        for (event, sub, _) in HOOKS {
            let a = arg_after(&args, &format!("hooks.{event}="));
            // The path with a space is quoted for the shell, escaped for TOML.
            assert!(
                a.contains(&format!(
                    "command=\"\\\"/Applications/Mulpex Dev.app/Contents/MacOS/mulpex-helper\\\" hook {sub}\""
                )),
                "{a}"
            );
        }
        assert!(arg_after(&args, "hooks.PreToolUse=").contains("timeout=280"));
        let env = arg_after(&args, "mcp_servers.mulpex.env=");
        assert!(env.contains("MULPEX_INSTANCE_ID=\"7\""), "{env}");
        assert!(env.contains("MULPEX_STATE_DIR=\"/tmp/st ate\""), "{env}");
        assert!(env.contains("MULPEX_PROJECT_DIR=\"/p\""), "{env}");
        assert!(env.contains("MULPEX_AGENT=\"codex\""), "{env}");
        assert_eq!(
            arg_after(&args, "mcp_servers.mulpex.command="),
            "mcp_servers.mulpex.command=\"/Applications/Mulpex Dev.app/Contents/MacOS/mulpex-helper\""
        );
        let rules = arg_after(&args, "developer_instructions=");
        let body = rules.strip_prefix("developer_instructions=").unwrap();
        // The TOML string decodes (as JSON, its subset) back to the exact rules.
        let decoded: String = serde_json::from_str(body).unwrap();
        assert_eq!(decoded, crate::rules::system_prompt_for(helper, Agent::Codex));
    }

    #[test]
    fn trust_lists_the_dir_and_its_git_root() {
        let dir = std::env::temp_dir().join(format!("mpx-trust-{}", crate::persist::new_uuid()));
        let sub = dir.join("a b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join(".git"), "gitdir: x").unwrap();
        let root = std::fs::canonicalize(&dir).unwrap();
        let t = trust_override(&sub);
        assert_eq!(
            t,
            format!(
                "projects={{{}={{trust_level=\"trusted\"}},{}={{trust_level=\"trusted\"}}}}",
                toml_str(&sub.display().to_string()),
                toml_str(&root.display().to_string())
            )
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Not a test: writes `launch_args` NUL-separated to `$MPX_ARGS_OUT` for a
    /// live run against a real `codex` (`MPX_HELPER`, `MPX_STATE`, `MPX_PROJECT`).
    #[test]
    #[ignore]
    fn dump_launch_args() {
        let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} unset"));
        let args = launch_args(
            Path::new(&var("MPX_HELPER")),
            Path::new(&var("MPX_STATE")),
            Path::new(&var("MPX_PROJECT")),
            var("MPX_ID").parse().unwrap(),
        );
        std::fs::write(var("MPX_ARGS_OUT"), args.join("\0")).unwrap();
    }

    #[test]
    fn the_wake_line_round_trips_and_nothing_else_matches() {
        assert_eq!(wake_line(3), "mulpex: 3 new hub message(s) — call mcp__mulpex__hub_inbox now and act on them");
        assert!(wake_line(1).starts_with(&wake_count(1)));
        assert!(is_wake_line(&wake_line(1)));
        assert!(is_wake_line(" mulpex: 12 new hub message(s)\n"), "the bare form an older build sent");
        assert!(!is_wake_line("mulpex: please fix the hub"));
        assert!(!is_wake_line("mulpex:  new hub message(s)"));
        assert!(!is_wake_line("mulpex: 2 new hub message(s) and also fix X"));
    }

    #[test]
    fn ctx_pct_reads_the_last_token_count() {
        let dir = std::env::temp_dir().join(format!("mpx-codexctx-{}", crate::persist::new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("rollout.jsonl");
        assert_eq!(ctx_pct(&f), None, "no file");
        let tc = |used: u64| {
            format!(
                "{{\"timestamp\":\"x\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":{{\"total_tokens\":999999}},\"last_token_usage\":{{\"total_tokens\":{used}}},\"model_context_window\":258400}},\"rate_limits\":null}}}}\n"
            )
        };
        std::fs::write(&f, "{\"type\":\"session_meta\",\"payload\":{}}\n").unwrap();
        assert_eq!(ctx_pct(&f), None, "no token_count yet");
        let mut body = std::fs::read_to_string(&f).unwrap();
        body.push_str(&tc(14512));
        body.push_str(&tc(129200));
        body.push_str("{\"type\":\"response_item\",\"payload\":{\"type\":\"message\"}}\n");
        std::fs::write(&f, body).unwrap();
        assert_eq!(ctx_pct(&f), Some(50));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_waiting_marker_must_be_the_last_line() {
        assert!(ends_waiting("Which one?\n\n[waiting for you]\n"));
        assert!(ends_waiting("[waiting for you]"));
        assert!(!ends_waiting("[waiting for you]\nthen I continue"));
        assert!(!ends_waiting("done"));
    }
}
