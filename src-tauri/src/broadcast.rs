//! ⌘⇧B Ask All Claudes: one question to every claude in every open project,
//! a row per claude showing whether it has answered, and a Sonnet summary of
//! the answers once they are in.
//!
//! The question goes out through the hub inbox as the app itself (sender
//! "Mulpex", `Core::send_app_message`) — the ⌘⇧Q path. The answer comes back
//! through the one hub tool that can reach the app, `hub_reply_broadcast`,
//! which writes `broadcast/<bid>/<id>.json` in that project's scratch dir. The
//! tool refuses unless `broadcast/<bid>/open` exists, so only a round this app
//! opened can be answered, and closing the round removes the marker.
//!
//! A claude that reads the question and goes idle without calling the tool is
//! not left hanging: once it has been `waiting` for `SETTLE` with the message
//! read, its last assistant text from the transcript stands in as its answer
//! (`via_tool: false`, "no tool" on its row).
//!
//! One round at a time, in memory only: a new one replaces it, a relaunch
//! forgets it.

use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mulpex_core::agent::Agent;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::headless;
use crate::saves::failure_reason;
use crate::snapshot::ProjectHandle;
use crate::state::Workspace;

const PROMPT: &str = include_str!("broadcast_prompt.md");

/// How long a claude must sit `waiting` with the question read before its last
/// assistant text is taken as its answer. Same settle as ⌘⇧Q's `SETTLE_MS`.
const SETTLE: Duration = Duration::from_millis(1500);

/// Only catches a hung summary child.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How much of a transcript's end the fallback reads.
const TAIL_BYTES: u64 = 512 * 1024;

/// One claude to ask, as the compose view lists it.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub handle: ProjectHandle,
    pub project: String,
    pub id: usize,
    pub name: Option<String>,
    /// Its status word: `working`, `waiting` or `needs`.
    pub status: String,
}

/// Which claude, by (project, instance).
#[derive(Deserialize, Clone, Copy, Debug)]
pub struct Pick {
    pub handle: ProjectHandle,
    pub id: usize,
}

struct Row {
    handle: ProjectHandle,
    project: String,
    id: usize,
    name: Option<String>,
    /// The inbox file; gone once the claude has read the question.
    file: Option<PathBuf>,
    /// Since when it has been idle with the question read (for the fallback).
    idle_since: Option<Instant>,
    /// Set once: the tool's answer, or the fallback's, or "gone".
    done: Option<Done>,
}

#[derive(Clone)]
struct Done {
    state: &'static str,
    reply: Option<String>,
}

pub struct Round {
    bid: String,
    message: String,
    rows: Vec<Row>,
    /// The project state dirs holding this round's `open` marker.
    dirs: Vec<PathBuf>,
}

#[derive(Default)]
pub struct Broadcast {
    round: Option<Round>,
}

/// A row as the progress view shows it. `state` is one of `sent` (unread),
/// `busy` (read, still working or asking), `read` (read, idle, settling),
/// `replied`, `fallback` (answer taken from the transcript), `noanswer` (it
/// went idle with nothing to show) or `gone` (the claude closed).
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RowView {
    pub handle: ProjectHandle,
    pub project: String,
    pub id: usize,
    pub name: Option<String>,
    pub state: &'static str,
    pub reply: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RoundView {
    pub bid: String,
    pub message: String,
    pub rows: Vec<RowView>,
    /// Every row is in a final state.
    pub complete: bool,
}

/// The finished states: nothing more will change for that row.
fn is_final(state: &str) -> bool {
    matches!(state, "replied" | "fallback" | "noanswer" | "gone")
}

/// The state of a row that has not finished yet, from what the backend sees.
/// `status` is the claude's status word (`""` = missing file = `waiting`).
fn live_state(unread: bool, status: &str) -> &'static str {
    if unread {
        "sent"
    } else if matches!(status, "" | "waiting") {
        "read"
    } else {
        "busy"
    }
}

