//! The Explainer: after each claude turn ends, a cheap Sonnet call produces a
//! very short, very simple **Hebrew** explanation of what that claude said, for
//! the right-hand Explainer panel.
//!
//! The turn-end signal is the `Stop` hook writing `explainreq/<id>` (the turn's
//! transcript path — see `hook::write_explain_request`); the poll loop consumes
//! it (`Core::take_explain_requests`) and hands the job here. Everything after
//! that happens on this module's own worker threads: read the transcript,
//! extract the finished turn's assistant text, run `claude -p --model sonnet`
//! headless, append the result to the in-memory feed and emit `explain-update`.
//! Nothing here may ever run inside a hook (a Stop hook blocks the claude's
//! turn end) or block the 200 ms poll loop.
//!
//! The summarizer child is a plain process, not a PTY session, and must stay a
//! nobody: `env_clear()` + `claude_bin::forwarded_env()` (whose deny-list
//! strips `MULPEX_*` / `CLAUDE_CODE_CHILD_SESSION` / `CLAUDE_CODE_ENTRYPOINT`)
//! keeps it off the hub, and `--setting-sources "" --tools "" --strict-mcp-config
//! --no-session-persistence` keeps it cheap, quiet and diskless. NOT `--bare`:
//! measured 2026-08-30, `--bare -p` cannot see the subscription OAuth token and
//! dies with "Not logged in".

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::claude_bin;
use crate::snapshot::{ExplainEntry, ExplainKind, ExplainPending, ExplainUpdate, ProjectHandle};

/// The system prompt of the summarizer. Hebrew because the panel is for reading
/// at a glance; English identifiers stay as-is because translating them is how
/// you lose track of what the claude actually touched. Tone/length approved on
/// real turns (probe0, 2026-08-30).
const HEBREW_PROMPT: &str = "אתה \"המסביר\" של Mulpex. תקבל את הטקסט שכתב Claude Code בסיום תור עבודה.\n\
כתוב הסבר קצר בעברית פשוטה מאוד — משפט אחד עד שלושה, כמו הסבר בעל־פה לחבר.\n\
כללים:\n\
- מונחים טכניים, שמות קבצים, פקודות ושמות functions/branches נשארים באנגלית כמו שהם.\n\
- בלי הקדמה, בלי \"לסיכום\", בלי כותרות ובלי bullet points. רק ההסבר עצמו.\n\
- טקסט פשוט בלבד — בלי סימוני markdown: בלי **, בלי #, בלי `backticks`.\n\
- אם Claude שואל שאלה או מחכה להחלטה — פתח בזה: מה הוא צריך ממך עכשיו.\n\
- אם Claude נכשל או נתקע — אמור את זה ישירות.\n\
- אל תוסיף שום דבר שלא מופיע בטקסט.";

/// The question-mode prompt: Claude stopped mid-turn on `AskUserQuestion` and
/// the panel should say what is being asked and what each option means, while
/// the question sits on screen waiting.
const QUESTION_PROMPT: &str = "אתה \"המסביר\" של Mulpex. Claude עצר באמצע העבודה ושואל את המשתמש שאלה לפני שימשיך.\n\
תקבל את השאלות והאפשרויות. הסבר בעברית פשוטה מאוד: מה הוא שואל, ומה המשמעות של כל אפשרות — משפט קצר לכל אחת.\n\
כללים:\n\
- מונחים טכניים, שמות קבצים, פקודות ושמות functions/branches נשארים באנגלית כמו שהם.\n\
- בלי הקדמה ובלי סיכום. שורה לשאלה, ואז שורה קצרה לכל אפשרות שמתחילה ב\"- \".\n\
- טקסט פשוט בלבד — בלי סימוני markdown: בלי **, בלי #, בלי `backticks`.\n\
- סמן אפשרות כמומלצת רק אם כתוב בה (Recommended) במפורש. אם לא כתוב — אל תסמן שום אפשרות, ואל תרמוז מה עדיף.\n\
- אל תוסיף שום דבר שלא מופיע בשאלות.";

/// Feed cap per instance. The panel is a "what just happened" glance, not an
/// archive; the transcript itself is the archive.
const MAX_ENTRIES: usize = 50;

