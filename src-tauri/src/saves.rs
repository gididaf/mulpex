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

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::claude_bin;
use crate::snapshot::{ProjectHandle, SaveProgress};

const WRITE_PROMPT: &str = include_str!("save_prompts/write.md");
const CHECK_PROMPT: &str = include_str!("save_prompts/check.md");
const FIX_PROMPT: &str = include_str!("save_prompts/fix.md");

/// Appended to the write prompt when ⌘S lands on an instance that already has a
/// save (it was loaded from one, or saved before): one current doc replaces the
/// old one, so nothing still true is lost to a conversation that never saw it.
const UPDATE_NOTE: &str = "This work was saved before, and that doc is below. It may have \
been written from an earlier conversation, so it can hold things this conversation never saw. \
Produce ONE current doc that replaces it: keep everything from it that is still relevant, \
update what changed, and drop what is no longer true. Keep its `slug`.";

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
        // An instance that was loaded from a save, or saved before, updates
        // that file in place instead of starting a second one.
        let existing = save_for_uuid(&read_links(&links_file()), &uuid);
        let result = save(&dir, &uuid, existing.as_deref(), |stage| emit(stage, None));
        if let Ok(path) = &result {
            link("saved", &uuid, &dir, path);
        }
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
fn save(
    dir: &Path,
    uuid: &str,
    existing: Option<&Path>,
    stage: impl Fn(&str),
) -> Result<PathBuf, String> {
    stage("writing");
    let write = match existing.and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(doc) => format!("{WRITE_PROMPT}\n\n{UPDATE_NOTE}\n\n<current_doc>\n{doc}\n</current_doc>\n"),
        None => WRITE_PROMPT.to_string(),
    };
    let draft = parse_draft(&run_step(dir, Some(uuid), &write)?)?;

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
    // The latest saver is the author: the list shows who touched it last.
    let author = git_user_name(&root);
    match existing {
        Some(path) => update_save(path, &draft, &author, &today()),
        None => write_save(&root.join(SAVES_DIR), &draft, &author, &today()),
    }
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
    std::fs::write(&path, render(d, author, date, date)).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// Rewrite an existing save in place: same file, its `created` kept, `updated`
/// bumped. If the file vanished meanwhile (someone deleted it), it is recreated
/// at the same path rather than failing a three-minute save at the last step.
fn update_save(path: &Path, d: &Draft, author: &str, date: &str) -> Result<PathBuf, String> {
    let old = std::fs::read_to_string(path).unwrap_or_default();
    let created = entry_from(String::new(), &old).created;
    let created = if created.is_empty() { date.to_string() } else { created };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, render(d, author, &created, date)).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path.to_path_buf())
}

/// The file: a front-matter header, then the body. Header values are JSON
/// strings — valid YAML, and unambiguous for any text a model can produce
/// (colons, quotes, newlines).
fn render(d: &Draft, author: &str, created: &str, updated: &str) -> String {
    let q = |s: &str| serde_json::to_string(s.trim()).unwrap_or_default();
    format!(
        "---\ntitle: {}\ndescription: {}\nauthor: {}\ncreated: {created}\nupdated: {updated}\n---\n\n{}\n",
        q(&d.title_he),
        q(&d.description_he),
        q(author),
        d.body.trim()
    )
}

// ---- ⌘L Load: the list, delete, and the prompt a loaded claude starts on ----

/// One save as the ⌘L list shows it. `file` is the bare file name inside
/// `mulpex/saves/` — the only handle the frontend ever passes back.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct SaveEntry {
    pub file: String,
    pub title: String,
    pub description: String,
    pub author: String,
    pub created: String,
    pub updated: String,
    /// The conversation that last saved it still exists on THIS machine, in
    /// this project, so ⌘L can offer to continue it. Filled by `list_saves`.
    pub continuable: bool,
    /// That conversation is already open as this instance: "continue" focuses
    /// it instead of spawning a second claude on the same transcript.
    pub open_as: Option<usize>,
}

/// Every save of the repo `dir` belongs to, most recently updated first. A file
/// without a readable header still appears, titled by its file name: it is in
/// the folder, so hiding it would be the list lying about what's there.
pub fn list(dir: &Path) -> Vec<SaveEntry> {
    let saves_dir = repo_root(dir).join(SAVES_DIR);
    let Ok(rd) = std::fs::read_dir(&saves_dir) else {
        return Vec::new();
    };
    let mut out: Vec<SaveEntry> = rd
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_str()?.to_string();
            if !file.ends_with(".md") || !e.path().is_file() {
                return None;
            }
            let text = std::fs::read_to_string(e.path()).unwrap_or_default();
            Some(entry_from(file, &text))
        })
        .collect();
    out.sort_by(|a, b| {
        (&b.updated, &b.created).cmp(&(&a.updated, &a.created)).then_with(|| a.file.cmp(&b.file))
    });
    out
}

