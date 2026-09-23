//! Save Session (⌘S): turn one claude's unfinished work into a handoff doc in
//! the repo (`mulpex/saves/<slug>.md`), with a short Hebrew title and
//! description for the human browsing the list, and an English body for the
//! claude that picks the work up.
//!
//! **The doc must stand alone.** Claude Code deletes conversations after 30
//! days (`cleanupPeriodDays`), and a coworker never had the conversation at
//! all — so nothing here may assume the transcript survives.
//!
//! Three headless steps, each a `claude -p` child:
//!
//! 1. **write** — a hidden fork of the instance's own conversation
//!    (`--resume <uuid> --fork-session`) writes the doc. It has the whole
//!    conversation in context, which is what makes the doc as good as asking the
//!    instance itself, without typing anything into its TUI.
//! 2. **check** — a claude with no memory reads only the doc (plus the repo) and
//!    lists what it could not continue without.
//! 3. **fix** — skipped on `NONE`; otherwise a second fork answers those gaps.
//!
//! Measured 2026-09-23 (docs/verification-log.md): the fork leaves the original
//! `.jsonl` byte-identical (on a live instance, the old bytes are intact and the
//! instance's own appends continue), `--no-session-persistence` leaves no new
//! transcript behind, and a save takes ~2.5 min / ~$2–4 on Opus. The check step
//! is worth its cost: on the first real save it found an unmentioned crash path.
//!
//! Every step is read-only — `Read/Grep/Glob` plus read-only git — and Mulpex,
//! not the child, writes the file. Each save runs on its own thread; there is no
//! queue, because saves are rare and a second ⌘S on the same instance is refused
//! while one is running.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use tauri::{AppHandle, Emitter};

use crate::claude_bin;
use crate::snapshot::{ProjectHandle, SaveProgress};

const WRITE_PROMPT: &str = include_str!("save_prompts/write.md");
const CHECK_PROMPT: &str = include_str!("save_prompts/check.md");
const FIX_PROMPT: &str = include_str!("save_prompts/fix.md");

/// Per step. A fork re-reads the whole conversation and then explores the repo;
/// the measured steps took 30–70 s, so this only catches a hung child.
const STEP_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Where saves live, relative to the repo root.
pub const SAVES_DIR: &str = "mulpex/saves";

/// Instances with a save in flight, so a second ⌘S is refused instead of
/// racing the first one to the same slug.
static RUNNING: Mutex<Option<HashSet<(ProjectHandle, usize)>>> = Mutex::new(None);

/// What the fork returns. `body` is the English doc; the rest is the header.
#[derive(Debug, Deserialize, PartialEq)]
struct Draft {
    slug: String,
    title_he: String,
    description_he: String,
    body: String,
}

/// Start saving `claude#id`. `dir` is the project dir the instance runs in —
/// `--resume` finds a conversation by its cwd, so the forks must run there too.
/// Progress arrives as `save-progress` events; this returns once the save is
/// under way.
pub fn start(
    app: AppHandle,
    handle: ProjectHandle,
    id: usize,
    dir: PathBuf,
    uuid: String,
) -> Result<(), String> {
    {
        let mut running = RUNNING.lock().unwrap();
        if !running.get_or_insert_with(HashSet::new).insert((handle, id)) {
            return Err(format!("claude#{id} is already being saved"));
        }
    }
    std::thread::spawn(move || {
        let emit = |state: &str, detail: Option<String>| {
            let _ = app.emit(
                "save-progress",
                SaveProgress { handle, id, state: state.into(), detail },
            );
        };
        let result = save(&dir, &uuid, |stage| emit(stage, None));
        if let Some(running) = RUNNING.lock().unwrap().as_mut() {
            running.remove(&(handle, id));
        }
        match result {
            Ok(path) => emit("done", Some(path.display().to_string())),
            Err(reason) => {
                eprintln!("[saves] claude#{id} failed: {reason}");
                emit("error", Some(reason));
            }
        }
    });
    Ok(())
}

/// The three steps, then the file. Returns the path written.
fn save(dir: &Path, uuid: &str, stage: impl Fn(&str)) -> Result<PathBuf, String> {
    stage("writing");
    let draft = parse_draft(&run_step(dir, Some(uuid), WRITE_PROMPT)?)?;

    stage("checking");
    let check = format!("{CHECK_PROMPT}\n\n<handoff>\n{}\n</handoff>\n", draft.body);
    let gaps = run_step(dir, None, &check)?;
    let gaps = gaps.trim();

    let draft = if gaps == "NONE" {
        draft
    } else {
        stage("fixing");
        let draft_json = serde_json::json!({
            "slug": draft.slug,
            "title_he": draft.title_he,
            "description_he": draft.description_he,
            "body": draft.body,
        });
        let fix = format!(
            "{FIX_PROMPT}\n\n<draft>\n{draft_json}\n</draft>\n\n<reviewer_questions>\n{gaps}\n</reviewer_questions>\n"
        );
        parse_draft(&run_step(dir, Some(uuid), &fix)?)?
    };

    let root = repo_root(dir);
    let author = git_user_name(&root);
    write_save(&root.join(SAVES_DIR), &draft, &author, &today())
}