/// Cap on the turn text handed to Sonnet, keeping the **tail** — the end of a
/// turn is where conclusions live. Byte-based, trimmed to a char boundary.
const MAX_TURN_BYTES: usize = 24_000;

/// Summarizer wall-clock budget. Measured runs take 6–13 s; anything past this
/// is a hang, and the turn gets a failure entry rather than a stuck queue.
const SUMMARIZER_TIMEOUT: Duration = Duration::from_secs(90);

/// Two turns routinely end together (test the plural); more workers than this
/// just races more Sonnet calls for a panel the user reads one row at a time.
const WORKERS: usize = 2;

/// The Stop hook races Claude Code's own transcript flush: measured 2026-08-30,
/// the turn's final assistant entry hit the JSONL at 11:55:15.346 while the
/// Stop hook fired inside the same second — the first read then sees the turn's
/// boundary but no text yet. So: wait once before reading, and while the turn
/// reads as empty keep retrying a little; a turn still empty after ~2.5s is a
/// genuinely textless turn (an interrupt, tool-calls-only) and is skipped.
const EXTRACT_DELAY: Duration = Duration::from_millis(400);
const EXTRACT_RETRIES: usize = 6;

/// What a job explains. Turn and Question jobs for the same instance are
/// distinct — a pending question must not be coalesced away by the turn that
/// eventually follows it, nor vice versa.
enum Input {
    /// A finished turn: read this transcript and summarize it.
    Turn { transcript: PathBuf },
    /// A pending `AskUserQuestion`: the tool's `tool_input` JSON.
    Question { json: String },
}

struct Job {
    handle: ProjectHandle,
    id: usize,
    input: Input,
    /// The project's scratch dir — a neutral cwd for the summarizer child.
    cwd: PathBuf,
}

struct Inner {
    queue: Mutex<VecDeque<Job>>,
    wake: Condvar,
    /// `(handle, id)` → that instance's feed, newest first.
    store: Mutex<HashMap<(ProjectHandle, usize), Vec<ExplainEntry>>>,
    /// `(handle, id)` → jobs queued or running for it — the panel's busy dot.
    /// A count, not a bool: a running job plus a freshly queued one must not go
    /// idle when only the first finishes. `explain-pending` fires on the 0↔1+
    /// transitions only.
    pending: Mutex<HashMap<(ProjectHandle, usize), u32>>,
}

static INNER: OnceLock<Arc<Inner>> = OnceLock::new();
static APP: OnceLock<AppHandle> = OnceLock::new();

fn inner() -> &'static Arc<Inner> {
    INNER.get_or_init(|| {
        Arc::new(Inner {
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            store: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        })
    })
}

/// Start the worker threads. Called once from `hub::start`; the handle is what
/// lets a worker emit `explain-update` the moment its summary lands.
pub fn init(app: AppHandle) {
    if APP.set(app).is_err() {
        return; // already running
    }
    for _ in 0..WORKERS {
        let inner = inner().clone();
        std::thread::spawn(move || loop {
            let job = {
                let mut q = inner.queue.lock().unwrap();
                loop {
                    if let Some(job) = q.pop_front() {
                        break job;
                    }
                    q = inner.wake.wait(q).unwrap();
                }
            };
            let (handle, id) = (job.handle, job.id);
            process(&inner, job);
            // Every path through process — entry, failure entry, skip — ends
            // the job; the busy dot must never outlive it.
            finish_pending(&inner, handle, id);
        });
    }
}

/// Queue one finished turn. Latest-wins per instance for jobs not yet running:
/// if a queued job for the same `(handle, id)` exists it is replaced — its
/// transcript is the same file, and the newer request supersedes it exactly the
/// way `explainreq/<id>` overwrites between polls. A job already *running* is
/// not touched; the new one queues behind it.
pub fn submit(handle: ProjectHandle, id: usize, transcript: String, cwd: PathBuf) {
    enqueue(handle, id, Input::Turn { transcript: PathBuf::from(transcript) }, cwd);
}

/// Queue one pending `AskUserQuestion` (the `tool_input` JSON). Same latest-wins
/// contract, within its own kind.
pub fn submit_question(handle: ProjectHandle, id: usize, json: String, cwd: PathBuf) {
    enqueue(handle, id, Input::Question { json }, cwd);
}