fn entry_from(file: String, text: &str) -> SaveEntry {
    let h = parse_header(text);
    let get = |k: &str| h.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).unwrap_or_default();
    let title = get("title");
    SaveEntry {
        title: if title.is_empty() { file.trim_end_matches(".md").to_string() } else { title },
        description: get("description"),
        author: get("author"),
        created: get("created"),
        updated: get("updated"),
        file,
        continuable: false,
        open_as: None,
    }
}

/// The `---` front-matter block as `(key, value)` pairs. Values `render` wrote
/// are JSON strings; a hand-edited plain value is taken as-is.
fn parse_header(text: &str) -> Vec<(String, String)> {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        let v = if v.starts_with('"') {
            serde_json::from_str::<String>(v).unwrap_or_else(|_| v.trim_matches('"').to_string())
        } else {
            v.to_string()
        };
        out.push((k.trim().to_string(), v));
    }
    out
}

/// The absolute path of save `file`, refusing anything that is not a plain
/// `.md` name inside the saves dir — `file` comes from the webview.
pub fn resolve(dir: &Path, file: &str) -> Result<PathBuf, String> {
    resolve_in(dir, SAVES_DIR, file)
}

fn resolve_in(dir: &Path, sub: &str, file: &str) -> Result<PathBuf, String> {
    if file.is_empty() || file.contains('/') || file.contains('\\') || file.starts_with('.') || !file.ends_with(".md") {
        return Err(format!("not a save: {file}"));
    }
    let path = repo_root(dir).join(sub).join(file);
    if !path.is_file() {
        return Err(format!("{file} is gone — someone may have deleted it"));
    }
    Ok(path)
}

pub fn delete(dir: &Path, file: &str) -> Result<(), String> {
    let path = resolve(dir, file)?;
    std::fs::remove_file(&path).map_err(|e| format!("delete {file}: {e}"))
}

/// The save's title, for naming the loaded row.
pub fn title_of(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let e = entry_from(String::new(), &text);
    (!e.title.is_empty()).then_some(e.title)
}

/// What a loaded claude starts on: read the doc, check the repo, report, and
/// wait for the user's go. The path is absolute because the instance's cwd is
/// the project dir, which may be a subfolder of the repo the save lives in.
pub fn load_prompt(path: &Path) -> String {
    format!(
        "Continue the work saved in {}. It is a handoff doc that Mulpex saved from an \
         earlier session; that conversation is not available. Read the whole doc first, \
         then check the repo's current state against it (git status, git log, the files it \
         names). Then reply briefly: where things stand, anything that changed since the \
         doc was written, and the next step you propose. Do not start the work yet — wait \
         for my go.",
        path.display()
    )
}

// ---- playbooks: pointers to recurring-incident runbooks ----
//
// A playbook is a runbook that is used again and again ("attach this when a
// client reports X"), so it is never "finished" and never moves. Mulpex knows
// it through a small committed pointer, `mulpex/playbooks/<slug>.md`, whose
// header carries the Hebrew `title` / `description` and `source` — the
// runbook's path relative to the repo root. The runbook itself stays where
// code and CLAUDE.md files already link to it.

pub const PLAYBOOKS_DIR: &str = "mulpex/playbooks";

/// One playbook as the ⌘L Playbooks tab shows it.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PlaybookEntry {
    pub file: String,
    pub title: String,
    pub description: String,
    /// The runbook, relative to the repo root.
    pub source: String,
    /// `source` is not on disk (moved or deleted without its pointer).
    pub missing: bool,
}

