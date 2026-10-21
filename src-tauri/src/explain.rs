//! ⌘⇧E Explain Selection: the user selects part of a claude's pane they don't
//! understand, and a side panel explains it in simple Hebrew.
//!
//! The selection is only the target. The context is the whole conversation:
//! a hidden Sonnet fork of the instance (`--resume <uuid> --fork-session`, the
//! same trick ⌘S uses — `saves.rs`) gets the selection as its prompt. Nothing is
//! typed into the claude, so it works mid-turn and the conversation stays clean.
//!
//! Measured 2026-10-07/08 on a 2.4 MB transcript (233 k tokens): the answer in
//! 12–22 s for $0.92–0.95, almost all of it the cache write of the conversation.
//! The original `.jsonl` stayed byte-identical.
//!
//! **Follow-ups resume the fork's own session, never a fresh fork.** Measured on
//! the same conversation: a re-fork carrying the exchange in a longer prompt
//! missed the cache (8 k read, 226 k written, $0.91), while `--resume <fork>`
//! read it all back ($0.12, 7 s). The prompt cache is only consulted at the end
//! of the previous request, and a re-fork's last message is not that.
//! So the first ask keeps its transcript (no `--no-session-persistence`), and
//! Mulpex deletes that file itself when the panel closes, the instance or project
//! goes, or the app quits ([`discard`], [`discard_all`]) — otherwise every ⌘⇧E
//! would leave a conversation in the project's `claude --resume` list.
//!
//! The answer streams: `--output-format stream-json --include-partial-messages`
//! gives `text_delta` events, forwarded as `explain-progress`. **A resumed fork
//! prints a stale `result` line first** (the old conversation's, `num_turns: 0`),
//! so only the last `result` counts.
//!
//! One explain per instance: a new ⌘⇧E, or closing the panel, kills the old child.
//!
//! **A codex row** is explained by codex (docs/codex.md S18): `codex exec fork
//! <thread> --json` — not `--ephemeral`, so follow-ups can `codex exec resume
//! <fork>`, and the fork is removed with `codex delete --force <fork>` (without
//! `--force` it refuses: "cannot confirm session deletion without an interactive
//! terminal", measured 2026-10-21). Its `--json` stream has no text deltas, so
//! the answer arrives as one chunk at the end. The model is the user's
//! `~/.codex/config.toml` choice.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mulpex_core::agent::Agent;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::headless;
use crate::saves::failure_reason;
use crate::state::AppState;
use crate::snapshot::ProjectHandle;

const PROMPT: &str = include_str!("explain_prompt.md");

/// Only catches a hung child: the measured answer took ~12 s.
const TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// `explain-progress` payload. `kind` is `delta` (`text` = the next piece),
/// `done`, or `error` (`text` = the reason). `req_id` is the frontend's, so a
/// late event from a replaced request is ignored there.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Progress {
    handle: ProjectHandle,
    id: usize,
    req_id: String,
    kind: &'static str,
    text: String,
}

/// The running child per instance, so a new request or a close can kill it.
type Running = HashMap<(ProjectHandle, usize), (String, Arc<Mutex<Child>>)>;
static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

/// Each panel's fork session, so a follow-up can resume it and a close can
/// delete it: the project dir it lives under, the ⌘⇧E request that created it,
/// and its uuid once that request's child has reported it.
struct Fork {
    agent: Agent,
    dir: PathBuf,
    req_id: String,
    uuid: Option<String>,
}
type Forks = HashMap<(ProjectHandle, usize), Fork>;
static FORKS: Mutex<Option<Forks>> = Mutex::new(None);

/// Kill (handle, id)'s running explain, if any. Silent: the frontend already
/// dropped the request.
pub fn cancel(handle: ProjectHandle, id: usize) {
    let entry = RUNNING.lock().unwrap().as_mut().and_then(|m| m.remove(&(handle, id)));
    if let Some((_, child)) = entry {
        let _ = child.lock().unwrap().kill();
    }
}

/// The prompt for a fresh ⌘⇧E.
fn prompt_for(selection: &str) -> String {
    PROMPT.replace("{{SELECTION}}", selection.trim())
}

/// The prompt for a follow-up. The fork already holds the selection, the rules
/// and its own answers, so this is only the question.
fn followup_prompt(question: &str) -> String {
    format!(
        "The user asks a follow-up question about your explanation, in the same side panel. \
Answer it by the same rules: simple Hebrew, key terms explained, no tools, short.\n\n\
<user_question>\n{}\n</user_question>\n",
        question.trim()
    )
}

