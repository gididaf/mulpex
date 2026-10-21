//! One read-only headless run of an agent: `claude -p` or `codex exec`. The
//! save (⌘S), the handoff (⌘⌥S) and Import Docs all go through [`run`]; the
//! streaming callers (⌘⇧E, the ⌘⇧B summary) build their child from
//! [`command`] and parse the stream themselves.
//!
//! **Claude** argv is what `saves.rs::run_claude` always ran, byte for byte.
//!
//! **Codex** (measured on codex-cli 0.149.1, docs/codex.md S18):
//! `codex exec [fork <id>] --ephemeral --skip-git-repo-check -c sandbox_mode="read-only"
//! -c approval_policy="never" -c check_for_update_on_startup=false -o <file> -`,
//! the prompt on stdin (closed after, or exec waits for it). A fork leaves the
//! original rollout byte-identical and `--ephemeral` writes no new one. No `-m`:
//! the model is the user's `~/.codex/config.toml` choice. The answer is read
//! from the `-o` file, not stdout.
//!
//! **Codex reports a failure on stderr as `ERROR: …` lines, the LAST one being
//! the verdict** — measured 2026-10-21 with no login: a page of reconnect
//! retries, then `ERROR: unexpected status 401 Unauthorized: …`. So a codex
//! failure takes the last `ERROR:` line, where a claude one takes the first.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mulpex_core::agent::Agent;

use crate::claude_bin;
use crate::saves::{failure_reason, first_chars, result_text};

/// What one headless run is asked to do. `model` and `route` are Claude's
/// (a model alias, and the Live Token route of the claude it works for);
/// codex takes its model from its own config and has no Live Token.
pub struct Job<'a> {
    pub dir: &'a Path,
    pub model: &'a str,
    pub route: Option<&'a str>,
    /// Fork this conversation (claude uuid / codex thread id) as a throwaway.
    pub fork: Option<&'a str>,
    pub prompt: &'a str,
    pub timeout: Duration,
}

/// Run `job` on `agent` and return its answer text.
pub fn run(agent: Agent, job: &Job) -> Result<String, String> {
    match agent {
        Agent::Claude => run_claude(job),
        Agent::Codex => run_codex(job),
    }
}

/// The agent for a job that belongs to no row (the ⌘⇧B summary, Import Docs):
/// claude when it is installed, else codex.
pub fn preferred_agent() -> Agent {
    if claude_bin::resolve(Agent::Claude).is_some() { Agent::Claude } else { Agent::Codex }
}

/// The binary, or the error a caller shows (and the handoff falls back on).
pub fn binary(agent: Agent) -> Result<&'static PathBuf, String> {
    claude_bin::resolve(agent).ok_or_else(|| format!("{} not found", agent.word()))
}

/// A child of `agent` in `dir` with the scrubbed login env: no `MULPEX_*`, no
/// `CLAUDE_CODE_CHILD_SESSION` (`forwarded_env`), none of the markers codex
/// sets in commands it runs, and — for claude — the Live Token of `route`.
/// Args, stdio and the spawn are the caller's.
pub fn command(agent: Agent, dir: &Path, route: Option<&str>) -> Result<Command, String> {
    let mut cmd = Command::new(binary(agent)?);
    cmd.env_clear()
        .envs(claude_bin::forwarded_env().into_iter().filter(|(k, _)| match agent {
            Agent::Claude => true,
            Agent::Codex => !mulpex_core::codex::CHILD_ENV_DENY.contains(&k.as_str()),
        }))
        .env("PATH", claude_bin::merged_path());
    if agent == Agent::Claude {
        cmd.envs(crate::live_token::spawn_env(route).unwrap_or_default());
    }
    cmd.current_dir(dir);
    Ok(cmd)
}

/// The options every headless codex gets, after `exec` (and `fork <id>` /
/// `resume <id>`, whose own options they are).
pub fn codex_exec_opts(ephemeral: bool) -> Vec<&'static str> {
    let mut v = Vec::new();
    if ephemeral {
        v.push("--ephemeral");
    }
    v.extend([
        "--skip-git-repo-check",
        "-c",
        "sandbox_mode=\"read-only\"",
        "-c",
        "approval_policy=\"never\"",
        "-c",
        "check_for_update_on_startup=false",
    ]);
    v
}

