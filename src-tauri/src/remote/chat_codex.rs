//! The phone's chat view of a codex, read from its rollout `.jsonl`
//! (`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<thread id>.jsonl`) — the codex
//! counterpart of [`super::chat::parse_line`], producing the same [`Item`]s.
//!
//! Measured on codex-cli 0.149.1 rollouts (2026-10-21). Each line is
//! `{"timestamp", "type", "payload"}`; what the phone shows comes only from
//! `type: "response_item"`:
//! - `message` / `role: "user"`: the user's bubble — its `input_text` parts,
//!   minus the ones codex itself injects (`<environment_context>`,
//!   `<user_instructions>`, `<recommended_plugins>`, `# AGENTS.md …`, the
//!   `<image …>` wrappers around an `input_image`). A Mulpex wake line
//!   (`mulpex: N new hub message(s)`) is a system line, not the user's;
//! - `message` / `role: "assistant"`: `output_text` parts, codex's bubbles;
//! - `function_call` (`name`, `arguments` a JSON string) and `custom_tool_call`
//!   (`name`, `input` a raw string: the patch for `apply_patch`, a small JS
//!   program calling `tools.exec_command({...})` for `exec`): one collapsed
//!   tool line, keyed by `call_id`;
//! - `function_call_output` / `custom_tool_call_output` (`output`, a string
//!   that is itself often a JSON array of `input_text` parts): its result.
//!
//! A `type: "compacted"` line (a `/compact` ended) is a system line.
//!
//! Hidden: `developer`/`system` messages (rules, skills, permissions),
//! `reasoning`, `session_meta`, `turn_context`, `world_state`, and every
//! `event_msg` — `user_message` / `agent_message` events repeat what the
//! response items already said, so showing both would double every bubble.

use serde_json::Value;

use super::chat::Item;

const MAX_TEXT: usize = 20_000;
const MAX_DETAIL: usize = 4_000;

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n… (cut)");
    out
}

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A part of a user message that codex wrote, not the person.
fn injected(text: &str) -> bool {
    let t = text.trim_start();
    if t.starts_with("# AGENTS.md") {
        return true;
    }
    // `<environment_context>`, `<user_instructions>`, `<recommended_plugins>`,
    // `<image name=…>`, `</image>`: an opening (or closing) lowercase tag.
    let tag = t.strip_prefix("</").or_else(|| t.strip_prefix('<'));
    tag.is_some_and(|rest| {
        let name: String = rest.chars().take_while(|c| c.is_ascii_lowercase() || *c == '_').collect();
        !name.is_empty() && rest[name.len()..].starts_with(['>', ' '])
    })
}