fn enqueue(handle: ProjectHandle, id: usize, input: Input, cwd: PathBuf) {
    let kind = std::mem::discriminant(&input);
    let job = Job { handle, id, input, cwd };
    let inner = inner();
    let mut q = inner.queue.lock().unwrap();
    let replaced = if let Some(existing) = q.iter_mut().find(|j| {
        j.handle == handle && j.id == id && std::mem::discriminant(&j.input) == kind
    }) {
        *existing = job;
        true
    } else {
        q.push_back(job);
        false
    };
    drop(q);
    inner.wake.notify_one();
    // A replaced job was already counted; only a genuinely new one raises the
    // pending count (and lights the busy dot on the 0→1 edge).
    if !replaced {
        let mut p = inner.pending.lock().unwrap();
        let c = p.entry((handle, id)).or_insert(0);
        *c += 1;
        let lit = *c == 1;
        drop(p);
        if lit {
            emit_pending(handle, id, true);
        }
    }
}

/// One job ended (summary, failure, or skip): drop its pending count, and dark
/// the busy dot on the 1→0 edge. An entry already cleared by `forget` stays
/// cleared — no negative counts, no spurious re-emit.
fn finish_pending(inner: &Inner, handle: ProjectHandle, id: usize) {
    let mut p = inner.pending.lock().unwrap();
    let mut dark = false;
    if let Some(c) = p.get_mut(&(handle, id)) {
        *c = c.saturating_sub(1);
        if *c == 0 {
            p.remove(&(handle, id));
            dark = true;
        }
    }
    drop(p);
    if dark {
        emit_pending(handle, id, false);
    }
}

fn emit_pending(handle: ProjectHandle, id: usize, active: bool) {
    if let Some(app) = APP.get() {
        let _ = app.emit("explain-pending", ExplainPending { handle, id, active });
    }
}

/// Drop one instance's feed (its row is gone from the sidebar, so the feed is
/// unreachable) and any queued job for it. A job already mid-run may still
/// finish and deposit one zombie entry; it is invisible (the frontend keys the
/// panel by live rows) and freed with the project.
pub fn forget(handle: ProjectHandle, id: usize) {
    let inner = inner();
    inner.queue.lock().unwrap().retain(|j| !(j.handle == handle && j.id == id));
    inner.store.lock().unwrap().remove(&(handle, id));
    // The row is gone; so is its busy dot. A still-running job's own
    // finish_pending later finds no entry and stays silent.
    if inner.pending.lock().unwrap().remove(&(handle, id)).is_some() {
        emit_pending(handle, id, false);
    }
}

/// Drop everything a closed project accumulated.
pub fn forget_project(handle: ProjectHandle) {
    let inner = inner();
    inner.queue.lock().unwrap().retain(|j| j.handle != handle);
    inner.store.lock().unwrap().retain(|(h, _), _| *h != handle);
    inner.pending.lock().unwrap().retain(|(h, _), _| *h != handle);
}