/// One headless step. `fork` = `Some(uuid)` resumes that conversation as a
/// throwaway fork; `None` is a blank claude. The prompt goes on stdin (closed
/// after, or `-p` waits for EOF forever). Env is scrubbed exactly like the
/// Explainer's child (`forwarded_env` drops `MULPEX_*` and
/// `CLAUDE_CODE_CHILD_SESSION`), and like it, no `--bare`: that can't see the
/// OAuth token.
fn run_step(dir: &Path, fork: Option<&str>, prompt: &str) -> Result<String, String> {
    let claude = claude_bin::resolve_claude().ok_or("claude not found")?;
    let mut args: Vec<&str> = vec![
        "-p",
        "--model",
        "opus",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--output-format",
        "json",
        "--tools",
        "Read,Grep,Glob,Bash",
        "--allowedTools",
        "Read,Grep,Glob,Bash(git log:*),Bash(git status:*),Bash(git diff:*),\
         Bash(git show:*),Bash(git branch:*)",
        "--disallowedTools",
        "Edit,Write,NotebookEdit",
    ];
    if let Some(uuid) = fork {
        args.extend(["--resume", uuid, "--fork-session"]);
    }
    let mut child = Command::new(claude)
        .args(&args)
        .env_clear()
        .envs(claude_bin::forwarded_env())
        .env("PATH", claude_bin::merged_path())
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }
    // Both pipes drained on their own threads — see explainer::run_summarizer.
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut f) = pipe {
                let _ = f.read_to_string(&mut s);
            }
            s
        })
    };
    let out_reader = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let err_reader = drain(child.stderr.take().map(|p| Box::new(p) as _));

    let deadline = std::time::Instant::now() + STEP_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timeout after {} min", STEP_TIMEOUT.as_secs() / 60));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(250)),
            Err(e) => return Err(format!("wait: {e}")),
        }
    };
    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    if !status.success() {
        return Err(crate::explainer::failure_reason(status.code(), &out, &err));
    }
    result_text(&out)
}

/// The `result` of `--output-format json`. A JSON body with `is_error` is a
/// failure the CLI itself reported (exit 0 is not proof of success).
fn result_text(out: &str) -> Result<String, String> {
    let v: serde_json::Value = serde_json::from_str(out.trim())
        .map_err(|_| format!("unreadable output: {}", first_chars(out, 200)))?;
    let text = v.get("result").and_then(|r| r.as_str()).unwrap_or("").to_string();
    if v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false) {
        return Err(first_chars(&text, 200));
    }
    if text.trim().is_empty() {
        return Err("empty output".into());
    }
    Ok(text)
}

/// The fork is told to answer with one JSON object and nothing else, but a
/// model may still wrap it in a fence or a sentence — take the outermost braces.
fn parse_draft(text: &str) -> Result<Draft, String> {
    let (start, end) = match (text.find('{'), text.rfind('}')) {
        (Some(s), Some(e)) if e > s => (s, e),
        _ => return Err(format!("no JSON in the answer: {}", first_chars(text, 200))),
    };
    let d: Draft = serde_json::from_str(&text[start..=end])
        .map_err(|e| format!("bad JSON in the answer: {e}"))?;
    if d.title_he.trim().is_empty() || d.body.trim().is_empty() {
        return Err("the answer has no title or no body".into());
    }
    Ok(d)
}

/// Write the doc under `saves_dir`, never overwriting: a taken slug gets `-2`,
/// `-3`… (updating an existing save is Phase 3's link, not a name collision).
fn write_save(saves_dir: &Path, d: &Draft, author: &str, date: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(saves_dir).map_err(|e| format!("create {}: {e}", saves_dir.display()))?;
    let slug = clean_slug(&d.slug);
    let mut n = 1;
    let path = loop {
        let name = if n == 1 { format!("{slug}.md") } else { format!("{slug}-{n}.md") };
        let p = saves_dir.join(name);
        if !p.exists() {
            break p;
        }
        n += 1;
    };
    std::fs::write(&path, render(d, author, date)).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// The file: a front-matter header, then the body. Header values are JSON
/// strings — valid YAML, and unambiguous for any text a model can produce
/// (colons, quotes, newlines).
fn render(d: &Draft, author: &str, date: &str) -> String {
    let q = |s: &str| serde_json::to_string(s.trim()).unwrap_or_default();
    format!(
        "---\ntitle: {}\ndescription: {}\nauthor: {}\ncreated: {date}\nupdated: {date}\n---\n\n{}\n",
        q(&d.title_he),
        q(&d.description_he),
        q(author),
        d.body.trim()
    )
}

/// Lowercase kebab-case, `[a-z0-9-]` only, ≤ 60 chars; `session` if nothing
/// survives. The slug is a file name a model chose, so it is never trusted as a
/// path (no `/`, no `..`).
fn clean_slug(raw: &str) -> String {
    let mut s = String::new();
    for c in raw.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.ends_with('-') {
            s.push('-');
        }
    }
    let s: String = s.trim_matches('-').chars().take(60).collect();
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() { "session".into() } else { s }
}

