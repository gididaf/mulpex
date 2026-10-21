//! ⌘⌥S "Continue in Codex / Continue in Claude": hand one row's work to the
//! other agent, in place. The reason it exists is to keep working when one
//! account hits its 5-hour or weekly limit (docs/codex.md).
//!
//! Only the save's WRITE step runs (no check, no fix — this is the fast path):
//!
//! 1. the **source** agent forks its own conversation and writes the handoff
//!    doc, exactly like ⌘S's first step (`saves.rs`);
//! 2. if the source can't serve — out of quota, logged out, not installed
//!    (`headless::is_unavailable`) — the **target** agent writes the doc fresh
//!    from the rendered transcript (`agent_transcript::render`, the last
//!    ~120 KB), checking the repo itself;
//! 3. both fail → the error shows and the row stays held as a saved row
//!    (`Core::begin_save` already stopped it), so nothing is lost.
//!
//! The doc lands in `mulpex/saves/` like any save, and the row is then
//! replaced by the other agent (`Core::replace_with_agent`: same number,
//! name, parent and inbox) with the doc as its first prompt.

use std::path::Path;

use mulpex_core::agent::Agent;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::headless::{self, Job};
use crate::saves::{self, Draft};
use crate::snapshot::{ProjectHandle, SaveProgress};
use crate::state::AppState;

const FALLBACK_PROMPT: &str = include_str!("save_prompts/handoff_fallback.md");

/// How much of the rendered conversation the fallback writer gets.
const RENDER_BYTES: usize = 120 * 1024;

/// What the chain needs about the row being handed off.
struct Source<'a> {
    agent: Agent,
    dir: &'a Path,
    session_id: &'a str,
    /// Live Token route of the row (claude only).
    route: &'a str,
    existing: Option<&'a Path>,
}

/// ⌘⌥S on `id`: hand its work to the other agent. Refuses what ⌘S refuses,
/// and when the other agent is not installed. Returns once the handoff is
/// under way; progress arrives as `save-progress` (`writing`, then
/// `handed-off` with the doc's path, or `error` with the reason — the row is
/// then held like a failed save). On success the row is replaced and
/// `session-restarted` is emitted for it.
#[tauri::command]
pub fn continue_in_other_agent(
    app: AppHandle,
    state: State<AppState>,
    project_handle: ProjectHandle,
    id: usize,
) -> Result<(), String> {
    let handle = project_handle;
    let (source, dir, state_dir, uuid) = {
        let mut ws = state.ws.lock().unwrap();
        let core = ws.project_mut(handle).ok_or("no such project")?;
        let source = core.agent_of(id).ok_or_else(|| format!("term#{id} is a terminal — only an agent can be handed off"))?;
        let target = source.other();
        if let Some(why) = core.save_refusal(id) {
            return Err(why);
        }
        headless::binary(target).map_err(|_| format!("{} is not installed", target.title()))?;
        if saves::is_running(handle, id) {
            return Err(format!("{}#{id} is already being saved", source.word()));
        }
        let (dir, uuid) = core.begin_save(id)?;
        (source, dir, core.state_dir.clone(), uuid)
    };
    if !saves::claim(handle, id) {
        return Err(format!("{}#{id} is already being saved", source.word()));
    }
    let target = source.other();
    std::thread::spawn(move || {
        let emit = |st: &str, detail: Option<String>, title: Option<String>| {
            let _ = app.emit("save-progress", SaveProgress { handle, id, state: st.into(), detail, title });
        };
        emit("writing", None, None);
        let existing = saves::existing_save(&uuid);
        let route = crate::live_token::route(&dir, id);
        let src = Source { agent: source, dir: &dir, session_id: &uuid, route: &route, existing: existing.as_deref() };
        let render = || {
            crate::agent_transcript::path(source, &dir, &uuid, &state_dir, id)
                .filter(|p| p.is_file())
                .and_then(|p| crate::agent_transcript::render(source, &p, RENDER_BYTES))
        };
        let note = |reason: &str| {
            eprintln!("[handoff] {}#{id}: {} can't write the doc ({reason}); {} writes it from the transcript",
                source.word(), source.title(), target.title());
            emit("writing", Some(format!("{} is unavailable — {} is writing the doc", source.title(), target.title())), None);
        };
        let result = write_doc(&src, headless::run, render, note)
            .and_then(|draft| saves::store(&dir, &draft, existing.as_deref()));
        let result = result.and_then(|path| {
            saves::link_agent("saved", source, &uuid, &dir, &path);
            let app_state = app.state::<AppState>();
            let mut ws = app_state.ws.lock().unwrap();
            let core = ws.project_mut(handle).ok_or("the project was closed")?;
            core.replace_with_agent(id, target, saves::load_prompt(&path))
                .map_err(|e| format!("the doc is saved, but {} did not start: {e}", target.title()))?;
            if let Some(sid) = core.session_id_of(id) {
                saves::link_agent("loaded", target, &sid, &dir, &path);
            }
            Ok(path)
        });
        saves::release(handle, id);
        match result {
            Ok(path) => {
                emit("handed-off", Some(path.display().to_string()), saves::title_of(&path));
                let _ = app.emit("session-restarted", crate::snapshot::SessionExited { handle, id });
            }
            Err(reason) => {
                eprintln!("[handoff] {}#{id} failed: {reason}", source.word());
                emit("error", Some(reason), None);
            }
        }
    });
    Ok(())
}

