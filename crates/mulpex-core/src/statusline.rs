//! `mulpex-helper statusline` — the statusline command Mulpex injects into every
//! `claude` through `--settings`.
//!
//! Claude Code hands a statusline command a JSON on stdin that carries
//! `context_window.used_percentage`: the exact number it uses itself, and the
//! only place that number is published. Reading tokens out of the transcript
//! instead would mean guessing the window size (200 k or 1 M is not recorded
//! there). So the helper saves that one field to `ctx/<id>` for the sidebar.
//!
//! **The catch:** `--settings` outranks the user's own settings, so ours
//! replaces whatever statusline the person already had. To leave their pane
//! looking exactly as before, we find their own `statusLine.command` and run it
//! with the same stdin, printing what it prints. Someone with no statusline of
//! their own gets an empty line, which is Claude Code's default.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Set on the person's own statusline command when we run it, so that if it is
/// (somehow) ours again, the nested copy saves the % and stops there instead of
/// recursing.
const CHAINED_ENV: &str = "MULPEX_STATUSLINE_CHAINED";

/// A statusline that takes longer than this is killed. Claude Code redraws the
/// line often; a stuck script must not pile up behind it.
const CHAIN_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(_args: &[String]) -> anyhow::Result<()> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let json: Option<Value> = serde_json::from_slice(&input).ok();

    if let (Some(pct), Some((state_dir, id))) = (json.as_ref().and_then(used_percentage), identity())
    {
        write_pct(&state_dir, id, pct);
    }

    if std::env::var_os(CHAINED_ENV).is_some() {
        return Ok(());
    }
    let cwd = json.as_ref().and_then(project_dir).or_else(|| std::env::current_dir().ok());
    let Some(cmd) = own_statusline(cwd.as_deref(), &user_config_dir()) else {
        return Ok(());
    };
    if let Some(out) = run_chained(&cmd, &input) {
        std::io::stdout().write_all(&out)?;
    }
    Ok(())
}

fn identity() -> Option<(PathBuf, usize)> {
    let id = std::env::var("MULPEX_INSTANCE_ID").ok()?.parse().ok()?;
    let dir = PathBuf::from(std::env::var_os("MULPEX_STATE_DIR")?);
    Some((dir, id))
}

/// `context_window.used_percentage`, when Claude Code has one yet (it is null
/// before the first reply).
fn used_percentage(json: &Value) -> Option<f64> {
    let pct = json.get("context_window")?.get("used_percentage")?.as_f64()?;
    (pct.is_finite() && pct >= 0.0).then_some(pct)
}

fn project_dir(json: &Value) -> Option<PathBuf> {
    let ws = json.get("workspace");
    ws.and_then(|w| w.get("project_dir"))
        .or_else(|| ws.and_then(|w| w.get("current_dir")))
        .or_else(|| json.get("cwd"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Atomic, so the app's poll never reads a half-written number.
fn write_pct(state_dir: &Path, id: usize, pct: f64) {
    let path = crate::ctx_path(state_dir, id);
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let text = format!("{pct:.1}");
    if std::fs::read_to_string(&path).is_ok_and(|old| old == text) {
        return;
    }
    let tmp = dir.join(format!("{id}.tmp"));
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Where the person's user-level settings live: `$CLAUDE_CONFIG_DIR`, else
/// `~/.claude`.
fn user_config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".claude")
}

/// The person's own `statusLine.command`, searched in Claude Code's order:
/// the project's local settings, then the project's shared settings, then the
/// user's. Anything that is our own helper is skipped.
fn own_statusline(project: Option<&Path>, user_dir: &Path) -> Option<String> {
    let mut files = Vec::new();
    if let Some(p) = project {
        files.push(p.join(".claude/settings.local.json"));
        files.push(p.join(".claude/settings.json"));
    }
    files.push(user_dir.join("settings.json"));
    files.iter().find_map(|f| statusline_in(f))
}

fn statusline_in(file: &Path) -> Option<String> {
    let json: Value = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    let sl = json.get("statusLine")?;
    if sl.get("type").and_then(Value::as_str).is_some_and(|t| t != "command") {
        return None;
    }
    let cmd = sl.get("command")?.as_str()?.trim();
    (!cmd.is_empty() && !is_ours(cmd)).then(|| cmd.to_string())
}

fn is_ours(cmd: &str) -> bool {
    cmd.contains("mulpex-helper") && cmd.contains("statusline")
}

/// Run the person's statusline with the same stdin; `None` if it failed to
/// start or ran past the timeout.
fn run_chained(cmd: &str, input: &[u8]) -> Option<Vec<u8>> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .env(CHAINED_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input);
    }
    // Read on a thread so a chatty script can't fill the pipe and stall while
    // we wait for it to exit.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < CHAIN_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    reader.join().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mpxsl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn reads_used_percentage_and_ignores_null() {
        let j: Value = serde_json::from_str(r#"{"context_window":{"used_percentage":42.3}}"#).unwrap();
        assert_eq!(used_percentage(&j), Some(42.3));
        let j: Value = serde_json::from_str(r#"{"context_window":{"used_percentage":null}}"#).unwrap();
        assert_eq!(used_percentage(&j), None);
        // `rate_limits` has its own used_percentage; it must not be mistaken for ours.
        let j: Value = serde_json::from_str(r#"{"rate_limits":{"used_percentage":90}}"#).unwrap();
        assert_eq!(used_percentage(&j), None);
    }

    #[test]
    fn writes_the_pct_file() {
        let d = tmp("write");
        write_pct(&d, 7, 12.345);
        assert_eq!(std::fs::read_to_string(crate::ctx_path(&d, 7)).unwrap(), "12.3");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn project_local_beats_project_beats_user() {
        let d = tmp("prec");
        let proj = d.join("proj");
        let user = d.join("user");
        std::fs::create_dir_all(proj.join(".claude")).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        let sl = |c: &str| format!(r#"{{"statusLine":{{"type":"command","command":"{c}"}}}}"#);
        std::fs::write(user.join("settings.json"), sl("user")).unwrap();
        assert_eq!(own_statusline(Some(&proj), &user).as_deref(), Some("user"));
        std::fs::write(proj.join(".claude/settings.json"), sl("shared")).unwrap();
        assert_eq!(own_statusline(Some(&proj), &user).as_deref(), Some("shared"));
        std::fs::write(proj.join(".claude/settings.local.json"), sl("local")).unwrap();
        assert_eq!(own_statusline(Some(&proj), &user).as_deref(), Some("local"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn never_chains_to_itself() {
        let d = tmp("self");
        std::fs::write(
            d.join("settings.json"),
            r#"{"statusLine":{"type":"command","command":"\"/Apps/Mulpex.app/Contents/MacOS/mulpex-helper\" statusline"}}"#,
        )
        .unwrap();
        assert_eq!(own_statusline(None, &d), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn chained_command_gets_stdin_and_its_stdout_is_returned() {
        let out = run_chained("cat", b"hello").unwrap();
        assert_eq!(out, b"hello");
    }
}