/// Every playbook pointer of the repo `dir` belongs to, by title.
pub fn list_playbooks(dir: &Path) -> Vec<PlaybookEntry> {
    let root = repo_root(dir);
    let Ok(rd) = std::fs::read_dir(root.join(PLAYBOOKS_DIR)) else {
        return Vec::new();
    };
    let mut out: Vec<PlaybookEntry> = rd
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_str()?.to_string();
            if !file.ends_with(".md") || !e.path().is_file() {
                return None;
            }
            let h = parse_header(&std::fs::read_to_string(e.path()).unwrap_or_default());
            let get = |k: &str| h.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).unwrap_or_default();
            let source = get("source");
            let title = get("title");
            Some(PlaybookEntry {
                missing: source_path(&root, &source).is_none_or(|p| !p.is_file()),
                title: if title.is_empty() { file.trim_end_matches(".md").to_string() } else { title },
                description: get("description"),
                source,
                file,
            })
        })
        .collect();
    out.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.file.cmp(&b.file)));
    out
}

/// `source` resolved under `root`, refusing absolute paths and `..` — it is
/// text in a committed file, so it is not trusted as a path.
fn source_path(root: &Path, source: &str) -> Option<PathBuf> {
    let rel = Path::new(source.trim());
    if source.trim().is_empty()
        || !rel.components().all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(root.join(rel))
}

/// Pointer path, runbook path and title of playbook `file`.
pub fn playbook(dir: &Path, file: &str) -> Result<(PathBuf, PathBuf, String), String> {
    let pointer = resolve_in(dir, PLAYBOOKS_DIR, file)?;
    let e = list_playbooks(dir).into_iter().find(|e| e.file == file).ok_or("playbook is gone")?;
    let source = source_path(&repo_root(dir), &e.source)
        .filter(|p| p.is_file())
        .ok_or_else(|| format!("its runbook {} is missing", e.source))?;
    Ok((pointer, source, e.title))
}

/// Retire a playbook for good: the runbook and its pointer. Git keeps the
/// history; Mulpex never commits the deletion.
pub fn delete_playbook(dir: &Path, file: &str) -> Result<(), String> {
    let pointer = resolve_in(dir, PLAYBOOKS_DIR, file)?;
    let h = parse_header(&std::fs::read_to_string(&pointer).unwrap_or_default());
    let source = h.iter().find(|(k, _)| k == "source").map(|(_, v)| v.clone()).unwrap_or_default();
    if let Some(src) = source_path(&repo_root(dir), &source).filter(|p| p.is_file()) {
        std::fs::remove_file(&src).map_err(|e| format!("delete {source}: {e}"))?;
    }
    std::fs::remove_file(&pointer).map_err(|e| format!("delete {file}: {e}"))
}

/// What a claude started from a playbook begins on: read the runbook, say in a
/// line or two what it is for, and ask what the user needs — then work it
/// read-only first, and at the end suggest (never make) an edit, or the
/// playbook's deletion if it describes something that is gone.
pub fn playbook_prompt(pointer: &Path, source: &Path) -> String {
    format!(
        "Read the playbook at {src} fully. Then tell me in one or two short lines what it is \
         for, and ask me what I need it for this time.\n\n\
         Once I tell you, stay read-only until you know what is going on, and ask me before \
         changing anything (data, production, code). When we are done: if you learned something the playbook lacks or has wrong, \
         suggest a concrete edit to it and ask before writing it. If the playbook describes \
         something that no longer exists, say so and suggest deleting it — both {src} and its \
         Mulpex pointer {ptr} — and ask before deleting.",
        src = source.display(),
        ptr = pointer.display(),
    )
}

// ---- local links: save file <-> conversation, never in the repo ----
//
// `<mulpex home>/save-links.tsv` (so a debug build uses `~/.mulpex-dev`), one
// line per event: `kind \t uuid \t project dir \t save path`, all paths
// canonical. `saved` = that conversation wrote the save; `loaded` = a fresh
// claude was started on it. A conversation id means nothing on a coworker's
// machine, which is why this is not in the doc's header. Append-only and read
// newest-last; a line whose save file is gone is simply skipped.

#[derive(Clone, Debug, PartialEq)]
struct Link {
    kind: String,
    uuid: String,
    dir: PathBuf,
    save: PathBuf,
}

fn links_file() -> PathBuf {
    mulpex_core::mulpex_home().join("save-links.tsv")
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Record that conversation `uuid` (running in `dir`) saved or loaded `save`.
pub fn link(kind: &str, uuid: &str, dir: &Path, save: &Path) {
    append_link(&links_file(), kind, uuid, dir, save);
}

fn append_link(file: &Path, kind: &str, uuid: &str, dir: &Path, save: &Path) {
    if uuid.is_empty() {
        return;
    }
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = format!("{kind}\t{uuid}\t{}\t{}\n", canon(dir).display(), canon(save).display());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn read_links(file: &Path) -> Vec<Link> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some(Link {
                kind: f.next()?.to_string(),
                uuid: f.next()?.to_string(),
                dir: PathBuf::from(f.next()?),
                save: PathBuf::from(f.next()?),
            })
        })
        .collect()
}

