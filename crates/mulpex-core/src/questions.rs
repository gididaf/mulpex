//! Async questions: an `AskUserQuestion` call the model flags with
//! `metadata: {"source": "async"}` does not open Claude Code's blocking dialog.
//! The `askq` hook saves it here as a **card** and denies the tool with a
//! reason that says "queued, keep going"; the user answers from the side
//! panel's Questions tab whenever they like, and the app sends the answer back
//! as hub mail from Mulpex. → docs/hub.md "Async questions".
//!
//! The helper (hook + `hub_question_edit` / `hub_question_cancel`) and the app
//! (reads, answers, dismisses) are different processes, so this file is the
//! one format both agree on.
//!
//! Layout, under the project's scratch dir:
//! - `questions/<instance>/<qid>.json` — one open card: [`Card`].
//! - `questions/ids/<qid>` — an empty file per qid ever handed out; created with
//!   `create_new`, which is what makes a qid unique across every instance of the
//!   project without a lock.
//!
//! A card is removed when it is answered, dismissed or cancelled, and the
//! instance's whole folder goes when its row is reaped.

use serde_json::Value;
use std::path::{Path, PathBuf};

pub const QUESTIONS_DIR: &str = "questions";

/// The `metadata.source` value that makes an AskUserQuestion async.
pub const ASYNC_SOURCE: &str = "async";

#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub qid: String,
    pub instance: usize,
    /// Unix seconds when first asked.
    pub ts: u64,
    /// Bumped by every `hub_question_edit`, so the UI knows to re-render.
    pub rev: u32,
    /// AskUserQuestion's own `questions` array, as the model wrote it.
    pub questions: Value,
}

/// One question's answer from the card UI, index-aligned with `questions`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Answer {
    /// Option labels the user picked (one for single-select).
    pub picks: Vec<String>,
    /// The "Other" free text, if used.
    pub other: Option<String>,
    /// Free notes on this question.
    pub notes: Option<String>,
}

impl Card {
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "qid": self.qid, "instance": self.instance, "ts": self.ts,
            "rev": self.rev, "questions": self.questions,
        })
    }

    pub fn from_json(v: &Value) -> Option<Card> {
        Some(Card {
            qid: v.get("qid")?.as_str()?.to_string(),
            instance: v.get("instance")?.as_u64()? as usize,
            ts: v.get("ts").and_then(Value::as_u64).unwrap_or(0),
            rev: v.get("rev").and_then(Value::as_u64).unwrap_or(1) as u32,
            questions: v.get("questions")?.clone(),
        })
    }
}

impl Answer {
    /// `{picks: [..], other?: "..", notes?: ".."}` from the UI.
    pub fn from_json(v: &Value) -> Answer {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Answer {
            picks: v
                .get("picks")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            other: s("other"),
            notes: s("notes"),
        }
    }
}

/// Is this AskUserQuestion `tool_input` flagged async?
pub fn is_async(tool_input: &Value) -> bool {
    tool_input
        .pointer("/metadata/source")
        .and_then(Value::as_str)
        .is_some_and(|s| s.trim().eq_ignore_ascii_case(ASYNC_SOURCE))
}

pub fn instance_dir(state_dir: &Path, instance: usize) -> PathBuf {
    state_dir.join(QUESTIONS_DIR).join(instance.to_string())
}

pub fn card_path(state_dir: &Path, instance: usize, qid: &str) -> PathBuf {
    instance_dir(state_dir, instance).join(format!("{qid}.json"))
}