/// The whole feed of one project, for the frontend's initial paint (bootstrap /
/// dev hot-reload). Per instance newest-first; instances in arbitrary order —
/// the frontend groups by `entry.id` anyway.
pub fn feed(handle: ProjectHandle) -> Vec<ExplainEntry> {
    let store = inner().store.lock().unwrap();
    let mut out = Vec::new();
    for ((h, _), entries) in store.iter() {
        if *h == handle {
            out.extend(entries.iter().cloned());
        }
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One job, end to end. An empty input — a turn with no assistant text (an
/// interrupt, a tool-calls-only turn) or an unparseable question payload — is
/// skipped: there is nothing to explain, and a Sonnet call on nothing would
/// invent something. The skip is logged: a silent branch here already cost a
/// debugging round (the flush race above looked like the whole feature not
/// working).
fn process(inner: &Inner, job: Job) {
    let (kind, prompt, text) = match &job.input {
        Input::Turn { transcript } => match extract_turn_text_settled(transcript) {
            Some(t) => (ExplainKind::Turn, HEBREW_PROMPT, t),
            None => {
                eprintln!(
                    "[explainer] project {} claude#{}: turn skipped (no assistant text in {})",
                    job.handle,
                    job.id,
                    transcript.display()
                );
                return;
            }
        },
        Input::Question { json } => match question_text(json) {
            Some(t) => (ExplainKind::Question, QUESTION_PROMPT, t),
            None => {
                eprintln!(
                    "[explainer] project {} claude#{}: question skipped (unparseable payload)",
                    job.handle, job.id
                );
                return;
            }
        },
    };
    let entry = match run_summarizer(prompt, &text, &job.cwd) {
        Ok(text) => ExplainEntry { id: job.id, ts: now_ms(), text, ok: true, kind },
        Err(why) => ExplainEntry {
            id: job.id,
            ts: now_ms(),
            text: format!("ההסבר נכשל ({why})"),
            ok: false,
            kind,
        },
    };
    // Dev-visible trace (stderr): the one place a summary — or its failure — can
    // be seen before the panel exists, and after it exists the place to check
    // when the panel shows nothing.
    eprintln!("[explainer] project {} claude#{}: {}", job.handle, job.id, entry.text);
    push_entry(inner, job.handle, job.id, entry.clone());
    if let Some(app) = APP.get() {
        let _ = app.emit("explain-update", ExplainUpdate { handle: job.handle, id: job.id, entry });
    }
}

/// Append to one instance's feed: newest first, capped.
fn push_entry(inner: &Inner, handle: ProjectHandle, id: usize, entry: ExplainEntry) {
    let mut store = inner.store.lock().unwrap();
    let feed = store.entry((handle, id)).or_default();
    feed.insert(0, entry);
    feed.truncate(MAX_ENTRIES);
}

/// Run the headless Sonnet call, input text on stdin, `prompt` choosing the
/// turn or question persona. See the module docs for why each flag is there and
/// why `--bare` is not.
fn run_summarizer(prompt: &str, turn: &str, cwd: &Path) -> Result<String, String> {
    let claude = claude_bin::resolve_claude().ok_or("claude not found")?;
    let mut child = Command::new(claude)
        .args([
            "-p",
            "--setting-sources",
            "",
            "--model",
            "sonnet",
            "--no-session-persistence",
            "--tools",
            "",
            "--strict-mcp-config",
            "--system-prompt",
            prompt,
        ])
        .env_clear()
        .envs(claude_bin::forwarded_env())
        .env("PATH", claude_bin::merged_path())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;

    // Feed stdin and close it, or `-p` waits for EOF forever.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(turn.as_bytes());
    }
    // Drain both pipes on their own threads: Sonnet's output fits a pipe buffer
    // today, but a child blocked on a full pipe while we only `try_wait` would
    // read as a "timeout" — the classic self-inflicted deadlock.
    let stdout = child.stdout.take();
    let out_reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut f) = stdout {
            use std::io::Read as _;
            let _ = f.read_to_string(&mut s);
        }
        s
    });
    let stderr = child.stderr.take();
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut f) = stderr {
            use std::io::Read as _;
            let _ = f.read_to_string(&mut s);
        }
        s
    });

    let deadline = std::time::Instant::now() + SUMMARIZER_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait(); // reap; a zombie would pin the pid
                return Err(format!("timeout after {}s", SUMMARIZER_TIMEOUT.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(format!("wait: {e}")),
        }
    };
    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();

    if !status.success() {
        let code = status.code().map_or("killed".into(), |c| format!("exit {c}"));
        let first = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        return Err(if first.is_empty() { code } else { format!("{code}: {first}") });
    }
    let text = out.trim();
    if text.is_empty() {
        return Err("empty output".into());
    }
    Ok(text.to_string())
}

/// Is this transcript entry a real human prompt — a turn boundary? Measured on
/// real transcripts (probe0): tool results also arrive as `type:"user"` but
/// carry a `tool_result` block; local-command caveats carry `isMeta:true`; a
/// sidechain's entries carry `isSidechain:true`. A `<task-notification>` turn
/// arrives as a plain string user entry and deliberately counts — a
/// notification-triggered turn gets its own explanation.
fn is_real_user_prompt(e: &Value) -> bool {
    if e.get("type").and_then(Value::as_str) != Some("user")
        || e.get("isMeta").and_then(Value::as_bool).unwrap_or(false)
        || e.get("isSidechain").and_then(Value::as_bool).unwrap_or(false)
    {
        return false;
    }
    match e.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => {
            let mut prompt_like = false;
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("tool_result") => return false,
                    Some("text") | Some("image") => prompt_like = true,
                    _ => {}
                }
            }
            prompt_like
        }
        _ => false,
    }
}