/// The save conversation `uuid` last saved or was loaded from, if that file
/// still exists — what a ⌘S on it updates.
fn save_for_uuid(links: &[Link], uuid: &str) -> Option<PathBuf> {
    links.iter().rev().find(|l| l.uuid == uuid && l.save.is_file()).map(|l| l.save.clone())
}

/// The conversation that most recently SAVED `save` from project `dir` and
/// still has its transcript here — the one "Continue conversation" resumes. A
/// merely `loaded` conversation doesn't count: it may never have saved, so the
/// doc can be newer than anything it holds.
fn resumable(links: &[Link], save: &Path, dir: &Path, exists: impl Fn(&str, &Path) -> bool) -> Option<String> {
    let (save, dir) = (canon(save), canon(dir));
    links
        .iter()
        .rev()
        .find(|l| l.kind == "saved" && l.save == save && l.dir == dir && exists(&l.uuid, &l.dir))
        .map(|l| l.uuid.clone())
}

/// Public face of [`resumable`] against the real links file and transcripts.
pub fn resumable_uuid(save: &Path, dir: &Path) -> Option<String> {
    resumable(&read_links(&links_file()), save, dir, |uuid, dir| transcript_path(dir, uuid).is_file())
}

/// Where Claude Code keeps conversation `uuid` of project `dir`: the canonical
/// dir with every non-alphanumeric byte turned into `-` (checked against the
/// real `~/.claude/projects/` names, e.g. `/private/tmp/...` → `-private-tmp-...`).
fn transcript_path(dir: &Path, uuid: &str) -> PathBuf {
    let slug: String = canon(dir)
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    claude_config_dir().join("projects").join(slug).join(format!("{uuid}.jsonl"))
}

