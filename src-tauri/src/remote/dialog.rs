//! A claude's open dialog — an `AskUserQuestion` or a plan up for approval — as
//! the phone draws it, and the phone's answer turned back into keystrokes.
//!
//! The dialog comes from `dialog/<id>.json`, which the `askq`/`plan` hooks write
//! (`mulpex_core::DIALOG_DIR`). The keys are what claude's own dialog takes,
//! measured on a real `claude` 2.1.292 on a PTY (answers checked in the
//! transcript's `tool_result`):
//!
//! - Single choice: the option's digit picks it and moves on.
//! - "Type something" (one past the last option): in a single choice, its digit
//!   puts the cursor in the text field; type, then Enter. In a multi-select its
//!   digit only ticks the box — the cursor has to be moved onto the row (↓ from
//!   row 1) before typing reaches it.
//! - Multi-select: digits toggle and don't move the cursor. Tab moves on — from
//!   a plain row to the next tab, but from inside the text field only to the
//!   in-list "Submit" row, which then needs Enter.
//! - With more than one question, or any multi-select, a review tab follows,
//!   where `1` is "Submit answers". One single-choice question has none.
//! - Plan approval: `1` approve, `2` approve but review edits, `3` + text +
//!   Enter sends feedback instead.

use std::path::Path;

use serde_json::{json, Value};

const MAX_PLAN: usize = 30_000;

fn read_raw(state_dir: &Path, id: usize) -> Option<Value> {
    let s = std::fs::read_to_string(mulpex_core::dialog_path(state_dir, id)).ok()?;
    serde_json::from_str(&s).ok()
}

/// The dialog as the phone draws it, or `None` if there is none we can offer.
pub fn read(state_dir: &Path, id: usize) -> Option<Value> {
    phone_view(&read_raw(state_dir, id)?)
}

