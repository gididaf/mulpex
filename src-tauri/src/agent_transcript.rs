//! Where an agent keeps the conversation of one instance — the file a save,
//! an explain, the phone's chat and the handoff all read.
//!
//! * **claude** — `<CLAUDE_CONFIG_DIR or ~/.claude>/projects/<slug>/<uuid>.jsonl`
//!   ([`claude_path`]; the slug is the canonical project dir with every
//!   non-alphanumeric byte turned into `-`).
//! * **codex** — a rollout file, `<CODEX_HOME or ~/.codex>/sessions/YYYY/MM/DD/
//!   rollout-<timestamp>-<session id>.jsonl`. The codex hooks write its absolute
//!   path to `transcript/<id>` in the scratch dir (payload `transcript_path`,
//!   docs/codex.md S5), which is exact; without it the sessions tree is searched
//!   by the id in the file name.

use std::io::{Read as _, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use mulpex_core::agent::Agent;

use crate::claude_bin;
use crate::remote::chat::Item;

/// The transcript of instance `id` running `agent` in `project_dir`, whose
/// conversation id is `session_id`. For claude this is where the file *would*
/// be (callers check `is_file`, as they always did); for codex it is a file
/// that exists, or `None`.
pub fn path(
    agent: Agent,
    project_dir: &Path,
    session_id: &str,
    state_dir: &Path,
    id: usize,
) -> Option<PathBuf> {
    match agent {
        Agent::Claude => Some(claude_path(project_dir, session_id)),
        Agent::Codex => codex_path(session_id, state_dir, id),
    }
}

/// Where Claude Code keeps conversation `uuid` of project `dir` (checked
/// against the real `~/.claude/projects/` names, e.g. `/private/tmp/...` →
/// `-private-tmp-...`).
pub fn claude_path(dir: &Path, uuid: &str) -> PathBuf {
    let slug: String = canon(dir)
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    claude_config_dir().join("projects").join(slug).join(format!("{uuid}.jsonl"))
}

/// The codex rollout: what the hook recorded, else a search by session id.
pub fn codex_path(session_id: &str, state_dir: &Path, id: usize) -> Option<PathBuf> {
    let recorded = std::fs::read_to_string(state_dir.join("transcript").join(id.to_string()))
        .ok()
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| p.is_absolute() && p.is_file());
    // A recorded path is only trusted if it is THIS conversation: after a
    // handoff or a resume the file can name an older thread.
    match recorded {
        Some(p) if session_id.is_empty() || rollout_matches(&p, session_id) => Some(p),
        _ => find_rollout(&codex_home().join("sessions"), session_id),
    }
}

/// The rollout of `session_id` under `sessions` (`YYYY/MM/DD/rollout-*-<id>.jsonl`),
/// newest directory first. An empty id finds nothing.
pub(crate) fn find_rollout(sessions: &Path, session_id: &str) -> Option<PathBuf> {
    let session_id = session_id.trim();
    if session_id.is_empty() || session_id.contains('/') {
        return None;
    }
    fn walk(dir: &Path, sid: &str, depth: usize) -> Option<PathBuf> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
        // Date dirs sort lexically; newest first finds a recent thread fast.
        entries.sort();
        entries.reverse();
        for p in entries {
            if p.is_dir() {
                if depth < 4 {
                    if let Some(hit) = walk(&p, sid, depth + 1) {
                        return Some(hit);
                    }
                }
            } else if rollout_matches(&p, sid) {
                return Some(p);
            }
        }
        None
    }
    walk(sessions, session_id, 0)
}

fn rollout_matches(p: &Path, session_id: &str) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(&format!("-{session_id}.jsonl")))
}

/// `$CODEX_HOME` from the login env (what the codex children see), else ours,
/// else `~/.codex`.
pub fn codex_home() -> PathBuf {
    env_dir("CODEX_HOME").unwrap_or_else(|| home().join(".codex"))
}

fn claude_config_dir() -> PathBuf {
    env_dir("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home().join(".claude"))
}

fn env_dir(key: &str) -> Option<PathBuf> {
    claude_bin::forwarded_env()
        .into_iter()
        .find(|(k, v)| k == key && !v.is_empty())
        .map(|(_, v)| PathBuf::from(v))
        .or_else(|| std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from))
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

// ---- reading a transcript as a conversation ----

/// What one transcript line of `agent` contributes, as the phone's chat sees
/// it: user and assistant text, tool calls with a one-line summary.
pub fn items(agent: Agent, line: &str) -> Vec<Item> {
    match agent {
        Agent::Claude => crate::remote::chat::parse_line(line),
        Agent::Codex => crate::remote::chat_codex::parse_line(line),
    }
}

/// How much of a transcript's end is read before rendering. Rendering drops
/// tool output and machinery, so this is far more than what is kept.
const READ_BYTES: u64 = 8 * 1024 * 1024;

/// The end of a transcript (whole lines only).
fn tail_lines(path: &Path, bytes: u64) -> Option<Vec<String>> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(bytes);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // a partial line
    }
    Some(lines)
}