/// Delete a fork's transcript (and the sidecar dir Claude Code may keep beside
/// it), or have codex delete its thread. Only ever an id this module saw its
/// own child report.
fn delete_fork(agent: Agent, dir: &Path, fork: &str) {
    match agent {
        Agent::Claude => {
            let path = crate::saves::transcript_path(dir, fork);
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_dir_all(path.with_extension(""));
        }
        Agent::Codex => {
            let Ok(mut cmd) = headless::command(Agent::Codex, dir, None) else { return };
            cmd.args(["delete", "--force", fork])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // Off the caller's thread: teardown and panel closes must not wait.
            if let Ok(mut child) = cmd.spawn() {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
        }
    }
}

/// Stop claude#id's explain and delete its fork: the panel closed, was
/// replaced, or its instance is gone.
pub fn discard(handle: ProjectHandle, id: usize) {
    cancel(handle, id);
    let entry = FORKS.lock().unwrap().as_mut().and_then(|m| m.remove(&(handle, id)));
    if let Some(Fork { agent, dir, uuid: Some(fork), .. }) = entry {
        delete_fork(agent, &dir, &fork);
    }
}

/// App teardown: every fork still open.
pub fn discard_all() {
    let keys: Vec<_> = FORKS.lock().unwrap().as_ref().map(|m| m.keys().copied().collect()).unwrap_or_default();
    for (h, i) in keys {
        discard(h, i);
    }
}

/// A fresh ⌘⇧E: explain `selection` for claude#id in a new fork of its
/// conversation `uuid`, replacing the panel's previous fork. `dir` is the
/// project dir — `--resume` finds a conversation by its cwd.
pub fn start(
    app: AppHandle,
    handle: ProjectHandle,
    id: usize,
    req_id: String,
    dir: PathBuf,
    uuid: String,
    selection: String,
) -> Result<(), String> {
    discard(handle, id);
    let agent = app
        .state::<AppState>()
        .ws
        .lock()
        .unwrap()
        .project(handle)
        .and_then(|core| core.agent_of(id))
        .unwrap_or_default();
    let fork = Fork { agent, dir: dir.clone(), req_id: req_id.clone(), uuid: None };
    FORKS.lock().unwrap().get_or_insert_with(HashMap::new).insert((handle, id), fork);
    let resume: Vec<&str> = match agent {
        Agent::Claude => vec!["--resume", uuid.as_str(), "--fork-session"],
        Agent::Codex => vec!["fork", uuid.as_str()],
    };
    run(app, agent, handle, id, req_id, dir, &resume, true, prompt_for(&selection))
}

/// A follow-up in claude#id's panel: resume the panel's own fork, so the
/// conversation is read back from the prompt cache.
pub fn followup(
    app: AppHandle,
    handle: ProjectHandle,
    id: usize,
    req_id: String,
    question: String,
) -> Result<(), String> {
    let fork = FORKS
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.get(&(handle, id)))
        .and_then(|f| Some((f.agent, f.dir.clone(), f.uuid.clone()?)));
    let Some((agent, dir, fork)) = fork else {
        return Err("Nothing to follow up on — select the text and press ⌘⇧E again".into());
    };
    let resume: Vec<&str> = match agent {
        Agent::Claude => vec!["--resume", &fork],
        Agent::Codex => vec!["resume", &fork],
    };
    run(app, agent, handle, id, req_id, dir, &resume, false, followup_prompt(&question))
}

/// A codex `--json` event, as far as an explain cares.
#[derive(Debug, PartialEq)]
pub(crate) enum CodexEvent {
    Thread(String),
    Message(String),
    Failed(String),
}

pub(crate) fn codex_event(v: &serde_json::Value) -> Option<CodexEvent> {
    let s = |v: Option<&serde_json::Value>| v.and_then(|x| x.as_str()).map(str::to_string);
    match v.get("type")?.as_str()? {
        "thread.started" => s(v.get("thread_id")).map(CodexEvent::Thread),
        "item.completed" => {
            let item = v.get("item")?;
            (item.get("type")?.as_str()? == "agent_message").then(|| s(item.get("text")))?.map(CodexEvent::Message)
        }
        "error" => s(v.get("message")).map(CodexEvent::Failed),
        "turn.failed" => s(v.get("error").and_then(|e| e.get("message"))).map(CodexEvent::Failed),
        _ => None,
    }
}