fn claude_config_dir() -> PathBuf {
    claude_bin::forwarded_env()
        .into_iter()
        .find(|(k, v)| k == "CLAUDE_CONFIG_DIR" && !v.is_empty())
        .map(|(_, v)| PathBuf::from(v))
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude")
        })
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
        let text = render(&draft("x"), "Gidi", "2026-09-23", "2026-09-23");
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
        let path = save(&dir, &uuid, None, |s| eprintln!("stage: {s}")).unwrap();
        eprintln!("wrote {}", path.display());
    }

    #[test]
    fn header_round_trips_and_list_sorts_newest_first() {
        let dir = std::env::temp_dir().join(format!("mulpex-saves-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let saves = dir.join(SAVES_DIR);
        write_save(&saves, &draft("old"), "a", "2026-09-01").unwrap();
        write_save(&saves, &draft("new"), "b", "2026-09-20").unwrap();
        std::fs::write(saves.join("bare.md"), "no header here").unwrap();
        std::fs::write(saves.join("notes.txt"), "ignored").unwrap();
        let l = list(&dir);
        let files: Vec<_> = l.iter().map(|e| e.file.as_str()).collect();
        assert_eq!(files, ["new.md", "old.md", "bare.md"]);
        assert_eq!(l[0].title, "האצת דף היומן");
        assert_eq!(l[0].description, "כמעט גמור: \"בדיקה\" נשארה");
        assert_eq!(l[0].author, "b");
        assert_eq!(l[2].title, "bare");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_refuses_paths() {
        let dir = std::env::temp_dir().join(format!("mulpex-saves-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_save(&dir.join(SAVES_DIR), &draft("x"), "a", "d").unwrap();
        assert!(resolve(&dir, "x.md").is_ok());
        for bad in ["../x.md", "a/x.md", ".x.md", "x.txt", "", "missing.md"] {
            assert!(resolve(&dir, bad).is_err(), "{bad}");
        }
        delete(&dir, "x.md").unwrap();
        assert!(resolve(&dir, "x.md").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn links_pick_the_latest_live_saver() {
        let dir = std::env::temp_dir().join(format!("mulpex-saves-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let saves = dir.join(SAVES_DIR);
        let a = write_save(&saves, &draft("a"), "x", "d").unwrap();
        let b = write_save(&saves, &draft("b"), "x", "d").unwrap();
        let file = dir.join("links.tsv");
        append_link(&file, "saved", "u1", &dir, &a);
        append_link(&file, "loaded", "u2", &dir, &a);
        append_link(&file, "saved", "u3", &dir, &a);
        append_link(&file, "saved", "u4", &dir, &b);
        append_link(&file, "saved", "", &dir, &b); // ignored
        let links = read_links(&file);
        assert_eq!(links.len(), 4);

        // ⌘S target: whatever the conversation last saved or loaded.
        assert_eq!(save_for_uuid(&links, "u2"), Some(canon(&a)));
        assert_eq!(save_for_uuid(&links, "u4"), Some(canon(&b)));
        assert_eq!(save_for_uuid(&links, "nope"), None);

        // Continue: the latest SAVER with a transcript; a loader never counts.
        let all = |_: &str, _: &Path| true;
        assert_eq!(resumable(&links, &a, &dir, all).as_deref(), Some("u3"));
        let only_u1 = |u: &str, _: &Path| u == "u1";
        assert_eq!(resumable(&links, &a, &dir, only_u1).as_deref(), Some("u1"));
        let only_u2 = |u: &str, _: &Path| u == "u2";
        assert_eq!(resumable(&links, &a, &dir, only_u2), None);
        // Another project's conversation can't be resumed from here.
        assert_eq!(resumable(&links, &a, &saves, all), None);

        // A deleted save is no longer anyone's ⌘S target.
        std::fs::remove_file(&b).unwrap();
        assert_eq!(save_for_uuid(&links, "u4"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_keeps_created_and_the_file() {
        let dir = std::env::temp_dir().join(format!("mulpex-saves-upd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = write_save(&dir, &draft("x"), "first", "2026-09-01").unwrap();
        let mut d = draft("renamed-by-model");
        d.body = "## Goal\nnew\n".into();
        let q = update_save(&p, &d, "second", "2026-09-23").unwrap();
        assert_eq!(p, q);
        let e = entry_from(String::new(), &std::fs::read_to_string(&p).unwrap());
        assert_eq!((e.created.as_str(), e.updated.as_str(), e.author.as_str()), ("2026-09-01", "2026-09-23", "second"));
        assert!(std::fs::read_to_string(&p).unwrap().ends_with("## Goal\nnew\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcript_path_matches_claude_codes_layout() {
        let p = transcript_path(Path::new("/nonexistent/a b/c.d"), "u");
        assert!(p.ends_with("projects/-nonexistent-a-b-c-d/u.jsonl"), "{}", p.display());
    }

    #[test]
    fn playbooks_list_resolve_and_retire() {
        let dir = std::env::temp_dir().join(format!("mulpex-playbooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("docs/runbooks")).unwrap();
        std::fs::create_dir_all(dir.join(PLAYBOOKS_DIR)).unwrap();
        std::fs::write(dir.join("docs/runbooks/mfa.md"), "# MFA").unwrap();
        let ptr = |title: &str, src: &str| format!("---\ntitle: \"{title}\"\ndescription: \"d\"\nsource: \"{src}\"\n---\n");
        std::fs::write(dir.join(PLAYBOOKS_DIR).join("mfa.md"), ptr("ביטול MFA", "docs/runbooks/mfa.md")).unwrap();
        std::fs::write(dir.join(PLAYBOOKS_DIR).join("gone.md"), ptr("אבד", "docs/runbooks/gone.md")).unwrap();
        std::fs::write(dir.join(PLAYBOOKS_DIR).join("evil.md"), ptr("רע", "../../etc/passwd")).unwrap();

        let l = list_playbooks(&dir);
        let by = |f: &str| l.iter().find(|e| e.file == f).unwrap().clone();
        assert!(!by("mfa.md").missing);
        assert!(by("gone.md").missing);
        assert!(by("evil.md").missing, "a `..` source must never resolve");

        let (p, s, t) = playbook(&dir, "mfa.md").unwrap();
        assert!(p.ends_with("mulpex/playbooks/mfa.md") && s.ends_with("docs/runbooks/mfa.md"));
        assert_eq!(t, "ביטול MFA");
        assert!(playbook(&dir, "gone.md").is_err());
        assert!(playbook(&dir, "evil.md").is_err());

        delete_playbook(&dir, "mfa.md").unwrap();
        assert!(!dir.join("docs/runbooks/mfa.md").exists());
        assert!(!dir.join(PLAYBOOKS_DIR).join("mfa.md").exists());
        let _ = std::fs::remove_dir_all(&dir);
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