/// Flatten an `AskUserQuestion` `tool_input` into plain text for the
/// summarizer: each question with its options and descriptions, `(Recommended)`
/// labels kept verbatim so the prompt's rule can point at them. `None` when the
/// payload has no usable questions.
fn question_text(json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    let mut out = String::new();
    for (i, q) in v.get("questions")?.as_array()?.iter().enumerate() {
        let question = q.get("question").and_then(Value::as_str).unwrap_or("");
        if question.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("Question {}: {question}\n", i + 1));
        if q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false) {
            out.push_str("(multiple answers allowed)\n");
        }
        out.push_str("Options:\n");
        for o in q.get("options").and_then(Value::as_array).unwrap_or(&Vec::new()) {
            let label = o.get("label").and_then(Value::as_str).unwrap_or("");
            let desc = o.get("description").and_then(Value::as_str).unwrap_or("");
            out.push_str(&format!("- {label}: {desc}\n"));
        }
    }
    let trimmed = out.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// `extract_turn_text` with the flush race absorbed: an initial pause (the
/// final assistant entry lands within ~a second of the Stop hook), then retries
/// while the turn reads as empty. Runs on a worker thread — the waiting costs
/// nobody anything.
fn extract_turn_text_settled(path: &Path) -> Option<String> {
    std::thread::sleep(EXTRACT_DELAY);
    for _ in 0..EXTRACT_RETRIES {
        if let Some(turn) = extract_turn_text(path) {
            return Some(turn);
        }
        std::thread::sleep(EXTRACT_DELAY);
    }
    None
}