/// Spawn one headless child with `resume` args and stream its answer. `forks`:
/// this child creates the panel's fork, so the uuid it reports is recorded.
/// Returns once it is spawned; the answer arrives as events.
#[allow(clippy::too_many_arguments)]
fn run(
    app: AppHandle,
    agent: Agent,
    handle: ProjectHandle,
    id: usize,
    req_id: String,
    dir: PathBuf,
    resume: &[&str],
    forks: bool,
    prompt: String,
) -> Result<(), String> {
    cancel(handle, id);
    let mut cmd: Command = headless::command(agent, &dir, Some(&crate::live_token::route(&dir, id)))?;
    match agent {
        Agent::Claude => cmd
            .args([
                "-p",
                "--model",
                "sonnet",
                "--setting-sources",
                "",
                "--strict-mcp-config",
                "--tools",
                "",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
            ])
            .args(resume),
        Agent::Codex => cmd
            .arg("exec")
            .args(resume)
            .args(headless::codex_exec_opts(false))
            .args(["--json", "-"]),
    };
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(child));
    RUNNING
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert((handle, id), (req_id.clone(), child.clone()));

    // Watchdog: a hung child keeps stdout open, so the reader below would never
    // notice. Killing an already-reaped `Child` sends nothing.
    let timed_out = Arc::new(AtomicBool::new(false));
    {
        let (child, timed_out) = (child.clone(), timed_out.clone());
        std::thread::spawn(move || {
            std::thread::sleep(TIMEOUT);
            let mut c = child.lock().unwrap();
            if matches!(c.try_wait(), Ok(None)) {
                timed_out.store(true, Ordering::Relaxed);
                let _ = c.kill();
            }
        });
    }

    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut e) = stderr {
            let _ = e.read_to_string(&mut s);
        }
        s
    });

    std::thread::spawn(move || {
        let emit = |kind: &'static str, text: String| {
            let _ = app.emit(
                "explain-progress",
                Progress { handle, id, req_id: req_id.clone(), kind, text },
            );
        };
        // Lines that aren't JSON: `claude -p` prints its own failures as plain
        // text on stdout (saves.rs::failure_reason).
        let mut plain = String::new();
        let mut result: Option<(bool, String)> = None;
        // Codex: the last agent message, sent whole once the run succeeds.
        let mut answer: Option<String> = None;
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                match serde_json::from_str::<serde_json::Value>(&line) {
                    Ok(v) if agent == Agent::Codex => match codex_event(&v) {
                        Some(CodexEvent::Thread(sid)) if forks => note_fork(agent, handle, id, &req_id, &dir, &sid),
                        Some(CodexEvent::Message(t)) => answer = Some(t),
                        Some(CodexEvent::Failed(t)) => result = Some((true, t)),
                        _ => {}
                    },
                    Ok(v) => {
                        if let Some(t) = text_delta(&v) {
                            emit("delta", t.to_string());
                        } else if let Some(sid) = init_session(&v).filter(|_| forks) {
                            note_fork(agent, handle, id, &req_id, &dir, sid);
                        } else if v.get("type").and_then(|t| t.as_str()) == Some("result") {
                            let is_err = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                            let text = v.get("result").and_then(|r| r.as_str()).unwrap_or("");
                            result = Some((is_err, text.to_string()));
                        }
                    }
                    Err(_) => {
                        plain.push_str(&line);
                        plain.push('\n');
                    }
                }
            }
        }
        // stdout closed: the child is exiting (or was killed). Polled, not
        // `wait()`ed, so `cancel` can still take the lock to kill it.
        let status = loop {
            match child.lock().unwrap().try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) => {}
                Err(_) => break None,
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let err = err_reader.join().unwrap_or_default();
        if timed_out.load(Ordering::Relaxed) {
            if take_if_ours(handle, id, &req_id) {
                emit("error", "timed out".into());
            }
            return;
        }
        // A cancel already removed the entry and wants silence.
        if !take_if_ours(handle, id, &req_id) {
            return;
        }
        if agent == Agent::Codex {
            match (status, result, answer) {
                (_, Some((true, text)), _) => emit("error", text),
                (Some(s), _, Some(text)) if s.success() && !text.trim().is_empty() => {
                    emit("delta", text);
                    emit("done", String::new());
                }
                (Some(s), _, _) => emit("error", headless::codex_failure_reason(s.code(), &plain, &err)),
                (None, _, _) => emit("error", "lost the explaining process".into()),
            }
            return;
        }
        match (status, result) {
            (Some(s), Some((false, _))) if s.success() => emit("done", String::new()),
            (_, Some((true, text))) => emit("error", text),
            (Some(s), _) => emit("error", failure_reason(s.code(), &plain, &err)),
            (None, _) => emit("error", "lost the explaining process".into()),
        }
    });
    Ok(())
}