/// The write chain, with the runner and the transcript render passed in so it
/// can be tested without an agent. `fell_back` is told why the source could
/// not write, before the target is asked.
fn write_doc(
    src: &Source,
    run: impl Fn(Agent, &Job) -> Result<String, String>,
    render: impl FnOnce() -> Option<String>,
    fell_back: impl FnOnce(&str),
) -> Result<Draft, String> {
    let prompt = saves::write_prompt(src.existing);
    let job = Job {
        dir: src.dir,
        model: "opus",
        route: Some(src.route),
        fork: Some(src.session_id),
        prompt: &prompt,
        timeout: saves::STEP_TIMEOUT,
    };
    let first = run(src.agent, &job);
    let reason = match first.and_then(|text| saves::parse_draft(&text)) {
        Ok(draft) => return Ok(draft),
        Err(reason) if headless::is_unavailable(&reason) => reason,
        Err(reason) => return Err(format!("{} could not write the doc: {reason}", src.agent.title())),
    };
    fell_back(&reason);
    let target = src.agent.other();
    let transcript = render().ok_or_else(|| {
        format!("{} is unavailable ({reason}), and its conversation could not be read", src.agent.title())
    })?;
    let prompt = fallback_prompt(&transcript, src.existing);
    let job = Job {
        dir: src.dir,
        model: "opus",
        route: Some(src.route),
        fork: None,
        prompt: &prompt,
        timeout: saves::STEP_TIMEOUT,
    };
    run(target, &job).and_then(|text| saves::parse_draft(&text)).map_err(|e| {
        format!("{} is unavailable ({reason}), and {} could not write the doc either: {e}", src.agent.title(), target.title())
    })
}