/// A qid nobody in this project has had: `q1`, `q2`, …
pub fn alloc_qid(state_dir: &Path) -> std::io::Result<String> {
    let ids = state_dir.join(QUESTIONS_DIR).join("ids");
    std::fs::create_dir_all(&ids)?;
    let start = std::fs::read_dir(&ids)
        .map(|d| d.flatten().count())
        .unwrap_or(0)
        + 1;
    for n in start..start + 10_000 {
        let qid = format!("q{n}");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(ids.join(&qid))
        {
            Ok(_) => return Ok(qid),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free question id"))
}

pub fn write_card(state_dir: &Path, card: &Card) -> std::io::Result<()> {
    let path = card_path(state_dir, card.instance, &card.qid);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, card.to_json().to_string())?;
    std::fs::rename(&tmp, &path)
}

/// Save a new card for `questions` and return it.
pub fn create(state_dir: &Path, instance: usize, questions: Value) -> std::io::Result<Card> {
    let card = Card {
        qid: alloc_qid(state_dir)?,
        instance,
        ts: now(),
        rev: 1,
        questions,
    };
    write_card(state_dir, &card)?;
    Ok(card)
}

/// Every open card in the project, oldest first.
pub fn list(state_dir: &Path) -> Vec<Card> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(state_dir.join(QUESTIONS_DIR)) else {
        return out;
    };
    for d in dirs.flatten() {
        let Some(inst) = d.file_name().to_str().and_then(|s| s.parse::<usize>().ok()) else {
            continue; // `ids/`
        };
        let Ok(files) = std::fs::read_dir(d.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(c) = std::fs::read(&p)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| Card::from_json(&v))
            {
                if c.instance == inst {
                    out.push(c);
                }
            }
        }
    }
    out.sort_by(|a, b| (a.ts, qnum(&a.qid)).cmp(&(b.ts, qnum(&b.qid))));
    out
}

pub fn read(state_dir: &Path, instance: usize, qid: &str) -> Option<Card> {
    std::fs::read(card_path(state_dir, instance, qid))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| Card::from_json(&v))
}

/// Remove one card. `true` if it was there.
pub fn remove(state_dir: &Path, instance: usize, qid: &str) -> bool {
    std::fs::remove_file(card_path(state_dir, instance, qid)).is_ok()
}

/// Drop every card of a closed instance.
pub fn forget_instance(state_dir: &Path, instance: usize) {
    let _ = std::fs::remove_dir_all(instance_dir(state_dir, instance));
}

/// What changed, cheaply: `(instance, qid, rev)` of every open card. The poll
/// loop compares this between ticks.
pub fn signature(state_dir: &Path) -> Vec<(usize, String, u32)> {
    list(state_dir)
        .into_iter()
        .map(|c| (c.instance, c.qid, c.rev))
        .collect()
}

/// `hub_question_edit`: replace an open card's questions. The caller must own it.
pub fn edit(state_dir: &Path, instance: usize, qid: &str, questions: Value) -> Result<Card, String> {
    validate(&questions)?;
    let mut card = read(state_dir, instance, qid).ok_or_else(|| gone(qid))?;
    card.questions = questions;
    card.rev += 1;
    write_card(state_dir, &card).map_err(|e| e.to_string())?;
    Ok(card)
}

/// `hub_question_cancel`.
pub fn cancel(state_dir: &Path, instance: usize, qid: &str) -> Result<(), String> {
    if remove(state_dir, instance, qid) {
        Ok(())
    } else {
        Err(gone(qid))
    }
}

fn gone(qid: &str) -> String {
    format!(
        "no open async question {qid} of yours — it was already answered, dismissed or cancelled \
         (or it belongs to another instance)"
    )
}

/// The same shape AskUserQuestion takes: 1–4 questions, each with text and 2+ options.
pub fn validate(questions: &Value) -> Result<(), String> {
    let arr = questions.as_array().ok_or("`questions` must be an array")?;
    if arr.is_empty() || arr.len() > 4 {
        return Err("`questions` must hold 1 to 4 questions".into());
    }
    for q in arr {
        if q.get("question").and_then(Value::as_str).is_none_or(|s| s.trim().is_empty()) {
            return Err("every question needs a `question` text".into());
        }
        let n = q.get("options").and_then(Value::as_array).map_or(0, Vec::len);
        if n < 2 {
            return Err("every question needs at least 2 `options`".into());
        }
    }
    Ok(())
}