/// The conversation in `path` as plain text for another agent to read: every
/// user and assistant message in full, each tool call as one line, tool
/// output left out — the last `max_bytes` of it, cut at a line.
pub fn render(agent: Agent, path: &Path, max_bytes: usize) -> Option<String> {
    let mut out = String::new();
    for line in tail_lines(path, READ_BYTES)? {
        for item in items(agent, &line) {
            match item {
                Item::User { text } => out.push_str(&format!("\n[USER]\n{}\n", text.trim())),
                Item::Text { text } => out.push_str(&format!("\n[ASSISTANT]\n{}\n", text.trim())),
                Item::Tool { name, summary, .. } => out.push_str(&format!("[tool {name}] {summary}\n")),
                _ => {}
            }
        }
    }
    if out.trim().is_empty() {
        return None;
    }
    Some(keep_tail(&out, max_bytes))
}

/// The last `max` bytes of `s`, starting at a line, marked when cut.
fn keep_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = s.len() - max;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    let rest = &s[cut..];
    let rest = rest.split_once('\n').map_or(rest, |(_, r)| r);
    format!("[… the earlier part of the conversation is cut …]\n{rest}")
}

/// The last assistant text in a transcript, read from its end.
pub fn last_assistant_text(agent: Agent, path: &Path, bytes: u64) -> Option<String> {
    let mut last = None;
    for line in tail_lines(path, bytes)? {
        for item in items(agent, &line) {
            if let Item::Text { text } = item {
                last = Some(text);
            }
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mulpex-agent-transcript-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Lines in the shape of real 0.149.1 rollouts.
    const ROLLOUT: &str = r#"{"type":"session_meta","payload":{"id":"t"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x</environment_context>"}]}}
{"type":"event_msg","payload":{"type":"user_message","message":"Fix the login bug"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Fix the login bug"}]}}
{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"rg login src\",\"workdir\":\"/x\"}","call_id":"c1"}}
{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"LOTS OF OUTPUT"}}
{"type":"response_item","payload":{"type":"custom_tool_call","status":"completed","call_id":"c2","name":"apply_patch","input":"*** Begin Patch\n*** Update File: /x/src/login.ts\n@@\n-a\n+b\n*** End Patch"}}
{"type":"event_msg","payload":{"type":"agent_message","message":"Fixed it in login.ts."}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Fixed it in login.ts."}]}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"mulpex: 1 new hub message(s)"}]}}
"#;

    #[test]
    fn a_codex_rollout_renders_as_a_conversation() {
        let dir = tmp("render");
        let f = dir.join("rollout.jsonl");
        std::fs::write(&f, ROLLOUT).unwrap();
        let r = render(Agent::Codex, &f, 100_000).unwrap();
        assert_eq!(
            r,
            "\n[USER]\nFix the login bug\n[tool Bash] rg login src\n[tool apply_patch] login.ts\n\n[ASSISTANT]\nFixed it in login.ts.\n"
        );
        assert!(!r.contains("LOTS OF OUTPUT") && !r.contains("environment_context") && !r.contains("hub message"));
        assert_eq!(last_assistant_text(Agent::Codex, &f, 1 << 20).as_deref(), Some("Fixed it in login.ts."));
        let cut = render(Agent::Codex, &f, 40).unwrap();
        assert!(cut.starts_with("[… the earlier part") && cut.ends_with("Fixed it in login.ts.\n"), "{cut}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keep_tail_respects_char_boundaries() {
        let s = "שלום\nעולם\n";
        let t = keep_tail(s, 7);
        assert!(t.ends_with("עולם\n") || t.ends_with("\n"), "{t}");
        assert_eq!(keep_tail("abc", 10), "abc");
    }

    #[test]
    fn claude_path_matches_claude_codes_layout() {
        let p = claude_path(Path::new("/nonexistent/a b/c.d"), "u");
        assert!(p.ends_with("projects/-nonexistent-a-b-c-d/u.jsonl"), "{}", p.display());
        assert_eq!(path(Agent::Claude, Path::new("/nonexistent/x"), "u", Path::new("/nope"), 1), Some(claude_path(Path::new("/nonexistent/x"), "u")));
    }

    #[test]
    fn finds_a_rollout_by_session_id_in_the_date_tree() {
        let root = tmp("find");
        let sid = "01a03ced-8693-70c0-805a-120e41c31ab9";
        let day = root.join("2026/10/21");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::create_dir_all(root.join("2026/10/20")).unwrap();
        let file = day.join(format!("rollout-2026-10-21T10-16-34-{sid}.jsonl"));
        std::fs::write(&file, "{}\n").unwrap();
        std::fs::write(root.join("2026/10/20/rollout-2026-10-20T10-00-00-other.jsonl"), "").unwrap();
        assert_eq!(find_rollout(&root, sid), Some(file));
        assert_eq!(find_rollout(&root, "missing"), None);
        assert_eq!(find_rollout(&root, ""), None, "an empty id must not match every rollout");
        assert_eq!(find_rollout(&root, "../x"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_hooks_record_wins_when_it_is_this_thread() {
        let state = tmp("state");
        let rollout = state.join("rollout-2026-10-21T10-16-34-abc.jsonl");
        std::fs::write(&rollout, "").unwrap();
        std::fs::create_dir_all(state.join("transcript")).unwrap();
        std::fs::write(state.join("transcript/4"), format!("{}\n", rollout.display())).unwrap();
        assert_eq!(codex_path("abc", &state, 4), Some(rollout.clone()));
        assert_eq!(codex_path("", &state, 4), Some(rollout), "no id yet: the record is all there is");
        assert_eq!(codex_path("", &state, 5), None, "no record and no id: nothing");
        let _ = std::fs::remove_dir_all(&state);
    }
}
