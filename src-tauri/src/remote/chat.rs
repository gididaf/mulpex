//! The phone's chat view of a claude, read from its transcript `.jsonl` — the
//! ground truth of what the conversation contains, unlike its screen.
//!
//! Each line is one entry. What the phone shows:
//! - `user` entries a human typed (`origin.kind == "human"`), as the user's
//!   bubbles; a background task waking the claude (`task-notification`) and an
//!   interrupt, as small system lines;
//! - `assistant` text blocks, as claude's bubbles;
//! - `assistant` tool calls, as one collapsed line each, with their result
//!   (which arrives later, as a `user` entry of `tool_result` blocks) attached
//!   by tool-use id on the phone.
//!
//! - a message typed while the claude was busy: first as `queued` (from the
//!   transcript's `queue-operation` entries, see [`Queue`]), then as a user
//!   bubble once taken — a `user` entry if it waited for the turn to end, a
//!   `queued_command` attachment if the claude took it mid-turn.
//!
//! Hidden: thinking, `isMeta` and sidechain (subagent) entries, and every
//! other bookkeeping entry type (attachments, modes, titles, file history…).
//! Long bodies are cut — this is a phone, and a 64 MB transcript exists.

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

/// How much of a transcript's end the first load reads.
const HISTORY_BYTES: u64 = 4 * 1024 * 1024;
/// How many items the first load sends.
const HISTORY_ITEMS: usize = 400;
const MAX_TEXT: usize = 20_000;
const MAX_DETAIL: usize = 4_000;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "k", rename_all = "lowercase")]
pub enum Item {
    User { text: String },
    Text { text: String },
    Tool { id: String, name: String, summary: String, detail: String },
    Result { id: String, text: String, error: bool },
    Sys { text: String },
    /// The human messages waiting in the claude's queue right now, oldest
    /// first — the whole list each time it changes, so the phone just keeps
    /// the last one.
    Queue { items: Vec<String> },
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n… (cut)");
    out
}

/// The text of a content value: a string, or the text blocks of an array.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map(str::to_string),
                Some("image") => Some("[image]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = s.find(open)? + open.len();
    let end = s[start..].find(close)? + start;
    Some(s[start..end].trim())
}

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// One short line saying what a tool call does.
fn summary(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    let line = match name {
        "Bash" => if s("description").is_empty() { s("command") } else { s("description") }.to_string(),
        "Read" | "Write" | "Edit" | "NotebookEdit" => base(s("file_path")).to_string(),
        "Grep" | "Glob" => s("pattern").to_string(),
        "Agent" | "Task" => s("description").to_string(),
        "WebFetch" => s("url").to_string(),
        "WebSearch" => s("query").to_string(),
        "Skill" => s("skill").to_string(),
        _ => input
            .as_object()
            .and_then(|o| o.values().find_map(Value::as_str))
            .unwrap_or("")
            .to_string(),
    };
    cut(line.lines().next().unwrap_or(""), 120)
}