/// Remove (handle, id)'s entry if it is still `req_id`'s; false when a cancel
/// or a newer request got there first.
fn take_if_ours(handle: ProjectHandle, id: usize, req_id: &str) -> bool {
    let mut g = RUNNING.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    match m.get(&(handle, id)) {
        Some((r, _)) if r == req_id => {
            m.remove(&(handle, id));
            true
        }
        _ => false,
    }
}

/// The session a `system`/`init` line reports — for a `--fork-session` child,
/// the new fork's uuid.
fn init_session(v: &serde_json::Value) -> Option<&str> {
    if v.get("type")?.as_str()? != "system" || v.get("subtype")?.as_str()? != "init" {
        return None;
    }
    v.get("session_id")?.as_str()
}

/// Record the fork uuid `req_id`'s child reported, if the panel's slot is still
/// that request's. When it is not (the panel was closed or replaced mid-run),
/// the file is deleted at once instead — nobody would ever resume it.
fn note_fork(agent: Agent, handle: ProjectHandle, id: usize, req_id: &str, dir: &Path, sid: &str) {
    let mut g = FORKS.lock().unwrap();
    match g.get_or_insert_with(HashMap::new).get_mut(&(handle, id)) {
        Some(f) if f.req_id == req_id => f.uuid = Some(sid.to_string()),
        _ => {
            drop(g);
            delete_fork(agent, dir, sid);
        }
    }
}

/// The text of a `stream_event` / `content_block_delta` / `text_delta` line.
pub(crate) fn text_delta(v: &serde_json::Value) -> Option<&str> {
    if v.get("type")?.as_str()? != "stream_event" {
        return None;
    }
    let ev = v.get("event")?;
    if ev.get("type")?.as_str()? != "content_block_delta" {
        return None;
    }
    let d = ev.get("delta")?;
    if d.get("type")?.as_str()? != "text_delta" {
        return None;
    }
    d.get("text")?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_delta_reads_only_text() {
        let t: serde_json::Value = serde_json::from_str(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"שלום"}}}"#,
        )
        .unwrap();
        assert_eq!(text_delta(&t), Some("שלום"));
        let th: serde_json::Value = serde_json::from_str(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"x"}}}"#,
        )
        .unwrap();
        assert_eq!(text_delta(&th), None);
    }

    #[test]
    fn prompt_carries_the_selection() {
        let p = prompt_for("  idempotent teardown \n");
        assert!(p.contains("idempotent teardown"));
        assert!(!p.contains("{{SELECTION}}"));
    }

    /// Lines measured 2026-10-21 from `codex exec --json`.
    #[test]
    fn codex_events_give_the_thread_and_the_answer() {
        let ev = |l: &str| codex_event(&serde_json::from_str(l).unwrap());
        assert_eq!(
            ev(r#"{"type":"thread.started","thread_id":"01a15d17-f19e-7473-b008-a2d35d0d240d"}"#),
            Some(CodexEvent::Thread("01a15d17-f19e-7473-b008-a2d35d0d240d".into()))
        );
        assert_eq!(
            ev(r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"שלום"}}"#),
            Some(CodexEvent::Message("שלום".into()))
        );
        assert_eq!(ev(r#"{"type":"item.completed","item":{"type":"reasoning","text":"x"}}"#), None);
        assert_eq!(ev(r#"{"type":"turn.completed","usage":{}}"#), None);
        assert_eq!(
            ev(r#"{"type":"turn.failed","error":{"message":"usage limit"}}"#),
            Some(CodexEvent::Failed("usage limit".into()))
        );
    }

    #[test]
    fn init_session_reads_only_init() {
        let init: serde_json::Value =
            serde_json::from_str(r#"{"type":"system","subtype":"init","session_id":"abc"}"#).unwrap();
        assert_eq!(init_session(&init), Some("abc"));
        let status: serde_json::Value =
            serde_json::from_str(r#"{"type":"system","subtype":"status","session_id":"abc"}"#).unwrap();
        assert_eq!(init_session(&status), None);
    }
}