/// The git top-level of `dir`, so a project tab opened on a subfolder still
/// saves into the one `mulpex/saves/` of its repo. Not a repo → `dir` itself.
fn repo_root(dir: &Path) -> PathBuf {
    git(dir, &["rev-parse", "--show-toplevel"]).map(PathBuf::from).unwrap_or_else(|| dir.to_path_buf())
}

fn git_user_name(dir: &Path) -> String {
    git(dir, &["config", "user.name"]).unwrap_or_default()
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("PATH", claude_bin::merged_path())
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

/// Today's local date as `YYYY-MM-DD`, from `date` (the tree has no date crate,
/// and local — not UTC — is what a person reading "created" expects).
fn today() -> String {
    Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

fn first_chars(s: &str, n: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(n) {
        Some((cut, _)) => format!("{}…", &s[..cut]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(slug: &str) -> Draft {
        Draft {
            slug: slug.into(),
            title_he: "האצת דף היומן".into(),
            description_he: "כמעט גמור: \"בדיקה\" נשארה".into(),
            body: "## Goal\nx\n".into(),
        }
    }

    #[test]
    fn slug_is_cleaned_and_never_a_path() {
        assert_eq!(clean_slug("ShowLogs Performance"), "showlogs-performance");
        assert_eq!(clean_slug("../../etc/passwd"), "etc-passwd");
        assert_eq!(clean_slug("--a__b--"), "a-b");
        assert_eq!(clean_slug("שמירה"), "session");
        assert_eq!(clean_slug(&"a".repeat(80)).len(), 60);
    }

    #[test]
    fn parse_draft_takes_the_outer_object() {
        let raw = r###"Here: ```json
{"slug":"x","title_he":"כותרת","description_he":"d","body":"## Goal\n{nested}"}
```"###;
        let d = parse_draft(raw).unwrap();
        assert_eq!(d.slug, "x");
        assert_eq!(d.body, "## Goal\n{nested}");
        assert!(parse_draft("no json").is_err());
        assert!(parse_draft(r#"{"slug":"x","title_he":"","description_he":"","body":"b"}"#).is_err());
    }

    #[test]
    fn result_text_reports_cli_errors() {
        assert_eq!(result_text(r#"{"result":"hi","is_error":false}"#).unwrap(), "hi");
        assert_eq!(
            result_text(r#"{"result":"API Error: 401","is_error":true}"#).unwrap_err(),
            "API Error: 401"
        );
        assert!(result_text("not json").is_err());
        assert!(result_text(r#"{"result":"  "}"#).is_err());
    }

    #[test]
    fn header_quotes_hebrew_and_quotes() {
        let text = render(&draft("x"), "Gidi", "2026-09-23");
        assert!(text.starts_with("---\ntitle: \"האצת דף היומן\"\n"));
        assert!(text.contains("description: \"כמעט גמור: \\\"בדיקה\\\" נשארה\"\n"));
        assert!(text.contains("author: \"Gidi\"\ncreated: 2026-09-23\nupdated: 2026-09-23\n---\n\n## Goal\nx\n"));
    }

    /// The whole pipeline against a real conversation — three Opus calls, so
    /// manual only: `SAVE_TEST_DIR=<project dir> SAVE_TEST_UUID=<uuid> cargo test
    /// live_save -- --ignored --nocapture`. Writes into that repo's saves dir.
    #[test]
    #[ignore]
    fn live_save() {
        let dir = PathBuf::from(std::env::var("SAVE_TEST_DIR").unwrap());
        let uuid = std::env::var("SAVE_TEST_UUID").unwrap();
        let path = save(&dir, &uuid, |s| eprintln!("stage: {s}")).unwrap();
        eprintln!("wrote {}", path.display());
    }

    #[test]
    fn write_save_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("mulpex-saves-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = write_save(&dir, &draft("Same"), "a", "d").unwrap();
        let b = write_save(&dir, &draft("same"), "a", "d").unwrap();
        assert_eq!(a.file_name().unwrap(), "same.md");
        assert_eq!(b.file_name().unwrap(), "same-2.md");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