/// The finished turn's assistant text: every `text` block of every
/// non-sidechain assistant entry after the last real user prompt, joined. Tail-
/// capped at `MAX_TURN_BYTES`. `None` when there is nothing to explain.
fn extract_turn_text(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let entries: Vec<Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let boundary = entries.iter().rposition(is_real_user_prompt)?;
    let mut texts: Vec<&str> = Vec::new();
    for e in &entries[boundary + 1..] {
        if e.get("type").and_then(Value::as_str) != Some("assistant")
            || e.get("isSidechain").and_then(Value::as_bool).unwrap_or(false)
        {
            continue;
        }
        let Some(Value::Array(blocks)) = e.get("message").and_then(|m| m.get("content")) else {
            continue;
        };
        for b in blocks {
            if b.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    if !t.trim().is_empty() {
                        texts.push(t);
                    }
                }
            }
        }
    }
    let mut joined = texts.join("\n\n");
    if joined.trim().is_empty() {
        return None;
    }
    if joined.len() > MAX_TURN_BYTES {
        let mut cut = joined.len() - MAX_TURN_BYTES;
        while !joined.is_char_boundary(cut) {
            cut += 1;
        }
        joined = format!("[…truncated…]\n{}", &joined[cut..]);
    }
    Some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_transcript(tag: &str, lines: &[&str]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("mulpex-explain-{tag}.jsonl"));
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    /// The shape measured on real transcripts (probe0): the boundary is the last
    /// *human* prompt — a `tool_result` "user" entry is mid-turn plumbing, a
    /// meta entry is a caveat, a sidechain belongs to a subagent — and only this
    /// turn's non-sidechain assistant text survives.
    #[test]
    fn extraction_finds_the_last_real_turn_and_only_its_text() {
        let path = write_transcript("turn", &[
            r#"{"type":"user","message":{"content":"first prompt"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"OLD TURN"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"second prompt"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"},{"type":"tool_use","name":"Bash"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"caveat"}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"SUBAGENT"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"part one"}]}}"#,
            r#"{"type":"ai-title","title":"noise"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"part two"}]}}"#,
        ]);
        assert_eq!(extract_turn_text(&path).as_deref(), Some("part one\n\npart two"));
        let _ = std::fs::remove_file(&path);
    }

    /// The flush race, reproduced: at Stop time the transcript holds the turn's
    /// boundary and tool traffic but not yet its final text — Claude Code
    /// appends that entry a beat later (measured: same second, after the hook).
    /// The settled reader must pick it up instead of skipping the turn.
    #[test]
    fn extraction_waits_out_the_transcript_flush_race() {
        let path = write_transcript("race", &[
            r#"{"type":"user","message":{"content":"question"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read"}]}}"#,
        ]);
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            let mut f = std::fs::OpenOptions::new().append(true).open(&writer_path).unwrap();
            f.write_all(
                b"\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"late answer\"}]}}",
            )
            .unwrap();
        });
        assert_eq!(extract_turn_text_settled(&path).as_deref(), Some("late answer"));
        writer.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// Measured 2026-08-30: a local command (`/clear`) lands as a plain string
    /// user entry — NOT meta — shaped `<command-name>…`. It must stay a valid
    /// boundary: a slash command that runs a real turn (`/sync-docs`) starts it,
    /// and text from before the command must never leak into that turn.
    #[test]
    fn a_local_command_entry_is_a_turn_boundary() {
        let path = write_transcript("cmd", &[
            r#"{"type":"user","message":{"content":"earlier prompt"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"EARLIER TURN"}]}}"#,
            r#"{"type":"user","message":{"content":"<command-name>/sync-docs</command-name>\n<command-message>sync-docs</command-message>"}}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"<local-command-caveat>Caveat: …</local-command-caveat>"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"the command turn"}]}}"#,
        ]);
        assert_eq!(extract_turn_text(&path).as_deref(), Some("the command turn"));
        let _ = std::fs::remove_file(&path);
    }

    /// Measured on a real session: an interrupt lands as a plain string user
    /// entry with nothing after it. Nothing to explain → no Sonnet call at all.
    #[test]
    fn a_turn_with_no_assistant_text_is_skipped() {
        let path = write_transcript("empty", &[
            r#"{"type":"user","message":{"content":"do things"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
            r#"{"type":"user","message":{"content":"[Request interrupted by user]"}}"#,
        ]);
        assert_eq!(extract_turn_text(&path), None);
        let _ = std::fs::remove_file(&path);
    }

    /// The cap keeps the tail — conclusions live at the end of a turn — and
    /// must cut on a char boundary (the text is Hebrew, every char multibyte).
    #[test]
    fn oversized_turns_keep_the_tail_marked_truncated() {
        let big = "א".repeat(MAX_TURN_BYTES); // 2 bytes per char → over the cap
        let line = format!(
            r#"{{"type":"user","message":{{"content":"p"}}}}
{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{big}END"}}]}}}}"#
        );
        let path = write_transcript("cap", &[&line]);
        let got = extract_turn_text(&path).unwrap();
        assert!(got.starts_with("[…truncated…]\n"));
        assert!(got.ends_with("END"));
        assert!(got.len() <= MAX_TURN_BYTES + "[…truncated…]\n".len() + 4);
        let _ = std::fs::remove_file(&path);
    }

    /// The store is bounded and forgettable: newest first, capped per instance,
    /// dropped per instance on reap and wholesale on project close. Exercises
    /// the real public surface against the module singleton — handles in the
    /// 990xx range so parallel tests can't collide (`init` is never called in
    /// tests, so no worker drains the queue behind our back).
    #[test]
    fn the_feed_is_newest_first_capped_and_forgettable() {
        const H: ProjectHandle = 99001;
        const H2: ProjectHandle = 99002;
        let entry = |id: usize, n: u64| ExplainEntry {
            id,
            ts: n,
            text: format!("e{n}"),
            ok: true,
            kind: ExplainKind::Turn,
        };
        for n in 0..(MAX_ENTRIES as u64 + 5) {
            push_entry(inner(), H, 1, entry(1, n));
        }
        push_entry(inner(), H, 2, entry(2, 0));
        push_entry(inner(), H2, 1, entry(1, 0));

        let mine: Vec<_> = feed(H);
        assert_eq!(mine.iter().filter(|e| e.id == 1).count(), MAX_ENTRIES, "capped");
        let newest = mine.iter().filter(|e| e.id == 1).next().unwrap();
        assert_eq!(newest.ts, MAX_ENTRIES as u64 + 4, "newest first");

        forget(H, 1);
        assert!(feed(H).iter().all(|e| e.id != 1));
        assert!(feed(H).iter().any(|e| e.id == 2), "other instance survives");
        forget_project(H);
        assert!(feed(H).is_empty());
        assert_eq!(feed(H2).len(), 1, "other project survives");
        forget_project(H2);
    }

    /// The busy dot's bookkeeping: a coalesced re-submit doesn't double-count,
    /// finish drops to idle exactly once, and forget clears without letting a
    /// late finish go negative or re-fire.
    #[test]
    fn pending_counts_track_queue_running_and_forget() {
        const H: ProjectHandle = 99004;
        let count = |id: usize| inner().pending.lock().unwrap().get(&(H, id)).copied();

        submit(H, 1, "/tmp/a.jsonl".into(), PathBuf::from("/tmp"));
        submit(H, 1, "/tmp/b.jsonl".into(), PathBuf::from("/tmp"));
        assert_eq!(count(1), Some(1), "a coalesced re-submit is one job, not two");
        submit(H, 2, "/tmp/c.jsonl".into(), PathBuf::from("/tmp"));
        assert_eq!(count(2), Some(1));

        finish_pending(inner(), H, 1);
        assert_eq!(count(1), None, "finished means idle, entry gone");
        finish_pending(inner(), H, 1);
        assert_eq!(count(1), None, "a late finish after forget/idle stays silent");

        forget(H, 2);
        assert_eq!(count(2), None, "forget clears the dot with the feed");
        forget_project(H); // drain this test's queue entries
    }

    /// Latest-wins coalescing: a queued-not-yet-running job for the same
    /// instance is replaced *within its kind* — a pending question is never
    /// coalesced away by the turn that follows it. A different instance queues
    /// alongside.
    #[test]
    fn queued_jobs_coalesce_per_instance_and_kind() {
        const H: ProjectHandle = 99003;
        submit(H, 1, "/tmp/a.jsonl".into(), PathBuf::from("/tmp"));
        submit(H, 2, "/tmp/b.jsonl".into(), PathBuf::from("/tmp"));
        submit(H, 1, "/tmp/c.jsonl".into(), PathBuf::from("/tmp"));
        submit_question(H, 1, r#"{"questions":[]}"#.into(), PathBuf::from("/tmp"));
        {
            let q = inner().queue.lock().unwrap();
            let mine: Vec<_> = q.iter().filter(|j| j.handle == H).collect();
            assert_eq!(mine.len(), 3, "turn replaced within kind; question queued alongside");
            let turn1 = mine
                .iter()
                .find(|j| j.id == 1 && matches!(j.input, Input::Turn { .. }))
                .unwrap();
            match &turn1.input {
                Input::Turn { transcript } => {
                    assert_eq!(transcript, &PathBuf::from("/tmp/c.jsonl"), "the newer request won")
                }
                Input::Question { .. } => unreachable!(),
            }
            assert!(
                mine.iter().any(|j| j.id == 1 && matches!(j.input, Input::Question { .. })),
                "the question job survived the turn submits"
            );
        }
        forget_project(H); // also drains this test's queue entries
        assert!(inner().queue.lock().unwrap().iter().all(|j| j.handle != H));
    }

    /// The measured `AskUserQuestion` tool_input shape flattens into text the
    /// summarizer can explain: questions numbered, options with descriptions,
    /// `(Recommended)` labels kept verbatim. Junk payloads yield None (skip).
    #[test]
    fn question_payloads_flatten_to_explainable_text() {
        let json = r#"{"questions":[
            {"question":"What next with todo.html?","header":"Next step","multiSelect":false,
             "options":[
                {"label":"Add localStorage persistence (Recommended)","description":"Tasks survive refresh."},
                {"label":"Nothing for now","description":"Just testing."}]},
            {"question":"Which parts?","header":"Parts","multiSelect":true,
             "options":[{"label":"UI","description":"the page"}]}
        ]}"#;
        let text = question_text(json).unwrap();
        assert!(text.contains("Question 1: What next with todo.html?"));
        assert!(text.contains("- Add localStorage persistence (Recommended): Tasks survive refresh."));
        assert!(text.contains("Question 2: Which parts?"));
        assert!(text.contains("(multiple answers allowed)"));
        assert_eq!(question_text(r#"{"questions":[]}"#), None, "no questions → skip");
        assert_eq!(question_text("not json"), None);
    }
}