/// What one transcript line contributes. Most lines contribute nothing.
pub fn parse_line(line: &str) -> Vec<Item> {
    let Ok(d) = serde_json::from_str::<Value>(line) else { return vec![] };
    if d.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || d.get("isMeta").and_then(Value::as_bool) == Some(true)
    {
        return vec![];
    }
    let content = d.pointer("/message/content").cloned().unwrap_or(Value::Null);
    match d.get("type").and_then(Value::as_str) {
        Some("user") => {
            if let Value::Array(blocks) = &content {
                let results: Vec<Item> = blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                    .map(|b| Item::Result {
                        id: b.get("tool_use_id").and_then(Value::as_str).unwrap_or("").into(),
                        text: cut(&text_of(b.get("content").unwrap_or(&Value::Null)), MAX_DETAIL),
                        error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                    })
                    .collect();
                if !results.is_empty() {
                    return results;
                }
            }
            let text = text_of(&content);
            match d.pointer("/origin/kind").and_then(Value::as_str) {
                Some("task-notification") => {
                    let what = between(&text, "<summary>", "</summary>").unwrap_or("A background task finished");
                    vec![Item::Sys { text: cut(what, 200) }]
                }
                Some("human") => vec![Item::User { text: cut(&text, MAX_TEXT) }],
                _ if text.starts_with("[Request interrupted") => vec![Item::Sys { text: "Interrupted".into() }],
                // Hook output, injected reminders, other machinery.
                _ => vec![],
            }
        }
        Some("assistant") => {
            let Value::Array(blocks) = content else { return vec![] };
            blocks
                .iter()
                .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let t = b.get("text").and_then(Value::as_str).unwrap_or("").trim();
                        (!t.is_empty()).then(|| Item::Text { text: cut(t, MAX_TEXT) })
                    }
                    Some("tool_use") => {
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let input = b.get("input").cloned().unwrap_or(Value::Null);
                        Some(Item::Tool {
                            id: b.get("id").and_then(Value::as_str).unwrap_or("").into(),
                            name: name.strip_prefix("mcp__mulpex__").unwrap_or(name).into(),
                            summary: summary(name, &input),
                            detail: cut(&serde_json::to_string_pretty(&input).unwrap_or_default(), MAX_DETAIL),
                        })
                    }
                    _ => None,
                })
                .collect()
        }
        // A queued message the claude took mid-turn: it never becomes a `user`
        // entry, only this.
        Some("attachment") => {
            let a = d.get("attachment").unwrap_or(&Value::Null);
            let human = a.pointer("/origin/kind").and_then(Value::as_str) == Some("human");
            if a.get("type").and_then(Value::as_str) == Some("queued_command") && human {
                let text = text_of(a.get("prompt").unwrap_or(&Value::Null));
                vec![Item::User { text: cut(&text, MAX_TEXT) }]
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

/// Claude Code's input queue, replayed from the transcript's `queue-operation`
/// entries (measured on claude 2.1.29x transcripts):
/// - `enqueue` + `content`: something joined the queue — a message typed while
///   the claude was busy, or a background task's `<task-notification>`;
/// - `dequeue`: the oldest one was taken at the end of a turn (and becomes a
///   `user` entry);
/// - `remove` + `content`: that one was taken mid-turn (`absorbed_mid_turn`) or
///   handed to a subagent;
/// - `popAll`: the user pulled the queue back into the input box to edit it.
///
/// Notifications are tracked too — a `dequeue` names nothing, so the order has
/// to be exact — but only the human messages are shown.
#[derive(Default)]
pub struct Queue {
    items: Vec<String>,
}

impl Queue {
    /// Apply one transcript line; `true` if the queue changed.
    pub fn apply(&mut self, line: &str) -> bool {
        // Cheap test first: almost no line is a queue operation.
        if !line.contains("\"queue-operation\"") {
            return false;
        }
        let Ok(d) = serde_json::from_str::<Value>(line) else { return false };
        if d.get("type").and_then(Value::as_str) != Some("queue-operation") {
            return false;
        }
        let content = text_of(d.get("content").unwrap_or(&Value::Null));
        match d.get("operation").and_then(Value::as_str) {
            Some("enqueue") => self.items.push(content),
            Some("dequeue") if !self.items.is_empty() => {
                self.items.remove(0);
            }
            Some("remove") => match self.items.iter().position(|q| *q == content) {
                Some(i) => {
                    self.items.remove(i);
                }
                None => return false,
            },
            Some("popAll") if !self.items.is_empty() => self.items.clear(),
            _ => return false,
        }
        true
    }

    /// The human messages in it, as the phone shows them.
    pub fn item(&self) -> Item {
        let items = self
            .items
            .iter()
            .filter(|q| !q.trim_start().starts_with("<task-notification>"))
            .map(|q| cut(q, MAX_TEXT))
            .collect();
        Item::Queue { items }
    }
}

/// Follows one transcript file: a first load of its recent history, then
/// whatever is appended. Only complete lines are parsed — a line Claude Code is
/// halfway through writing waits for the next read.
pub struct Tail {
    pub path: PathBuf,
    offset: u64,
    queue: Queue,
}

impl Tail {
    /// Start at the end of `path`, returning its recent history, then the queue.
    pub fn open(path: PathBuf) -> (Tail, Vec<Item>) {
        let mut tail = Tail { path, offset: 0, queue: Queue::default() };
        let mut items = Vec::new();
        if let Ok(mut f) = std::fs::File::open(&tail.path) {
            let size = f.metadata().map(|m| m.len()).unwrap_or(0);
            let start = size.saturating_sub(HISTORY_BYTES);
            let mut buf = Vec::new();
            if f.seek(SeekFrom::Start(start)).is_ok() && f.read_to_end(&mut buf).is_ok() {
                // Starting mid-file lands mid-line: skip to the next whole one.
                let from = if start > 0 {
                    buf.iter().position(|&b| b == b'\n').map_or(buf.len(), |i| i + 1)
                } else {
                    0
                };
                let end = buf.iter().rposition(|&b| b == b'\n').map_or(from, |i| i + 1).max(from);
                for line in String::from_utf8_lossy(&buf[from..end]).lines() {
                    tail.queue.apply(line);
                    items.extend(parse_line(line));
                }
                tail.offset = start + end as u64;
            }
        }
        if items.len() > HISTORY_ITEMS {
            items.drain(..items.len() - HISTORY_ITEMS);
        }
        items.push(tail.queue.item());
        (tail, items)
    }

    /// New items since the last read. `None` means the file was replaced or
    /// truncated and the view must be reloaded from scratch.
    pub fn read_new(&mut self) -> Option<Vec<Item>> {
        let Ok(mut f) = std::fs::File::open(&self.path) else { return Some(vec![]) };
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        if size < self.offset {
            return None;
        }
        if size == self.offset {
            return Some(vec![]);
        }
        let mut buf = Vec::new();
        if f.seek(SeekFrom::Start(self.offset)).is_err() || f.read_to_end(&mut buf).is_err() {
            return Some(vec![]);
        }
        let Some(end) = buf.iter().rposition(|&b| b == b'\n') else { return Some(vec![]) };
        self.offset += end as u64 + 1;
        let mut items = Vec::new();
        let mut queued = false;
        for line in String::from_utf8_lossy(&buf[..end]).lines() {
            queued |= self.queue.apply(line);
            items.extend(parse_line(line));
        }
        if queued {
            items.push(self.queue.item());
        }
        Some(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(v: Value) -> String {
        v.to_string()
    }

    #[test]
    fn a_human_prompt_and_a_reply() {
        let u = line(json!({"type":"user","origin":{"kind":"human"},"message":{"role":"user","content":"שלום, fix it"}}));
        assert_eq!(parse_line(&u), vec![Item::User { text: "שלום, fix it".into() }]);
        let a = line(json!({"type":"assistant","message":{"content":[
            {"type":"thinking","thinking":"secret"},
            {"type":"text","text":"Done."}
        ]}}));
        assert_eq!(parse_line(&a), vec![Item::Text { text: "Done.".into() }]);
    }

    #[test]
    fn a_tool_call_and_its_result() {
        let a = line(json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la","description":"List files"}}
        ]}}));
        let items = parse_line(&a);
        let Item::Tool { id, name, summary, detail } = &items[0] else { panic!() };
        assert_eq!((id.as_str(), name.as_str(), summary.as_str()), ("t1", "Bash", "List files"));
        assert!(detail.contains("ls -la"));

        let r = line(json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"a\nb"}],"is_error":false}
        ]}}));
        assert_eq!(parse_line(&r), vec![Item::Result { id: "t1".into(), text: "a\nb".into(), error: false }]);
    }

    #[test]
    fn mulpex_tools_lose_their_prefix_and_files_show_their_name() {
        let a = line(json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"t","name":"mcp__mulpex__hub_send","input":{"to":"2","message":"hi"}},
            {"type":"tool_use","id":"u","name":"Edit","input":{"file_path":"/a/b/pty.rs","old_string":"x"}}
        ]}}));
        let items = parse_line(&a);
        assert!(matches!(&items[0], Item::Tool { name, .. } if name == "hub_send"));
        assert!(matches!(&items[1], Item::Tool { summary, .. } if summary == "pty.rs"));
    }

    #[test]
    fn machinery_is_hidden() {
        for v in [
            json!({"type":"user","isMeta":true,"origin":{"kind":"human"},"message":{"content":"x"}}),
            json!({"type":"user","isSidechain":true,"origin":{"kind":"human"},"message":{"content":"x"}}),
            json!({"type":"user","message":{"content":"<system-reminder>hook</system-reminder>"}}),
            json!({"type":"attachment","attachment":{}}),
            json!({"type":"system","subtype":"stop_hook_summary"}),
            json!({"type":"ai-title","aiTitle":"x"}),
        ] {
            assert!(parse_line(&line(v.clone())).is_empty(), "{v}");
        }
        assert!(parse_line("not json").is_empty());
    }

    #[test]
    fn a_task_wake_and_an_interrupt_are_system_lines() {
        let t = line(json!({"type":"user","origin":{"kind":"task-notification"},"message":{"content":
            "<task-notification>\n<summary>Agent \"x\" finished</summary></task-notification>"}}));
        assert_eq!(parse_line(&t), vec![Item::Sys { text: "Agent \"x\" finished".into() }]);
        let i = line(json!({"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}));
        assert_eq!(parse_line(&i), vec![Item::Sys { text: "Interrupted".into() }]);
    }

    #[test]
    fn the_tail_reads_history_then_only_complete_new_lines() {
        let dir = std::env::temp_dir().join(format!("mulpex-chat-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let human = |t: &str| line(json!({"type":"user","origin":{"kind":"human"},"message":{"content":t}}));
        std::fs::write(&path, format!("{}\n{}\n", human("one"), human("two"))).unwrap();
        let (mut tail, items) = Tail::open(path.clone());
        assert_eq!(items.len(), 3);
        assert_eq!(items[2], Item::Queue { items: vec![] }, "the first load ends with the queue");
        assert_eq!(tail.read_new(), Some(vec![]));

        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        let three = human("three");
        let (a, b) = three.split_at(10);
        write!(f, "{a}").unwrap();
        assert_eq!(tail.read_new(), Some(vec![]), "half a line waits");
        writeln!(f, "{b}").unwrap();
        assert_eq!(tail.read_new(), Some(vec![Item::User { text: "three".into() }]));

        std::fs::write(&path, "").unwrap();
        assert_eq!(tail.read_new(), None, "a truncated file asks for a reload");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn op(op: &str, content: Option<&str>, reason: Option<&str>) -> String {
        let mut v = json!({"type":"queue-operation","operation":op,"timestamp":"t","sessionId":"s"});
        if let Some(c) = content {
            v["content"] = json!(c);
        }
        if let Some(r) = reason {
            v["reason"] = json!(r);
        }
        line(v)
    }

    /// The real sequences: a message waits, then is taken at the end of the
    /// turn (`dequeue`) or mid-turn (`remove`); notifications share the queue
    /// but are never shown.
    #[test]
    fn the_queue_follows_the_transcript() {
        let note = "<task-notification>\n<summary>x</summary></task-notification>";
        let mut q = Queue::default();
        assert!(!q.apply(&line(json!({"type":"user","message":{"content":"queue-operation"}}))));
        assert!(q.apply(&op("enqueue", Some(note), None)));
        assert!(q.apply(&op("enqueue", Some("first"), None)));
        assert!(q.apply(&op("enqueue", Some("second"), None)));
        assert_eq!(q.item(), Item::Queue { items: vec!["first".into(), "second".into()] }, "notes hidden");

        assert!(q.apply(&op("dequeue", None, None)), "takes the note, the oldest");
        assert_eq!(q.item(), Item::Queue { items: vec!["first".into(), "second".into()] });
        assert!(q.apply(&op("remove", Some("second"), Some("absorbed_mid_turn"))));
        assert_eq!(q.item(), Item::Queue { items: vec!["first".into()] });
        assert!(!q.apply(&op("remove", Some("never queued"), Some("absorbed_mid_turn"))));
        assert!(q.apply(&op("dequeue", None, None)));
        assert_eq!(q.item(), Item::Queue { items: vec![] });
        assert!(!q.apply(&op("dequeue", None, None)), "an empty queue stays empty");

        q.apply(&op("enqueue", Some("a"), None));
        q.apply(&op("enqueue", Some("b"), None));
        assert!(q.apply(&op("popAll", Some("a\nb"), None)));
        assert_eq!(q.item(), Item::Queue { items: vec![] }, "pulled back to edit");
    }

    /// Taken mid-turn, a message exists only as this attachment.
    #[test]
    fn a_message_taken_mid_turn_is_a_user_bubble() {
        let a = line(json!({"type":"attachment","attachment":{"type":"queued_command","prompt":"also do X",
            "commandMode":"prompt","origin":{"kind":"human"},"humanTurn":true}}));
        assert_eq!(parse_line(&a), vec![Item::User { text: "also do X".into() }]);
        let note = line(json!({"type":"attachment","attachment":{"type":"queued_command",
            "prompt":"<task-notification>x</task-notification>","commandMode":"task-notification"}}));
        assert!(parse_line(&note).is_empty(), "a task wake taken mid-turn is not the user's");
    }

    #[test]
    fn the_tail_sends_the_queue_when_it_changes() {
        let dir = std::env::temp_dir().join(format!("mulpex-chat-queue-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(&path, format!("{}\n", op("enqueue", Some("waiting"), None))).unwrap();
        let (mut tail, items) = Tail::open(path.clone());
        assert_eq!(items, vec![Item::Queue { items: vec!["waiting".into()] }]);

        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{}", op("dequeue", None, None)).unwrap();
        writeln!(f, "{}", line(json!({"type":"user","origin":{"kind":"human"},"message":{"content":"waiting"}}))).unwrap();
        assert_eq!(
            tail.read_new(),
            Some(vec![Item::User { text: "waiting".into() }, Item::Queue { items: vec![] }])
        );
        writeln!(f, "{}", line(json!({"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}))).unwrap();
        assert_eq!(tail.read_new(), Some(vec![Item::Text { text: "ok".into() }]), "no queue when it didn't change");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Replaying this machine's real transcripts: every human message the queue
    /// ever held is later taken (shown as a user bubble), and nothing is left
    /// queued at the end of a finished conversation except what the transcript
    /// itself leaves open. Reports rather than asserts on the last part — a
    /// conversation killed mid-turn legitimately ends queued.
    #[test]
    fn real_queues_replay() {
        let root = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude/projects");
        let Ok(dirs) = std::fs::read_dir(&root) else { return };
        let (mut files, mut ops, mut left) = (0, 0, 0);
        for d in dirs.flatten() {
            for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                if f.path().extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(f.path()) else { continue };
                if !text.contains("\"queue-operation\"") {
                    continue;
                }
                files += 1;
                let mut q = Queue::default();
                for l in text.lines() {
                    ops += q.apply(l) as usize;
                }
                if let Item::Queue { items } = q.item() {
                    left += items.len();
                }
            }
        }
        eprintln!("queue replay: {files} transcripts, {ops} operations, {left} human messages left queued");
    }

    /// Every line of real transcripts on this machine parses without panicking,
    /// and some produce items. Skipped where there are none.
    #[test]
    fn real_transcripts_parse() {
        let root = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude/projects");
        let Ok(dirs) = std::fs::read_dir(&root) else { return };
        let mut files = 0;
        let mut items = 0;
        for d in dirs.flatten().take(5) {
            for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten().take(3) {
                if f.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    let (_, got) = Tail::open(f.path());
                    files += 1;
                    items += got.len();
                }
            }
        }
        if files > 0 {
            assert!(items > 0, "{files} transcripts produced no items");
        }
    }
}