/// The deny reason the model reads in place of a tool result.
pub fn queued_reason(qid: &str) -> String {
    // Claude Code prints this in red as a "hook error", so it stays one line;
    // the full contract is in `PLANNING_RULES` ("ASYNC QUESTIONS").
    format!(
        "Queued as async question {qid} (not an error). The answer will come as a Mulpex message \
         \"[async {qid}]\"."
    )
}

/// The hub message for an answered card — the wording of AskUserQuestion's own
/// result, which the model already knows.
pub fn answer_message(card: &Card, answers: &[Answer]) -> String {
    let qs = card.questions.as_array().cloned().unwrap_or_default();
    let mut parts = Vec::new();
    for (i, q) in qs.iter().enumerate() {
        let text = q.get("question").and_then(Value::as_str).unwrap_or("");
        let a = answers.get(i).cloned().unwrap_or_default();
        let mut picked: Vec<String> = a.picks.clone();
        if let Some(o) = a.other.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            picked.push(o.to_string());
        }
        let mut s = format!("\"{text}\"=\"{}\"", picked.join(", "));
        if let Some(n) = a.notes.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            s.push_str(&format!(" (user notes: {n})"));
        }
        parts.push(s);
    }
    format!(
        "[async {}] Your questions have been answered: {}. You can now continue with these answers \
         in mind.",
        card.qid,
        parts.join(", ")
    )
}

pub fn dismissed_message(qid: &str) -> String {
    format!(
        "[async {qid}] Question {qid} was dismissed by the user without an answer. Use your best \
         judgment, and say what you chose."
    )
}

fn qnum(qid: &str) -> u64 {
    qid.trim_start_matches('q').parse().unwrap_or(0)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Tmp(PathBuf);
    impl Tmp {
        fn path(&self) -> &Path { &self.0 }
    }
    impl Drop for Tmp {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }
    fn tmp() -> Tmp {
        let d = std::env::temp_dir().join(format!("mulpex-q-{}", crate::persist::new_uuid()));
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    fn qs() -> Value {
        json!([{"question":"Which color?","header":"Color","multiSelect":false,
                "options":[{"label":"Red","description":""},{"label":"Blue","description":""}]}])
    }

    #[test]
    fn flag_is_metadata_source_async() {
        assert!(is_async(&json!({"questions":[],"metadata":{"source":"async"}})));
        assert!(!is_async(&json!({"questions":[]})));
        assert!(!is_async(&json!({"metadata":{"source":"remember"}})));
    }

    #[test]
    fn create_list_edit_cancel() {
        let d = tmp();
        let a = create(d.path(), 3, qs()).unwrap();
        let b = create(d.path(), 5, qs()).unwrap();
        assert_ne!(a.qid, b.qid);
        assert_eq!(list(d.path()).len(), 2);
        // Another instance can't touch it.
        assert!(edit(d.path(), 5, &a.qid, qs()).is_err());
        let e = edit(d.path(), 3, &a.qid, qs()).unwrap();
        assert_eq!(e.rev, 2);
        assert!(edit(d.path(), 3, &a.qid, json!([])).is_err());
        cancel(d.path(), 3, &a.qid).unwrap();
        assert!(cancel(d.path(), 3, &a.qid).is_err());
        forget_instance(d.path(), 5);
        assert!(list(d.path()).is_empty());
        // qids are never reused, even after the cards are gone.
        let c = create(d.path(), 3, qs()).unwrap();
        assert!(c.qid != a.qid && c.qid != b.qid);
    }

    #[test]
    fn answer_reads_like_the_real_tool_result() {
        let card = Card { qid: "q7".into(), instance: 1, ts: 0, rev: 1, questions: qs() };
        let m = answer_message(
            &card,
            &[Answer { picks: vec!["Red".into()], other: None, notes: Some("dark".into()) }],
        );
        assert_eq!(
            m,
            "[async q7] Your questions have been answered: \"Which color?\"=\"Red\" (user notes: \
             dark). You can now continue with these answers in mind."
        );
    }
}