fn phone_view(raw: &Value) -> Option<Value> {
    let input = raw.get("input")?;
    match raw.get("tool").and_then(Value::as_str)? {
        "AskUserQuestion" => {
            let qs = input.get("questions")?.as_array()?;
            let questions: Vec<Value> = qs
                .iter()
                .map(|q| {
                    let s = |k: &str| q.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                    let options: Vec<Value> = q
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|o| {
                            o.iter()
                                .map(|o| json!({
                                    "label": o.get("label").and_then(Value::as_str).unwrap_or(""),
                                    "description": o.get("description").and_then(Value::as_str).unwrap_or(""),
                                }))
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({
                        "question": s("question"),
                        "header": s("header"),
                        "multi": q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                        "options": options,
                    })
                })
                .collect();
            (!questions.is_empty()).then(|| json!({"kind": "ask", "questions": questions}))
        }
        "ExitPlanMode" => {
            let plan = input.get("plan").and_then(Value::as_str).unwrap_or("");
            let plan: String = plan.chars().take(MAX_PLAN).collect();
            Some(json!({"kind": "plan", "plan": plan}))
        }
        _ => None,
    }
}

const DOWN: &str = "\x1b[B";

fn digit(n: usize) -> Result<String, String> {
    if (1..=9).contains(&n) {
        Ok(n.to_string())
    } else {
        Err("That dialog has more options than the phone can pick".into())
    }
}

/// A paste of the user's own text. Control characters out: an ESC inside could
/// end the paste early and turn the rest into keystrokes.
fn paste(text: &str) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b[200~{clean}\x1b[201~")
}

/// The keystrokes that answer the dialog claude `id` is showing now, one write
/// each (the desktop plays them with a short gap). The dialog is re-read here
/// rather than trusted from the phone, and the answer must fit it exactly.
pub fn keys(state_dir: &Path, id: usize, answer: &Value) -> Result<Vec<String>, String> {
    let raw = read_raw(state_dir, id).ok_or("That question is no longer open")?;
    let view = phone_view(&raw).ok_or("That question is no longer open")?;
    keys_for(&view, answer)
}

fn keys_for(view: &Value, answer: &Value) -> Result<Vec<String>, String> {
    match view.get("kind").and_then(Value::as_str) {
        Some("plan") => plan_keys(answer),
        Some("ask") => ask_keys(view["questions"].as_array().ok_or("bad dialog")?, answer),
        _ => Err("That question is no longer open".into()),
    }
}

fn plan_keys(answer: &Value) -> Result<Vec<String>, String> {
    match answer.get("choice").and_then(Value::as_u64) {
        Some(1) => Ok(vec!["1".into()]),
        Some(2) => Ok(vec!["2".into()]),
        Some(3) => {
            let text = answer.get("text").and_then(Value::as_str).unwrap_or("").trim();
            if text.is_empty() {
                return Err("Say what to change".into());
            }
            Ok(vec!["3".into(), paste(text), "\r".into()])
        }
        _ => Err("Pick an option".into()),
    }
}

fn ask_keys(questions: &[Value], answer: &Value) -> Result<Vec<String>, String> {
    let answers = answer.get("answers").and_then(Value::as_array).ok_or("No answers")?;
    if answers.len() != questions.len() {
        return Err("The question changed — reopen it".into());
    }
    let mut out = Vec::new();
    let mut review = questions.len() > 1;
    for (q, a) in questions.iter().zip(answers) {
        let n = q["options"].as_array().map_or(0, Vec::len);
        let multi = q["multi"].as_bool().unwrap_or(false);
        let picks: Vec<usize> = a
            .get("pick")
            .and_then(Value::as_array)
            .map(|p| p.iter().filter_map(Value::as_u64).map(|x| x as usize).collect())
            .unwrap_or_default();
        if picks.iter().any(|&p| p >= n) {
            return Err("The question changed — reopen it".into());
        }
        let other = a.get("other").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty());
        if multi {
            review = true;
            if picks.is_empty() && other.is_none() {
                return Err("Pick at least one answer for every question".into());
            }
            for p in &picks {
                out.push(digit(p + 1)?);
            }
            match other {
                Some(text) => {
                    out.push(digit(n + 1)?);
                    out.extend(std::iter::repeat(DOWN.to_string()).take(n));
                    out.push(paste(text));
                    // From inside the text field Tab only reaches the in-list
                    // Submit row; Enter there moves on.
                    out.push("\t".into());
                    out.push("\r".into());
                }
                None => out.push("\t".into()),
            }
        } else {
            match (other, picks.as_slice()) {
                (Some(text), []) => {
                    out.push(digit(n + 1)?);
                    out.push(paste(text));
                    out.push("\r".into());
                }
                (None, [p]) => out.push(digit(p + 1)?),
                _ => return Err("Pick one answer for every question".into()),
            }
        }
    }
    if review {
        out.push("1".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(multi: bool, labels: &[&str]) -> Value {
        json!({
            "question": "Q?", "header": "H", "multiSelect": multi,
            "options": labels.iter().map(|l| json!({"label": l, "description": ""})).collect::<Vec<_>>(),
        })
    }

    fn ask(qs: Vec<Value>) -> Value {
        phone_view(&json!({"tool": "AskUserQuestion", "input": {"questions": qs}})).unwrap()
    }

    /// Each case is a sequence that was played into a real claude 2.1.292 and
    /// produced the expected `tool_result`.
    #[test]
    fn the_measured_sequences() {
        // "Pick a color?" → Green, "Which pets?" → Cats, Fish.
        let v = ask(vec![q(false, &["Red", "Green", "Blue"]), q(true, &["Cats", "Dogs", "Fish"])]);
        let a = json!({"answers": [{"pick": [1]}, {"pick": [0, 2]}]});
        assert_eq!(keys_for(&v, &a).unwrap(), vec!["2", "1", "3", "\t", "1"]);

        // "Pick a fruit?" → a typed answer, Hebrew included; no review tab.
        let v = ask(vec![q(false, &["Apple", "Pear"])]);
        let a = json!({"answers": [{"other": "Mango שלום"}]});
        assert_eq!(keys_for(&v, &a).unwrap(), vec!["3", "\x1b[200~Mango שלום\x1b[201~", "\r"]);

        // "Which tools?" → Hammer + a typed "Drill".
        let v = ask(vec![q(true, &["Hammer", "Saw"])]);
        let a = json!({"answers": [{"pick": [0], "other": "Drill"}]});
        assert_eq!(
            keys_for(&v, &a).unwrap(),
            vec!["1", "3", DOWN, DOWN, "\x1b[200~Drill\x1b[201~", "\t", "\r", "1"]
        );
    }

    #[test]
    fn one_single_choice_question_has_no_review() {
        let v = ask(vec![q(false, &["A", "B"])]);
        assert_eq!(keys_for(&v, &json!({"answers": [{"pick": [0]}]})).unwrap(), vec!["1"]);
    }

    #[test]
    fn plans() {
        let v = phone_view(&json!({"tool": "ExitPlanMode", "input": {"plan": "# P\n1. x"}})).unwrap();
        assert_eq!(v["plan"], "# P\n1. x");
        assert_eq!(keys_for(&v, &json!({"choice": 1})).unwrap(), vec!["1"]);
        assert_eq!(keys_for(&v, &json!({"choice": 2})).unwrap(), vec!["2"]);
        assert_eq!(
            keys_for(&v, &json!({"choice": 3, "text": "use hello.md"})).unwrap(),
            vec!["3", "\x1b[200~use hello.md\x1b[201~", "\r"]
        );
        assert!(keys_for(&v, &json!({"choice": 3, "text": "  "})).is_err());
    }

    #[test]
    fn an_answer_that_does_not_fit_the_dialog_is_refused() {
        let v = ask(vec![q(false, &["A", "B"]), q(true, &["C"])]);
        assert!(keys_for(&v, &json!({"answers": [{"pick": [0]}]})).is_err(), "too few");
        assert!(keys_for(&v, &json!({"answers": [{"pick": [5]}, {"pick": [0]}]})).is_err(), "out of range");
        assert!(keys_for(&v, &json!({"answers": [{"pick": [0, 1]}, {"pick": [0]}]})).is_err(), "two singles");
        assert!(keys_for(&v, &json!({"answers": [{"pick": [0]}, {"pick": []}]})).is_err(), "empty multi");
    }

    #[test]
    fn typed_text_cannot_escape_the_paste() {
        let v = ask(vec![q(false, &["A"])]);
        let k = keys_for(&v, &json!({"answers": [{"other": "x\x1b[201~\x03rm"}]})).unwrap();
        assert_eq!(k[1], "\x1b[200~x[201~rm\x1b[201~");
    }
}