/// The body each claude receives.
fn body_for(bid: &str, message: &str) -> String {
    format!(
        "The user is asking EVERY claude open in Mulpex the same thing (\"Ask All Claudes\"):\n\n\
<question>\n{}\n</question>\n\n\
Handle it as if the user had asked you directly. When you are done, call the \
mcp__mulpex__hub_reply_broadcast tool with id \"{bid}\" and a short, self-contained answer — \
the user reads every claude's answer side by side. This is the one case where you DO answer \
Mulpex: through that tool only, never hub_send. If the question does not concern you, answer \
that in one line.",
        message.trim()
    )
}

fn new_bid() -> String {
    mulpex_core::persist::new_uuid().chars().filter(|c| *c != '-').take(12).collect()
}

/// The last assistant text in a transcript of `agent`, read from its end.
fn last_assistant_text(agent: Agent, path: &Path) -> Option<String> {
    crate::agent_transcript::last_assistant_text(agent, path, TAIL_BYTES)
        .map(|t| strip_hub_marker(&t))
        .filter(|t| !t.is_empty())
}

/// Drop the "⟳ hub message from … →" line `HUB_RULES` has a hub-woken turn
/// start with — it is plumbing, not part of the answer.
fn strip_hub_marker(text: &str) -> String {
    let t = text.trim_start();
    match t.split_once('\n') {
        Some((first, rest)) if first.starts_with("⟳ hub message") => rest.trim().to_string(),
        None if t.starts_with("⟳ hub message") => String::new(),
        _ => t.trim().to_string(),
    }
}

fn read_reply(dir: &Path, id: usize) -> Option<String> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{id}.json"))).ok()?).ok()?;
    v.get("answer")?.as_str().map(str::to_string)
}

impl Workspace {
    /// Every claude that can take a message right now, in every project.
    pub fn broadcast_targets(&self) -> Vec<Target> {
        let mut out = Vec::new();
        for core in &self.projects {
            for id in core.live_claude_ids() {
                let word = std::fs::read_to_string(core.state_dir.join(id.to_string())).unwrap_or_default();
                let word = word.trim();
                out.push(Target {
                    handle: core.handle,
                    project: core.project_name.clone(),
                    id,
                    name: core.label_of(id),
                    status: if word.is_empty() { "waiting".into() } else { word.to_string() },
                });
            }
        }
        out
    }

    /// Ask `picks` the question, replacing any earlier round. Returns its id.
    pub fn broadcast_send(&mut self, message: &str, picks: &[Pick]) -> Result<String, String> {
        if message.trim().is_empty() {
            return Err("Write a question first".into());
        }
        if picks.is_empty() {
            return Err("Pick at least one claude".into());
        }
        self.broadcast_cancel();
        let bid = new_bid();
        let body = body_for(&bid, message);
        let mut rows = Vec::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        for p in picks {
            let Some(core) = self.project(p.handle) else { continue };
            if !core.live_claude_ids().contains(&p.id) {
                continue;
            }
            let Some(dir) = mulpex_core::broadcast_dir(&core.state_dir, &bid) else { continue };
            if !dirs.contains(&dir) {
                std::fs::create_dir_all(&dir)
                    .and_then(|_| std::fs::write(dir.join("open"), ""))
                    .map_err(|e| format!("could not open the round: {e}"))?;
                dirs.push(dir);
            }
            rows.push(Row {
                handle: core.handle,
                project: core.project_name.clone(),
                id: p.id,
                name: core.label_of(p.id),
                file: core.send_app_message(p.id, &body),
                idle_since: None,
                done: None,
            });
        }
        if rows.is_empty() {
            for d in &dirs {
                let _ = std::fs::remove_dir_all(d);
            }
            return Err("None of the picked claudes is running any more".into());
        }
        self.broadcast.round = Some(Round { bid: bid.clone(), message: message.trim().to_string(), rows, dirs });
        Ok(bid)
    }