/// The target's prompt: the rules, the earlier save if any, the conversation.
fn fallback_prompt(transcript: &str, existing: Option<&Path>) -> String {
    let current = existing
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|doc| {
            format!(
                "\n\nThis work was saved before, and that doc is below. Produce ONE current doc that \
replaces it: keep what is still relevant, update what changed, drop what is no longer true. \
Keep its `slug`.\n\n<current_doc>\n{doc}\n</current_doc>\n"
            )
        })
        .unwrap_or_default();
    format!("{FALLBACK_PROMPT}{current}\n\n<conversation>\n{transcript}\n</conversation>\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    const DOC: &str = r###"{"slug":"x","title_he":"כותרת","description_he":"d","body":"## Goal\ng"}"###;

    fn src<'a>(agent: Agent) -> Source<'a> {
        Source { agent, dir: Path::new("/nonexistent"), session_id: "sid", route: "", existing: None }
    }

    /// Runs recorded as (agent, forked?, prompt starts with the fallback).
    type Calls = RefCell<Vec<(Agent, bool, bool)>>;

    fn fake<'c>(calls: &'c Calls, answers: Vec<Result<&'static str, &'static str>>) -> impl Fn(Agent, &Job) -> Result<String, String> + 'c {
        let answers = RefCell::new(answers.into_iter());
        move |agent, job| {
            calls.borrow_mut().push((agent, job.fork.is_some(), job.prompt.starts_with("[mulpex:handoff]")));
            answers.borrow_mut().next().unwrap().map(str::to_string).map_err(str::to_string)
        }
    }

    #[test]
    fn the_source_fork_writes_it_when_it_can() {
        let calls = Calls::default();
        let d = write_doc(&src(Agent::Claude), fake(&calls, vec![Ok(DOC)]), || panic!("no render"), |_| panic!("no fallback")).unwrap();
        assert_eq!(d.slug, "x");
        assert_eq!(*calls.borrow(), [(Agent::Claude, true, false)]);
    }

    #[test]
    fn a_limited_source_hands_the_writing_to_the_target() {
        let calls = Calls::default();
        let fell = RefCell::new(String::new());
        let d = write_doc(
            &src(Agent::Claude),
            fake(&calls, vec![Err("exit 1: Claude AI usage limit reached|1792580000"), Ok(DOC)]),
            || Some("[USER]\nhi\n".into()),
            |r| *fell.borrow_mut() = r.to_string(),
        )
        .unwrap();
        assert_eq!(d.slug, "x");
        assert_eq!(*calls.borrow(), [(Agent::Claude, true, false), (Agent::Codex, false, true)]);
        assert!(fell.borrow().contains("usage limit"));

        // Codex → Claude the same way, on a 401 or a missing binary.
        for why in ["exit 1: unexpected status 401 Unauthorized", "codex not found"] {
            let calls = Calls::default();
            write_doc(&src(Agent::Codex), fake(&calls, vec![Err(why), Ok(DOC)]), || Some("t".into()), |_| {}).unwrap();
            assert_eq!(*calls.borrow(), [(Agent::Codex, true, false), (Agent::Claude, false, true)], "{why}");
        }
    }

    #[test]
    fn any_other_failure_is_not_papered_over() {
        let calls = Calls::default();
        let e = write_doc(&src(Agent::Claude), fake(&calls, vec![Err("timeout after 15 min")]), || panic!(), |_| panic!())
            .unwrap_err();
        assert!(e.contains("timeout"), "{e}");
        assert_eq!(calls.borrow().len(), 1);
        // A bad answer is the source's failure too, not a reason to switch.
        let calls = Calls::default();
        assert!(write_doc(&src(Agent::Claude), fake(&calls, vec![Ok("no json")]), || panic!(), |_| panic!()).is_err());
    }

    #[test]
    fn both_failing_says_both_reasons() {
        let calls = Calls::default();
        let e = write_doc(
            &src(Agent::Claude),
            fake(&calls, vec![Err("API Error: 429 rate_limit_error"), Err("exit 1: unexpected status 401 Unauthorized")]),
            || Some("t".into()),
            |_| {},
        )
        .unwrap_err();
        assert!(e.contains("429") && e.contains("401") && e.contains("Codex"), "{e}");
        // No transcript to hand over: say so, and don't ask the target blind.
        let calls = Calls::default();
        let e = write_doc(&src(Agent::Claude), fake(&calls, vec![Err("usage limit")]), || None, |_| {}).unwrap_err();
        assert!(e.contains("could not be read"), "{e}");
        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn the_fallback_prompt_carries_the_conversation() {
        let p = fallback_prompt("[USER]\nfix it\n", None);
        assert!(p.starts_with("[mulpex:handoff]"));
        assert!(p.contains("<conversation>\n[USER]\nfix it\n\n</conversation>"));
        assert!(p.contains("\"slug\""));
    }

    /// A real `codex exec fork` writes a handoff doc from a real rollout, and
    /// leaves that rollout byte-identical. Manual: `HANDOFF_TEST_DIR=<the
    /// rollout's cwd> HANDOFF_TEST_THREAD=<thread id> cargo test live_codex_fork
    /// -- --ignored --nocapture`. Writes nothing into the repo.
    #[test]
    #[ignore]
    fn live_codex_fork_writes_a_doc() {
        let dir = PathBuf::from(std::env::var("HANDOFF_TEST_DIR").unwrap());
        let sid = std::env::var("HANDOFF_TEST_THREAD").unwrap();
        let rollout = crate::agent_transcript::path(Agent::Codex, &dir, &sid, Path::new("/nonexistent"), 0)
            .expect("no rollout for that thread");
        let before = std::fs::read(&rollout).unwrap();
        let s = Source { agent: Agent::Codex, dir: &dir, session_id: &sid, route: "", existing: None };
        let started = std::time::Instant::now();
        let draft = write_doc(&s, headless::run, || None, |r| panic!("fell back: {r}")).unwrap();
        eprintln!("{:?} — {} / {}\n{}", started.elapsed(), draft.title_he, draft.description_he, draft.body);
        assert!(draft.body.contains("## Goal"));
        assert_eq!(std::fs::read(&rollout).unwrap(), before, "the fork must not touch the original rollout");
    }
}