/// Today's `claude -p` argv for a read-only step, unchanged from `saves.rs`.
fn claude_args<'a>(model: &'a str, fork: Option<&'a str>) -> Vec<&'a str> {
    let mut args: Vec<&str> = vec![
        "-p",
        "--model",
        model,
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--output-format",
        "json",
        "--tools",
        "Read,Grep,Glob,Bash",
        "--allowedTools",
        "Read,Grep,Glob,Bash(git log:*),Bash(git status:*),Bash(git diff:*),\
         Bash(git show:*),Bash(git branch:*)",
        "--disallowedTools",
        "Edit,Write,NotebookEdit",
    ];
    if let Some(uuid) = fork {
        args.extend(["--resume", uuid, "--fork-session"]);
    }
    args
}

fn run_claude(job: &Job) -> Result<String, String> {
    let args = claude_args(job.model, job.fork);
    let mut cmd = command(Agent::Claude, job.dir, job.route)?;
    cmd.args(&args);
    let (status, out, err) = run_child(cmd, job.prompt, job.timeout)?;
    if !status.success() {
        return Err(failure_reason(status.code(), &out, &err));
    }
    result_text(&out)
}

fn run_codex(job: &Job) -> Result<String, String> {
    static N: AtomicU64 = AtomicU64::new(0);
    let out_file = std::env::temp_dir().join(format!(
        "mulpex-codex-{}-{}.txt",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let out_arg = out_file.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["exec"];
    if let Some(id) = job.fork {
        args.extend(["fork", id]);
    }
    args.extend(codex_exec_opts(true));
    args.extend(["-o", &out_arg, "-"]);
    let mut cmd = command(Agent::Codex, job.dir, None)?;
    cmd.args(&args);
    let ran = run_child(cmd, job.prompt, job.timeout);
    let answer = std::fs::read_to_string(&out_file).unwrap_or_default();
    let _ = std::fs::remove_file(&out_file);
    let (status, out, err) = ran?;
    if !status.success() {
        return Err(codex_failure_reason(status.code(), &out, &err));
    }
    if answer.trim().is_empty() {
        return Err(format!("empty output{}", codex_reason(&out, &err).map(|r| format!(": {r}")).unwrap_or_default()));
    }
    Ok(answer)
}

/// Why a headless codex died: the last `ERROR:` line (see the module doc),
/// else whatever it said last, stderr before stdout.
pub fn codex_failure_reason(code: Option<i32>, out: &str, err: &str) -> String {
    let code = code.map_or("killed".to_string(), |c| format!("exit {c}"));
    match codex_reason(out, err) {
        Some(reason) => format!("{code}: {reason}"),
        None => code,
    }
}

fn codex_reason(out: &str, err: &str) -> Option<String> {
    let last_error = |s: &str| {
        s.lines()
            .rev()
            .map(str::trim)
            .find_map(|l| l.strip_prefix("ERROR:").map(str::trim).filter(|l| !l.is_empty()))
            .map(str::to_string)
    };
    let last_line = |s: &str| s.lines().rev().map(str::trim).find(|l| !l.is_empty()).map(str::to_string);
    let reason = last_error(err)
        .or_else(|| json_error(out))
        .or_else(|| last_line(err))
        .or_else(|| last_line(out))?;
    Some(first_chars(&reason, 200))
}

/// The message of a `--json` stream's `error` / `turn.failed` event.
fn json_error(out: &str) -> Option<String> {
    out.lines().rev().find_map(|l| {
        let v: serde_json::Value = serde_json::from_str(l.trim()).ok()?;
        match v.get("type")?.as_str()? {
            "error" => v.get("message")?.as_str().map(str::to_string),
            "turn.failed" => v.get("error")?.get("message")?.as_str().map(str::to_string),
            _ => None,
        }
    })
}

/// Whether a failure means the agent can't serve right now — out of quota,
/// logged out, or not installed — so the handoff should let the other agent
/// write the doc instead. Matches Claude's and Codex's own wording.
pub fn is_unavailable(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    [
        "not found",
        "usage limit",
        "limit reached",
        "rate limit",
        "rate_limit",
        "hit your limit",
        "quota",
        "429",
        "401",
        "unauthorized",
        "failed to authenticate",
        "authentication",
        "not logged in",
        "please run /login",
        "codex login",
        "overloaded",
    ]
    .iter()
    .any(|k| r.contains(k))
}

/// Spawn, feed `prompt` on stdin and close it, drain both pipes, wait with a
/// timeout. Both pipes are drained on their own threads: a child that fills one
/// while we block reading the other deadlocks until the timeout.
fn run_child(mut cmd: Command, prompt: &str, timeout: Duration) -> Result<(ExitStatus, String, String), String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut f) = pipe {
                let _ = f.read_to_string(&mut s);
            }
            s
        })
    };
    let out_reader = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let err_reader = drain(child.stderr.take().map(|p| Box::new(p) as _));

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timeout after {} min", timeout.as_secs() / 60));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(250)),
            Err(e) => return Err(format!("wait: {e}")),
        }
    };
    Ok((status, out_reader.join().unwrap_or_default(), err_reader.join().unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// stderr measured 2026-10-21 from `codex exec` with an empty CODEX_HOME.
    const NO_LOGIN: &str = "\
websocket: failed to connect to websocket: HTTP error: 401 Unauthorized, url: wss://api.openai.com/v1/responses
ERROR: Reconnecting... 5/5
warning: Falling back from WebSockets to HTTPS transport. unexpected status 401 Unauthorized: Missing bearer
ERROR: Reconnecting... 1/5
ERROR: unexpected status 401 Unauthorized: Missing bearer or basic authentication in header, url: https://api.openai.com/v1/responses
";

    #[test]
    fn codex_failure_is_the_last_error_line() {
        let r = codex_failure_reason(Some(1), "", NO_LOGIN);
        assert!(r.starts_with("exit 1: unexpected status 401 Unauthorized"), "{r}");
        assert!(is_unavailable(&r));
        assert_eq!(codex_failure_reason(Some(1), "", ""), "exit 1");
        let json = r#"{"type":"turn.failed","error":{"message":"You've hit your usage limit."}}"#;
        assert_eq!(codex_failure_reason(Some(1), json, ""), "exit 1: You've hit your usage limit.");
        assert_eq!(codex_failure_reason(Some(2), "", "boom\n"), "exit 2: boom");
    }

    #[test]
    fn unavailable_covers_limits_auth_and_missing_binaries() {
        for r in [
            "codex not found",
            "exit 1: Claude AI usage limit reached|1792580000",
            "exit 1: 5-hour limit reached ∙ resets 3pm",
            "exit 1: Failed to authenticate. API Error: 401 OAuth access token is invalid.",
            "API Error: 429 {\"type\":\"rate_limit_error\"}",
            "exit 1: You've hit your usage limit. Upgrade to Pro",
        ] {
            assert!(is_unavailable(r), "{r}");
        }
        for r in ["no JSON in the answer: hi", "timeout after 15 min", "exit 1: boom"] {
            assert!(!is_unavailable(r), "{r}");
        }
    }

    /// The argv `saves.rs::run_claude` ran before codex existed, byte for byte.
    #[test]
    fn claude_argv_is_unchanged() {
        let golden = [
            "-p", "--model", "opus", "--setting-sources", "", "--strict-mcp-config",
            "--no-session-persistence", "--output-format", "json", "--tools", "Read,Grep,Glob,Bash",
            "--allowedTools",
            "Read,Grep,Glob,Bash(git log:*),Bash(git status:*),Bash(git diff:*),Bash(git show:*),Bash(git branch:*)",
            "--disallowedTools", "Edit,Write,NotebookEdit", "--resume", "u", "--fork-session",
        ];
        assert_eq!(claude_args("opus", Some("u")), golden);
        assert_eq!(claude_args("opus", None), golden[..golden.len() - 3]);
    }

    #[test]
    fn codex_options_are_read_only_and_quiet() {
        let o = codex_exec_opts(true);
        assert_eq!(o[0], "--ephemeral");
        assert!(o.windows(2).any(|w| w == ["-c", "sandbox_mode=\"read-only\""]));
        assert!(o.windows(2).any(|w| w == ["-c", "check_for_update_on_startup=false"]));
        assert!(!codex_exec_opts(false).contains(&"--ephemeral"));
        assert!(!o.contains(&"-m"), "the model is the user's config.toml choice");
    }
}