    /// Recompute every row and return the round, or `None` when there is none.
    pub fn broadcast_progress(&mut self) -> Option<RoundView> {
        let mut round = self.broadcast.round.take()?;
        let bid = round.bid.clone();
        for r in &mut round.rows {
            if r.done.is_some() {
                continue;
            }
            let Some(core) = self.project(r.handle) else {
                r.done = Some(Done { state: "gone", reply: None });
                continue;
            };
            if let Some(reply) = mulpex_core::broadcast_dir(&core.state_dir, &bid)
                .and_then(|d| read_reply(&d, r.id))
            {
                r.done = Some(Done { state: "replied", reply: Some(reply) });
                continue;
            }
            if !core.live_claude_ids().contains(&r.id) {
                r.done = Some(Done { state: "gone", reply: None });
                continue;
            }
            let unread = r.file.as_ref().is_some_and(|f| f.exists());
            let word = std::fs::read_to_string(core.state_dir.join(r.id.to_string())).unwrap_or_default();
            if live_state(unread, word.trim()) != "read" {
                r.idle_since = None;
                continue;
            }
            let since = *r.idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= SETTLE {
                let agent = core.agent_of(r.id).unwrap_or_default();
                let text = core.transcript_of(r.id).and_then(|p| last_assistant_text(agent, &p));
                r.done = Some(match text {
                    Some(t) => Done { state: "fallback", reply: Some(t) },
                    None => Done { state: "noanswer", reply: None },
                });
            }
        }
        let view = self.view_of(&round);
        self.broadcast.round = Some(round);
        Some(view)
    }

    fn view_of(&self, round: &Round) -> RoundView {
        let rows: Vec<RowView> = round
            .rows
            .iter()
            .map(|r| {
                let (state, reply) = match &r.done {
                    Some(d) => (d.state, d.reply.clone()),
                    None => {
                        let unread = r.file.as_ref().is_some_and(|f| f.exists());
                        let word = self
                            .project(r.handle)
                            .and_then(|c| std::fs::read_to_string(c.state_dir.join(r.id.to_string())).ok())
                            .unwrap_or_default();
                        (live_state(unread, word.trim()), None)
                    }
                };
                RowView {
                    handle: r.handle,
                    project: r.project.clone(),
                    id: r.id,
                    name: r.name.clone(),
                    state,
                    reply,
                }
            })
            .collect();
        RoundView {
            bid: round.bid.clone(),
            message: round.message.clone(),
            complete: rows.iter().all(|r| is_final(r.state)),
            rows,
        }
    }