/// The text of a message's content parts of type `kind`, images as `[image]`.
fn parts(content: &Value, kind: &str, skip_injected: bool) -> String {
    let Value::Array(blocks) = content else { return String::new() };
    blocks
        .iter()
        .filter_map(|b| match b.get("type").and_then(Value::as_str) {
            Some(t) if t == kind => {
                let s = b.get("text").and_then(Value::as_str).unwrap_or("");
                (!(skip_injected && injected(s)) && !s.trim().is_empty()).then(|| s.trim().to_string())
            }
            Some("input_image") => Some("[image]".to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A tool's output: a plain string, or a JSON array of `input_text` parts
/// encoded in that string.
fn output_text(output: &Value) -> String {
    let s = match output {
        Value::String(s) => s.clone(),
        Value::Object(o) => o.get("output").and_then(Value::as_str).unwrap_or("").to_string(),
        _ => return String::new(),
    };
    match serde_json::from_str::<Value>(&s) {
        Ok(v @ Value::Array(_)) => parts(&v, "input_text", false),
        Ok(Value::Object(o)) => o.get("output").and_then(Value::as_str).unwrap_or(&s).to_string(),
        _ => s,
    }
}

/// The string value of `"key":"…"` inside raw text (the JS `exec` input is not
/// JSON as a whole, but its argument object is).
fn json_string_after(text: &str, key: &str) -> Option<String> {
    let at = text.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = text[at..].trim_start().strip_prefix(':')?.trim_start();
    let mut de = serde_json::Deserializer::from_str(rest).into_iter::<String>();
    de.next()?.ok()
}

/// The files an `apply_patch` touches, by base name.
fn patch_files(patch: &str) -> String {
    let files: Vec<&str> = patch
        .lines()
        .filter_map(|l| {
            ["*** Update File:", "*** Add File:", "*** Delete File:"]
                .iter()
                .find_map(|h| l.strip_prefix(h))
                .map(|p| base(p.trim()))
        })
        .collect();
    files.join(", ")
}

/// A tool's name and one short line saying what it does.
fn tool(name: &str, args: &str) -> (String, String) {
    let parsed: Value = serde_json::from_str(args).unwrap_or(Value::Null);
    let s = |k: &str| parsed.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let (name, line) = match name {
        "apply_patch" => ("apply_patch".to_string(), patch_files(args)),
        "exec_command" | "shell" => ("Bash".to_string(), s("cmd")),
        // `exec` runs a small program; name it after the first tool it calls.
        "exec" => {
            let called = args
                .find("tools.")
                .map(|i| args[i + 6..].chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect::<String>())
                .unwrap_or_default();
            let line = json_string_after(args, "cmd")
                .or_else(|| json_string_after(args, "path"))
                .or_else(|| json_string_after(args, "message"))
                .unwrap_or_default();
            match called.as_str() {
                "" => ("exec".to_string(), line),
                "exec_command" => ("Bash".to_string(), line),
                c => (c.strip_prefix("mcp__mulpex__").unwrap_or(c).to_string(), line),
            }
        }
        n => (
            n.strip_prefix("mcp__mulpex__").unwrap_or(n).to_string(),
            parsed.as_object().and_then(|o| o.values().find_map(Value::as_str)).unwrap_or("").to_string(),
        ),
    };
    (name, cut(line.lines().next().unwrap_or(""), 120))
}

/// What one rollout line contributes. Most lines contribute nothing.
pub fn parse_line(line: &str) -> Vec<Item> {
    let Ok(d) = serde_json::from_str::<Value>(line) else { return vec![] };
    match d.get("type").and_then(Value::as_str) {
        Some("response_item") => {}
        // `/compact` (or an auto-compaction) ended: the history before it was
        // replaced by `payload.replacement_history`.
        Some("compacted") => return vec![Item::Sys { text: "Context compacted".into() }],
        _ => return vec![],
    }
    let p = d.get("payload").unwrap_or(&Value::Null);
    let str_of = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("");
    let content = p.get("content").unwrap_or(&Value::Null);
    match str_of("type") {
        "message" => match str_of("role") {
            "user" => {
                let text = parts(content, "input_text", true);
                if text.is_empty() {
                    vec![]
                } else if mulpex_core::codex::is_wake_line(&text) {
                    vec![Item::Sys { text: text.trim().to_string() }]
                } else if text.starts_with("<turn_aborted>") {
                    vec![Item::Sys { text: "Interrupted".into() }]
                } else {
                    vec![Item::User { text: cut(&text, MAX_TEXT) }]
                }
            }
            "assistant" => {
                let text = parts(content, "output_text", false);
                if text.is_empty() { vec![] } else { vec![Item::Text { text: cut(&text, MAX_TEXT) }] }
            }
            _ => vec![],
        },
        t @ ("function_call" | "custom_tool_call") => {
            let args = if t == "function_call" { str_of("arguments") } else { str_of("input") };
            let (name, summary) = tool(str_of("name"), args);
            let detail = match serde_json::from_str::<Value>(args) {
                Ok(v @ Value::Object(_)) => serde_json::to_string_pretty(&v).unwrap_or_default(),
                _ => args.to_string(),
            };
            vec![Item::Tool { id: str_of("call_id").into(), name, summary, detail: cut(&detail, MAX_DETAIL) }]
        }
        "function_call_output" | "custom_tool_call_output" => {
            let text = output_text(p.get("output").unwrap_or(&Value::Null));
            let error = p.pointer("/output/success").and_then(Value::as_bool) == Some(false);
            vec![Item::Result { id: str_of("call_id").into(), text: cut(&text, MAX_DETAIL), error }]
        }
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(payload: Value) -> String {
        json!({"timestamp": "2026-10-21T06:13:14.000Z", "type": "response_item", "payload": payload}).to_string()
    }

    /// Lines shaped exactly as in a real 0.149.1 rollout (this machine,
    /// 2026-10-21, paths and ids redacted).
    #[test]
    fn a_prompt_and_a_reply() {
        let u = item(json!({"type":"message","role":"user","content":[
            {"type":"input_text","text":"<environment_context>\n  <cwd>/private/tmp</cwd>\n  <shell>zsh</shell>\n</environment_context>"}
        ]}));
        assert!(parse_line(&u).is_empty(), "codex's own context is not the user's");
        let u = item(json!({"type":"message","role":"user","content":[
            {"type":"input_text","text":"Reply with just the word hi."}
        ]}));
        assert_eq!(parse_line(&u), vec![Item::User { text: "Reply with just the word hi.".into() }]);
        let a = item(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}],
            "phase":"final_answer"}));
        assert_eq!(parse_line(&a), vec![Item::Text { text: "hi".into() }]);
    }

    #[test]
    fn injected_parts_and_images() {
        let u = item(json!({"type":"message","role":"user","content":[
            {"type":"input_text","text":"<recommended_plugins>\nHere is a list…</recommended_plugins>"},
            {"type":"input_text","text":"# AGENTS.md instructions for /x\n\n<INSTRUCTIONS>…"},
            {"type":"input_text","text":"<image name=[Image #1] path=\"art/raw/bg.png\">"},
            {"type":"input_image","image_url":"data:image/png;base64,AAAA"},
            {"type":"input_text","text":"</image>"},
            {"type":"input_text","text":"שלום, draw the bunny"}
        ]}));
        assert_eq!(parse_line(&u), vec![Item::User { text: "[image]\nשלום, draw the bunny".into() }]);
        assert!(!injected("<3 thanks"));
        assert!(!injected("see <environment_context>"), "only a part that starts with the tag");
    }

    #[test]
    fn machinery_is_hidden() {
        for v in [
            item(json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions instructions>…"}]})),
            item(json!({"type":"message","role":"system","content":[{"type":"input_text","text":"x"}]})),
            item(json!({"type":"reasoning","summary":[],"encrypted_content":"gAAA"})),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"Reply with just the word hi."}}).to_string(),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"hi"}}).to_string(),
            json!({"type":"event_msg","payload":{"type":"token_count","info":null}}).to_string(),
            json!({"type":"session_meta","payload":{"id":"x"}}).to_string(),
            json!({"type":"turn_context","payload":{}}).to_string(),
        ] {
            assert!(parse_line(&v).is_empty(), "{v}");
        }
        assert!(parse_line("not json").is_empty());
    }

    #[test]
    fn a_compaction_is_a_system_line() {
        let c = json!({"type":"compacted","payload":{"message":"","replacement_history":[]}}).to_string();
        assert_eq!(parse_line(&c), vec![Item::Sys { text: "Context compacted".into() }]);
    }

    #[test]
    fn a_wake_line_is_a_system_line() {
        let u = item(json!({"type":"message","role":"user","content":[
            {"type":"input_text","text": mulpex_core::codex::wake_line(2)}
        ]}));
        assert!(matches!(&parse_line(&u)[..], [Item::Sys { .. }]));
    }

    #[test]
    fn tool_calls_and_results() {
        let f = item(json!({"type":"function_call","name":"exec_command","call_id":"call_1",
            "arguments":"{\"cmd\":\"git status --short\",\"workdir\":\"/x\",\"max_output_tokens\":20000}"}));
        let got = parse_line(&f);
        let Item::Tool { id, name, summary, detail } = &got[0] else { panic!("{got:?}") };
        assert_eq!((id.as_str(), name.as_str(), summary.as_str()), ("call_1", "Bash", "git status --short"));
        assert!(detail.contains("workdir"));

        let e = item(json!({"type":"custom_tool_call","name":"exec","call_id":"call_2","status":"completed",
            "input":"const r = await tools.exec_command({\"cmd\":\"sed -n '1,240p' /x/SKILL.md\",\"workdir\":\"/x\",\"yield_time_ms\":10000});\ntext(r);"}));
        assert!(matches!(&parse_line(&e)[0], Item::Tool { name, summary, .. } if name == "Bash" && summary == "sed -n '1,240p' /x/SKILL.md"));

        let m = item(json!({"type":"custom_tool_call","name":"exec","call_id":"call_3",
            "input":"const r = await tools.mcp__mulpex__hub_send({\"to\":\"2\",\"message\":\"hi there\"});"}));
        assert!(matches!(&parse_line(&m)[0], Item::Tool { name, summary, .. } if name == "hub_send" && summary == "hi there"));

        let p = item(json!({"type":"custom_tool_call","name":"apply_patch","call_id":"call_4",
            "input":"*** Begin Patch\n*** Update File: packages/a/permissions.ts\n@@\n+x\n*** Add File: /abs/b.rs\n+y\n*** End Patch"}));
        assert!(matches!(&parse_line(&p)[0], Item::Tool { name, summary, .. } if name == "apply_patch" && summary == "permissions.ts, b.rs"));

        let o = item(json!({"type":"custom_tool_call_output","call_id":"call_2",
            "output":"[{\"type\":\"input_text\",\"text\":\"Script completed\\nWall time 0.1 seconds\\nOutput:\\n\"},{\"type\":\"input_text\",\"text\":\"---\\nname: imagegen\"}]"}));
        assert_eq!(
            parse_line(&o),
            vec![Item::Result { id: "call_2".into(), text: "Script completed\nWall time 0.1 seconds\nOutput:\n---\nname: imagegen".into(), error: false }]
        );
        let plain = item(json!({"type":"function_call_output","call_id":"call_1","output":"M a.rs\n"}));
        assert_eq!(parse_line(&plain), vec![Item::Result { id: "call_1".into(), text: "M a.rs\n".into(), error: false }]);
    }

    /// Every line of this machine's real rollouts parses without panicking,
    /// and a rollout with a conversation produces bubbles. Skipped where codex
    /// has never run.
    #[test]
    fn real_rollouts_parse() {
        let root = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".codex/sessions");
        let mut files = Vec::new();
        let mut stack = vec![root];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                    && e.metadata().map(|m| m.len() < 8 << 20).unwrap_or(false)
                {
                    files.push(p);
                }
            }
        }
        files.truncate(20);
        let (mut users, mut texts) = (0, 0);
        for f in &files {
            let Ok(text) = std::fs::read_to_string(f) else { continue };
            for l in text.lines() {
                for i in parse_line(l) {
                    match i {
                        Item::User { text } => {
                            assert!(!text.contains("<environment_context>"), "{f:?}: {text}");
                            users += 1
                        }
                        Item::Text { .. } => texts += 1,
                        _ => {}
                    }
                }
            }
        }
        eprintln!("codex rollouts: {} files, {users} user bubbles, {texts} replies", files.len());
        if !files.is_empty() {
            assert!(texts > 0, "{} rollouts produced no replies", files.len());
        }
    }
}