    /// End the round: unread questions are taken back, the `open` markers go
    /// (so a late `hub_reply_broadcast` is refused), and a running summary is
    /// killed.
    pub fn broadcast_cancel(&mut self) {
        cancel_summary();
        let Some(round) = self.broadcast.round.take() else { return };
        for r in &round.rows {
            if let Some(f) = &r.file {
                let _ = std::fs::remove_file(f);
            }
        }
        for d in &round.dirs {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// The summary prompt for the current round, over whatever has arrived.
    pub fn broadcast_summary_prompt(&self) -> Result<(String, String), String> {
        let round = self.broadcast.round.as_ref().ok_or("Nothing was asked")?;
        let view = self.view_of(round);
        let mut answers = String::new();
        let mut silent = Vec::new();
        for r in &view.rows {
            let who = match &r.name {
                Some(n) => format!("{} · claude#{} ({n})", r.project, r.id),
                None => format!("{} · claude#{}", r.project, r.id),
            };
            match &r.reply {
                Some(text) => answers.push_str(&format!("<answer from=\"{who}\">\n{}\n</answer>\n\n", text.trim())),
                None => silent.push(format!("{who}: {}", r.state)),
            }
        }
        if answers.is_empty() {
            return Err("No answers yet".into());
        }
        let silent = if silent.is_empty() { "(none)".to_string() } else { silent.join("\n") };
        let prompt = PROMPT
            .replace("{{QUESTION}}", &round.message)
            .replace("{{SILENT}}", &silent)
            .replace("{{ANSWERS}}", &answers);
        Ok((round.bid.clone(), prompt))
    }
}

// ---- the summary ------------------------------------------------------------

/// `broadcast-summary` payload: `kind` is `delta` (`text` = the next piece),
/// `done`, or `error` (`text` = the reason). `req_id` lets the frontend drop
/// events from a replaced summary.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SummaryEvent {
    req_id: String,
    kind: &'static str,
    text: String,
}

static RUNNING: Mutex<Option<(String, Arc<Mutex<Child>>)>> = Mutex::new(None);

pub fn cancel_summary() {
    if let Some((_, child)) = RUNNING.lock().unwrap().take() {
        let _ = child.lock().unwrap().kill();
    }
}

fn take_if_ours(req_id: &str) -> bool {
    let mut g = RUNNING.lock().unwrap();
    if g.as_ref().is_some_and(|(r, _)| r == req_id) {
        *g = None;
        true
    } else {
        false
    }
}

/// Run Sonnet over `prompt` and stream its answer as `broadcast-summary`
/// events. Same headless child as ⌘⇧E (`explain.rs`), minus the fork: no
/// tools, no session saved, scrubbed env. Without claude installed, codex
/// writes it (`headless::preferred_agent`) — one chunk at the end, since its
/// `--json` stream has no text deltas.
pub fn summarize(app: AppHandle, req_id: String, prompt: String) -> Result<(), String> {
    cancel_summary();
    let agent = headless::preferred_agent();
    let mut cmd = headless::command(agent, &std::env::temp_dir(), None)?;
    match agent {
        Agent::Claude => cmd.args([
            "-p",
            "--model",
            "sonnet",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--tools",
            "",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
        ]),
        Agent::Codex => cmd.arg("exec").args(headless::codex_exec_opts(true)).args(["--json", "-"]),
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
    *RUNNING.lock().unwrap() = Some((req_id.clone(), child.clone()));

    let timed_out = Arc::new(AtomicBool::new(false));
    {
        let (child, timed_out) = (child.clone(), timed_out.clone());
        std::thread::spawn(move || {
            std::thread::sleep(SUMMARY_TIMEOUT);
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
            let _ = app.emit("broadcast-summary", SummaryEvent { req_id: req_id.clone(), kind, text });
        };
        let mut plain = String::new();
        let mut result: Option<(bool, String)> = None;
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                match serde_json::from_str::<serde_json::Value>(&line) {
                    Ok(v) if agent == Agent::Codex => match crate::explain::codex_event(&v) {
                        Some(crate::explain::CodexEvent::Message(t)) => result = Some((false, t)),
                        Some(crate::explain::CodexEvent::Failed(t)) => result = Some((true, t)),
                        _ => {}
                    },
                    Ok(v) => {
                        if let Some(t) = crate::explain::text_delta(&v) {
                            emit("delta", t.to_string());
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
        let status = loop {
            match child.lock().unwrap().try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) => {}
                Err(_) => break None,
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let err = err_reader.join().unwrap_or_default();
        if !take_if_ours(&req_id) {
            return;
        }
        if timed_out.load(Ordering::Relaxed) {
            emit("error", "timed out".into());
            return;
        }
        match (status, result) {
            (Some(s), Some((false, text))) if s.success() => {
                if agent == Agent::Codex {
                    emit("delta", text);
                }
                emit("done", String::new())
            }
            (_, Some((true, text))) => emit("error", text),
            (Some(s), _) if agent == Agent::Codex => {
                emit("error", headless::codex_failure_reason(s.code(), &plain, &err))
            }
            (Some(s), _) => emit("error", failure_reason(s.code(), &plain, &err)),
            (None, _) => emit("error", "lost the summarizing process".into()),
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_bin;
    use std::process::Command;

    #[test]
    fn row_states() {
        assert_eq!(live_state(true, "working"), "sent");
        assert_eq!(live_state(true, ""), "sent");
        assert_eq!(live_state(false, ""), "read");
        assert_eq!(live_state(false, "waiting"), "read");
        assert_eq!(live_state(false, "working"), "busy");
        assert_eq!(live_state(false, "needs"), "busy");
        for s in ["replied", "fallback", "noanswer", "gone"] {
            assert!(is_final(s));
        }
        for s in ["sent", "read", "busy"] {
            assert!(!is_final(s));
        }
    }

    #[test]
    fn the_body_names_the_tool_and_the_id() {
        let b = body_for("abc123", "  who touched auth?  ");
        assert!(b.contains("who touched auth?"));
        assert!(b.contains("hub_reply_broadcast"));
        assert!(b.contains("\"abc123\""));
        assert!(b.contains("never hub_send"));
    }

    #[test]
    fn a_bid_is_a_valid_broadcast_dir() {
        let bid = new_bid();
        assert_eq!(bid.len(), 12);
        assert!(mulpex_core::broadcast_dir(Path::new("/x"), &bid).is_some());
    }

    #[test]
    fn the_fallback_takes_the_last_assistant_text() {
        let dir = std::env::temp_dir().join(format!("mulpex-bc-{}", new_bid()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let lines = [
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"old answer"}]}}"#,
            r#"{"type":"user","origin":{"kind":"human"},"message":{"content":"q"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Bash","input":{}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"I touched auth.rs"}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"subagent"}]}}"#,
        ];
        std::fs::write(&p, lines.join("\n")).unwrap();
        assert_eq!(last_assistant_text(Agent::Claude, &p).as_deref(), Some("I touched auth.rs"));
        assert_eq!(last_assistant_text(Agent::Claude, &dir.join("missing")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// END-TO-END against real `claude`s: two projects, one claude each, a real
    /// round through the inbox, the real helper's `hub_reply_broadcast`, and the
    /// real Sonnet summary. `#[ignore]`d — it burns model turns. Run alone, after
    /// `cargo build -p mulpex-helper`:
    ///   cargo test -p mulpex --lib -- --ignored --nocapture --exact \
    ///     broadcast::tests::a_real_round_end_to_end
    #[test]
    #[ignore]
    fn a_real_round_end_to_end() {
        use crate::state::Core;
        let root = std::env::temp_dir().join(format!("mulpex-bc-e2e-{}", new_bid()));
        let cfg = root.join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        std::env::set_var("MULPEX_HOME", root.join("mulpex-home"));
        let mut trusted = serde_json::Map::new();
        let dirs: Vec<PathBuf> = ["alpha", "beta"].iter().map(|n| root.join(n)).collect();
        for d in &dirs {
            std::fs::create_dir_all(d).unwrap();
            let c = std::fs::canonicalize(d).unwrap();
            trusted.insert(c.to_string_lossy().into(), serde_json::json!({ "hasTrustDialogAccepted": true }));
        }
        std::fs::write(
            cfg.join(".claude.json"),
            serde_json::json!({ "hasCompletedOnboarding": true, "lastOnboardingVersion": "99.0.0",
                "theme": "dark", "projects": trusted })
            .to_string(),
        )
        .unwrap();
        std::fs::write(cfg.join("settings.json"), r#"{"skipDangerousModePermissionPrompt":true}"#).unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &cfg);
        let built = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("target/debug/mulpex-helper");
        let helper = root.join("mulpex-helper");
        std::fs::copy(&built, &helper).expect("build mulpex-helper first");

        let mut ws = Workspace::new();
        for (i, d) in dirs.iter().enumerate() {
            let h = (i + 1) as u64;
            let mut core = Core::open(h, d.clone(), &helper, root.join(format!("state{h}")), (100, 40)).unwrap();
            core.spawn_instance().unwrap();
            ws.projects.push(core);
        }
        // Let both boot and their hub listeners come up.
        let up = (0..120).any(|_| {
            std::thread::sleep(Duration::from_millis(500));
            ws.projects.iter().all(|c| c.live_claude_ids().iter().all(|&id| c.wake_ready(id)))
        });
        eprintln!("listeners up: {up}");
        let targets = ws.broadcast_targets();
        assert_eq!(targets.len(), 2, "{targets:?}");
        let picks: Vec<Pick> = targets.iter().map(|t| Pick { handle: t.handle, id: t.id }).collect();
        let bid = ws
            .broadcast_send("What is the name of the directory you are working in? One line.", &picks)
            .unwrap();
        eprintln!("sent round {bid}");

        let mut last = None;
        for _ in 0..360 {
            std::thread::sleep(Duration::from_millis(500));
            let v = ws.broadcast_progress().unwrap();
            let states: Vec<_> = v.rows.iter().map(|r| r.state).collect();
            if last.as_ref() != Some(&states) {
                eprintln!("{states:?}");
                last = Some(states);
            }
            if v.complete {
                break;
            }
        }
        let v = ws.broadcast_progress().unwrap();
        for r in &v.rows {
            eprintln!("{} claude#{} {} → {:?}", r.project, r.id, r.state, r.reply);
        }
        assert!(v.complete, "the round never completed");
        assert!(v.rows.iter().all(|r| r.reply.is_some()), "a claude gave no answer");
        assert!(v.rows.iter().any(|r| r.state == "replied"), "no claude used the tool");

        // The summary prompt, through the same headless Sonnet flags.
        let (_, prompt) = ws.broadcast_summary_prompt().unwrap();
        let mut child = Command::new(claude_bin::resolve_claude().unwrap())
            .args(["-p", "--model", "sonnet", "--setting-sources", "", "--strict-mcp-config",
                "--no-session-persistence", "--tools", ""])
            .env_clear()
            .envs(claude_bin::forwarded_env())
            .env("PATH", claude_bin::merged_path())
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(prompt.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        let summary = String::from_utf8_lossy(&out.stdout);
        eprintln!("--- summary ---\n{summary}");
        assert!(out.status.success() && summary.contains("alpha") && summary.contains("beta"));

        // A new round closes the old one: its marker goes, so a late answer is refused.
        let open = mulpex_core::broadcast_dir(&ws.projects[0].state_dir, &bid).unwrap();
        assert!(open.join("open").exists());

        // The fallback: a claude that answers in text and never calls the tool.
        ws.broadcast_send(
            "Test of the fallback path: this time do NOT call hub_reply_broadcast or any other tool. \
             Just write the word 'banana' as your reply text and end your turn.",
            &picks,
        )
        .unwrap();
        assert!(!open.exists(), "the old round's marker survived a new round");
        for _ in 0..360 {
            std::thread::sleep(Duration::from_millis(500));
            if ws.broadcast_progress().unwrap().complete {
                break;
            }
        }
        let v = ws.broadcast_progress().unwrap();
        for r in &v.rows {
            eprintln!("round 2: {} claude#{} {} → {:?}", r.project, r.id, r.state, r.reply);
        }
        assert!(v.complete);
        assert!(v.rows.iter().all(|r| r.reply.as_deref().is_some_and(|t| t.to_lowercase().contains("banana"))));
        ws.broadcast_cancel();
        for c in &mut ws.projects {
            c.teardown();
        }
        std::env::remove_var("MULPEX_HOME");
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Measured in `a_real_round_end_to_end`: the fallback text came back as
    /// "⟳ hub message from Mulpex →\n\nbanana".
    #[test]
    fn the_hub_marker_is_not_part_of_the_answer() {
        assert_eq!(strip_hub_marker("⟳ hub message from Mulpex →\n\nbanana"), "banana");
        assert_eq!(strip_hub_marker("⟳ hub message from Mulpex →"), "");
        assert_eq!(strip_hub_marker("plain\nanswer"), "plain\nanswer");
    }

    #[test]
    fn the_prompt_has_every_placeholder() {
        for k in ["{{QUESTION}}", "{{ANSWERS}}", "{{SILENT}}"] {
            assert!(PROMPT.contains(k), "{k}");
        }
    }
}
