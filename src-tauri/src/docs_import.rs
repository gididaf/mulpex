//! Import Docs (File ▸ Import Docs…): turn a repo's existing markdown
//! runbooks into what ⌘L shows, with a Hebrew title and description each.
//!
//! 1. **Scan** every committed `.md`, minus READMEs and friends, dot-folders,
//!    `node_modules/`, Mulpex's own `mulpex/`, runbooks that already have a
//!    guide pointer, and whatever `mulpex/import-skip.txt` lists.
//! 2. **Sort** each file with a headless Sonnet (6 at a time, read-only tools
//!    so it can check whether what a file describes still exists) into one of
//!    `save` / `guide` / `stale` / `skip`, with a Hebrew title/description.
//! 3. **Review**: the user ticks, flips kinds and edits the Hebrew.
//! 4. **Apply**: a save moves into `mulpex/saves/` (original body, Hebrew
//!    header, dates and author from git; the old file is deleted); a guide
//!    gets a pointer in `mulpex/guides/` and stays where it is; a stale file
//!    is deleted; a skip is recorded in `mulpex/import-skip.txt` so no later
//!    import asks again. Mulpex never commits any of it.
//!
//! The job lives here, per project, not in the dialog: sorting ~30 files takes
//! minutes, and the user may close the window and reopen it to see progress.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::saves;
use crate::snapshot::ProjectHandle;

const SORT_PROMPT: &str = include_str!("save_prompts/sort.md");
const SORT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const WORKERS: usize = 6;
/// A runbook longer than this is cut for the sorter (the head says what it is).
const MAX_SORT_BYTES: usize = 60_000;
pub const SKIP_FILE: &str = "mulpex/import-skip.txt";

/// Base names that are never runbooks.
const NEVER: &[&str] = &["readme.md", "changelog.md", "claude.md", "agents.md", "license.md", "contributing.md"];

/// One file of an import, as the review list shows it.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Item {
    /// Repo-relative path.
    pub file: String,
    /// `pending` while sorting, then `done` or `error`.
    pub status: String,
    pub kind: String,
    pub slug: String,
    pub title: String,
    pub description: String,
    pub reason: String,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportState {
    pub running: bool,
    pub items: Vec<Item>,
}

struct Job {
    /// Bumped on discard, so a worker finishing after it drops its result.
    generation: u64,
    items: Vec<Item>,
}

static JOBS: Mutex<Option<HashMap<ProjectHandle, Job>>> = Mutex::new(None);
static GENERATION: Mutex<u64> = Mutex::new(0);

fn with_jobs<T>(f: impl FnOnce(&mut HashMap<ProjectHandle, Job>) -> T) -> T {
    f(JOBS.lock().unwrap().get_or_insert_with(HashMap::new))
}

fn state_of(job: &Job) -> ImportState {
    ImportState {
        running: job.items.iter().any(|i| i.status == "pending"),
        items: job.items.clone(),
    }
}

/// The project's import, if one exists (running, or finished and not yet applied
/// or discarded).
pub fn state(handle: ProjectHandle) -> Option<ImportState> {
    with_jobs(|j| j.get(&handle).map(state_of))
}

/// Start an import for the repo of `dir`, or return the one already there.
pub fn start(app: AppHandle, handle: ProjectHandle, dir: &Path) -> ImportState {
    if let Some(s) = state(handle) {
        return s;
    }
    let root = saves::repo_root(dir);
    let files = scan(&root);
    let generation = {
        let mut g = GENERATION.lock().unwrap();
        *g += 1;
        *g
    };
    let items: Vec<Item> = files
        .iter()
        .map(|f| Item {
            file: f.clone(),
            status: "pending".into(),
            kind: String::new(),
            slug: String::new(),
            title: String::new(),
            description: String::new(),
            reason: String::new(),
            error: None,
        })
        .collect();
    let st = with_jobs(|j| {
        j.insert(handle, Job { generation, items });
        state_of(&j[&handle])
    });

    let queue = Arc::new(Mutex::new(files));
    for _ in 0..WORKERS {
        let (queue, root, app) = (queue.clone(), root.clone(), app.clone());
        std::thread::spawn(move || loop {
            let Some(file) = queue.lock().unwrap().pop() else { return };
            let result = sort(&root, &file);
            let stored = with_jobs(|j| {
                let Some(job) = j.get_mut(&handle).filter(|job| job.generation == generation) else {
                    return false;
                };
                if let Some(item) = job.items.iter_mut().find(|i| i.file == file) {
                    match result {
                        Ok(s) => {
                            item.status = "done".into();
                            item.kind = s.kind;
                            item.slug = s.slug;
                            item.title = s.title_he;
                            item.description = s.description_he;
                            item.reason = s.reason;
                        }
                        Err(e) => {
                            item.status = "error".into();
                            item.error = Some(e);
                        }
                    }
                }
                true
            });
            if stored {
                let _ = app.emit("import-update", handle);
            }
        });
    }
    st
}

/// Throw the project's import away (running workers' results are dropped).
pub fn discard(handle: ProjectHandle) {
    with_jobs(|j| j.remove(&handle));
}

/// Every committed markdown file of `root` that could be a runbook and has not
/// been handled by an earlier import.
fn scan(root: &Path) -> Vec<String> {
    let listed = saves::git(root, &["ls-files", "-z", "--", "*.md"]).unwrap_or_default();
    let skip: Vec<String> = std::fs::read_to_string(root.join(SKIP_FILE))
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let pointed: Vec<String> = saves::list_guides(root).into_iter().map(|p| p.source).collect();
    let mut out: Vec<String> = listed
        .split('\0')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .filter(|f| eligible(f))
        .filter(|f| !skip.iter().any(|s| s == f) && !pointed.iter().any(|p| p == f))
        .filter(|f| root.join(f).is_file())
        .map(str::to_string)
        .collect();
    out.sort();
    out
}

fn eligible(file: &str) -> bool {
    let parts: Vec<&str> = file.split('/').collect();
    let base = parts.last().map(|b| b.to_lowercase()).unwrap_or_default();
    !NEVER.contains(&base.as_str())
        && !parts.iter().any(|p| p.starts_with('.') || *p == "node_modules")
        && parts.first() != Some(&"mulpex")
}

#[derive(Debug, Deserialize, PartialEq)]
struct Sorted {
    kind: String,
    slug: String,
    title_he: String,
    description_he: String,
    #[serde(default)]
    reason: String,
}

fn sort(root: &Path, file: &str) -> Result<Sorted, String> {
    let text = std::fs::read_to_string(root.join(file)).map_err(|e| format!("read: {e}"))?;
    let mut cut = text.len().min(MAX_SORT_BYTES);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let prompt = format!("{SORT_PROMPT}\n\n<file path=\"{file}\">\n{}\n</file>\n", &text[..cut]);
    let out = saves::run_claude(root, "sonnet", None, &prompt, SORT_TIMEOUT)?;
    parse_sorted(&out)
}

fn parse_sorted(text: &str) -> Result<Sorted, String> {
    let (start, end) = match (text.find('{'), text.rfind('}')) {
        (Some(s), Some(e)) if e > s => (s, e),
        _ => return Err(format!("no JSON in the answer: {}", saves::first_chars(text, 200))),
    };
    let s: Sorted =
        serde_json::from_str(&text[start..=end]).map_err(|e| format!("bad JSON in the answer: {e}"))?;
    if !matches!(s.kind.as_str(), "save" | "guide" | "stale" | "skip") {
        return Err(format!("unknown kind {:?}", s.kind));
    }
    Ok(s)
}

/// What the user decided for one file in the review.
#[derive(Clone, Debug, Deserialize)]
pub struct Decision {
    pub file: String,
    pub kind: String,
    pub slug: String,
    pub title: String,
    pub description: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ApplyReport {
    pub saves: usize,
    pub guides: usize,
    pub deleted: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
    /// `<short hash> <subject>` of the commit Apply made, if it made one.
    pub commit: Option<String>,
    /// Why the commit was not made (a hook failed, git refused) — the files
    /// are changed either way.
    pub commit_error: Option<String>,
    /// Places that still mention a deleted doc and were not safe to edit
    /// (code comments, prose outside a link) — for the user to look at.
    pub leftovers: Vec<String>,
}

/// Carry out the ticked decisions, then drop the job. One failed file doesn't
/// stop the rest; its reason is in the report.
pub fn apply(handle: ProjectHandle, dir: &Path, decisions: &[Decision]) -> ApplyReport {
    let root = saves::repo_root(dir);
    let mut report = ApplyReport::default();
    let mut touched: Vec<PathBuf> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    // (old, new) repo-relative paths of moved saves; deleted stale docs.
    let mut moved: Vec<(String, String)> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    for d in decisions {
        match apply_one(&root, d) {
            Err(e) => report.errors.push(format!("{}: {e}", d.file)),
            Ok((paths, line)) => {
                match d.kind.as_str() {
                    "save" => {
                        report.saves += 1;
                        if let Some(new) = paths.get(1) {
                            moved.push((d.file.clone(), rel_to(&root, new)));
                        }
                    }
                    "guide" => report.guides += 1,
                    "stale" => {
                        report.deleted += 1;
                        deleted.push(d.file.clone());
                    }
                    _ => report.skipped += 1,
                }
                touched.extend(paths);
                lines.push(line);
            }
        }
    }
    discard(handle);
    let fixed = fix_references(&root, &moved, &deleted);
    touched.extend(fixed.edited);
    lines.extend(fixed.lines);
    report.leftovers = fixed.leftovers;
    if !touched.is_empty() {
        let subject = format!(
            "docs: import into Mulpex ({} saves, {} guides, {} deleted)",
            report.saves, report.guides, report.deleted
        );
        match commit(&root, &touched, &subject, &lines) {
            Ok(c) => report.commit = Some(c),
            Err(e) => report.commit_error = Some(e),
        }
    }
    report
}

/// One commit holding exactly what the import touched — so the whole
/// conversion can be reviewed or reverted as one — and nothing else: `--only`
/// leaves anything else the user (or another instance sharing this tree) has
/// staged out of it. New files have to be added first (`--only` can't name an
/// untracked path). Hooks run like any commit; a failing one leaves the files
/// changed and the commit unmade, and the report says why.
fn commit(root: &Path, touched: &[PathBuf], subject: &str, lines: &[String]) -> Result<String, String> {
    let rels: Vec<String> = touched
        .iter()
        .filter_map(|p| p.strip_prefix(root).ok().map(|r| r.to_string_lossy().into_owned()))
        .collect();
    let existing: Vec<&str> = rels.iter().filter(|r| root.join(r).exists()).map(String::as_str).collect();
    if !existing.is_empty() {
        let mut args = vec!["add", "--"];
        args.extend(existing);
        git_run(root, &args)?;
    }
    let message = format!("{subject}\n\n{}\n", lines.join("\n"));
    let mut args = vec!["commit", "--only", "-m", message.as_str(), "--"];
    args.extend(rels.iter().map(String::as_str));
    git_run(root, &args)?;
    git_run(root, &["log", "-1", "--format=%h %s"]).map(|s| s.trim().to_string())
}

/// `git` in `root`, with the reason (stderr, else stdout) on failure — a hook's
/// complaint is on whichever stream it chose.
fn git_run(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .env("PATH", crate::claude_bin::merged_path())
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        return Ok(stdout);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let reason = [err.trim(), stdout.trim()].into_iter().find(|s| !s.is_empty()).unwrap_or("failed");
    Err(format!("git {}: {}", args[0], saves::first_chars(reason, 400)))
}

/// Apply one decision. Returns the paths it created, changed or deleted (for
/// the commit) and one line for the commit body.
fn apply_one(root: &Path, d: &Decision) -> Result<(Vec<PathBuf>, String), String> {
    // `file` comes from the webview: only a normal repo-relative path.
    let rel = Path::new(&d.file);
    if d.file.is_empty() || !rel.components().all(|c| matches!(c, std::path::Component::Normal(_))) {
        return Err("not a repo path".into());
    }
    let src = root.join(rel);
    match d.kind.as_str() {
        "save" => {
            let body = std::fs::read_to_string(&src).map_err(|e| format!("read: {e}"))?;
            let draft = saves::Draft {
                slug: d.slug.clone(),
                title_he: d.title.clone(),
                description_he: d.description.clone(),
                body,
            };
            let (created, updated, author) = git_dates(root, &d.file);
            let saved = saves::write_save_dated(&root.join(saves::SAVES_DIR), &draft, &author, &created, &updated)?;
            std::fs::remove_file(&src).map_err(|e| format!("delete original: {e}"))?;
            let line = format!("- save: {} -> {}", d.file, rel_to(root, &saved));
            Ok((vec![src, saved], line))
        }
        "guide" => {
            if !src.is_file() {
                return Err("file is gone".into());
            }
            let pointer = write_pointer(&root.join(saves::GUIDES_DIR), d)?;
            let line = format!("- guide: {} (pointer {})", d.file, rel_to(root, &pointer));
            Ok((vec![pointer], line))
        }
        "stale" => {
            std::fs::remove_file(&src).map_err(|e| format!("delete: {e}"))?;
            Ok((vec![src], format!("- stale, deleted: {}", d.file)))
        }
        "skip" => {
            let path = root.join(SKIP_FILE);
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            let mut text = std::fs::read_to_string(&path).unwrap_or_else(|_| {
                "# Files Mulpex's Import Docs should not ask about again (repo-relative).\n".into()
            });
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&d.file);
            text.push('\n');
            std::fs::write(&path, text).map_err(|e| e.to_string())?;
            Ok((vec![path], format!("- skip: {}", d.file)))
        }
        k => Err(format!("unknown kind {k:?}")),
    }
}

/// `created` = first commit, `updated` = last commit, `author` = last
/// committer's name — what git knows about the runbook, so its age stays honest.
fn git_dates(root: &Path, file: &str) -> (String, String, String) {
    let last = |fmt: &str| saves::git(root, &["log", "-1", &format!("--format={fmt}"), "--", file]).unwrap_or_default();
    let created = saves::git(root, &["log", "--diff-filter=A", "--format=%cs", "--", file])
        .and_then(|s| s.lines().last().map(str::to_string))
        .unwrap_or_default();
    let updated = last("%cs");
    let created = if created.is_empty() { updated.clone() } else { created };
    (created, updated, last("%an"))
}

/// `mulpex/guides/<slug>.md`: only a header pointing at the runbook.
// ---- keeping links whole: what pointed at a moved or deleted doc ----

#[derive(Default)]
struct Fixed {
    edited: Vec<PathBuf>,
    lines: Vec<String>,
    leftovers: Vec<String>,
}

/// After saves moved and stale docs were deleted, fix what pointed at them in
/// the repo's other tracked files:
/// - a markdown link to a **moved** doc is re-pointed (relative to the linking
///   file), and a plain repo-relative mention of it is rewritten;
/// - a markdown link to a **deleted** doc drops its line when that line is a
///   table row or list item (an index entry for a doc that no longer exists),
///   and otherwise becomes plain text;
/// - anything else still naming a deleted doc (a code comment, prose) is left
///   alone and reported, because guessing there does more harm than a note.
fn fix_references(root: &Path, moved: &[(String, String)], deleted: &[String]) -> Fixed {
    let mut fixed = Fixed::default();
    let names: Vec<&str> = moved
        .iter()
        .map(|(o, _)| o.as_str())
        .chain(deleted.iter().map(String::as_str))
        .filter_map(|p| p.rsplit('/').next())
        .collect();
    if names.is_empty() {
        return fixed;
    }
    let mut args = vec!["grep", "-l", "-z", "-F"];
    for n in &names {
        args.extend(["-e", n]);
    }
    let candidates = saves::git(root, &args).unwrap_or_default();
    for file in candidates.split('\0').map(str::trim).filter(|f| !f.is_empty()) {
        let path = root.join(file);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let (new_text, notes) = fix_text(file, &text, moved, deleted);
        if new_text != text && std::fs::write(&path, &new_text).is_ok() {
            fixed.edited.push(path);
            fixed.lines.push(format!("- fixed links in {file}"));
        }
        fixed.leftovers.extend(notes);
    }
    fixed
}

/// [`fix_references`] for one file's text. Pure, so it is tested directly.
fn fix_text(file: &str, text: &str, moved: &[(String, String)], deleted: &[String]) -> (String, Vec<String>) {
    let dir = Path::new(file).parent().unwrap_or(Path::new(""));
    let md = file.ends_with(".md");
    let mut out: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let mut line = line.to_string();
        if md {
            let (l, dropped) = fix_links(&line, dir, moved, deleted);
            if dropped {
                continue;
            }
            line = l;
        }
        for (old, new) in moved {
            if line.contains(old.as_str()) {
                line = line.replace(old.as_str(), new);
            }
        }
        for gone in deleted {
            let base = gone.rsplit('/').next().unwrap_or(gone);
            if line.contains(gone.as_str()) || line.contains(base) {
                notes.push(format!("{file}:{} still mentions deleted {gone}", i + 1));
            }
        }
        out.push(line);
    }
    (out.concat(), notes)
}

/// Rewrite the markdown links on one line. Returns the line and whether it
/// should be dropped (a table row / list item linking a deleted doc).
fn fix_links(line: &str, dir: &Path, moved: &[(String, String)], deleted: &[String]) -> (String, bool) {
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = rest.find("](") {
        let Some(close) = rest[at + 2..].find(')') else { break };
        let target = &rest[at + 2..at + 2 + close];
        let (path_part, anchor) = match target.find('#') {
            Some(h) => (&target[..h], &target[h..]),
            None => (target, ""),
        };
        let resolved = resolve_link(dir, path_part);
        let head = &rest[..at];
        let tail_from = at + 2 + close + 1;
        if let Some((_, new)) = resolved.as_ref().and_then(|r| moved.iter().find(|(o, _)| o == r)) {
            out.push_str(head);
            out.push_str("](");
            out.push_str(&relative(dir, Path::new(new)));
            out.push_str(anchor);
            out.push(')');
        } else if resolved.as_ref().is_some_and(|r| deleted.contains(r)) {
            let t = line.trim_start();
            let listy = t.starts_with('|')
                || t.starts_with("- ")
                || t.starts_with("* ")
                || t.split_once(". ").is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
            if listy {
                return (line.to_string(), true);
            }
            // Unlink: keep `[text]` as `text`.
            match head.rfind('[') {
                Some(open) => {
                    out.push_str(&head[..open]);
                    out.push_str(&head[open + 1..]);
                }
                None => {
                    out.push_str(head);
                    out.push_str("](");
                    out.push_str(target);
                    out.push(')');
                }
            }
        } else {
            out.push_str(&rest[..tail_from]);
        }
        rest = &rest[tail_from..];
    }
    out.push_str(rest);
    (out, false)
}

/// A link target as a repo-relative path: `/x` from the root, anything else
/// from the linking file's dir; `..` and `.` folded. None for URLs or a path
/// that climbs out of the repo.
fn resolve_link(dir: &Path, target: &str) -> Option<String> {
    let t = target.trim();
    if t.is_empty() || t.contains("://") || t.starts_with("mailto:") {
        return None;
    }
    let joined = match t.strip_prefix('/') {
        Some(abs) => PathBuf::from(abs),
        None => dir.join(t),
    };
    let mut parts: Vec<String> = Vec::new();
    for c in joined.components() {
        match c {
            std::path::Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            _ => {}
        }
    }
    Some(parts.join("/"))
}

/// `to` (repo-relative) as seen from `from_dir` (repo-relative).
fn relative(from_dir: &Path, to: &Path) -> String {
    let from: Vec<_> = from_dir.components().collect();
    let to_c: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to_c).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_string(); from.len() - common];
    parts.extend(to_c[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
    parts.join("/")
}

fn rel_to(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().into_owned()
}

fn write_pointer(dir: &Path, d: &Decision) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let slug = saves::clean_slug(&d.slug);
    let mut n = 1;
    let path: PathBuf = loop {
        let name = if n == 1 { format!("{slug}.md") } else { format!("{slug}-{n}.md") };
        let p = dir.join(name);
        if !p.exists() {
            break p;
        }
        n += 1;
    };
    let q = |s: &str| serde_json::to_string(s.trim()).unwrap_or_default();
    let text = format!(
        "---\ntitle: {}\ndescription: {}\nsource: {}\n---\n",
        q(&d.title),
        q(&d.description),
        q(&d.file)
    );
    std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real Sonnet calls: `SORT_TEST_ROOT=<repo> SORT_TEST_FILES=a.md,b.md cargo
    /// test live_sort -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_sort() {
        let root = PathBuf::from(std::env::var("SORT_TEST_ROOT").unwrap());
        let files = std::env::var("SORT_TEST_FILES").unwrap();
        let hs: Vec<_> = files
            .split(',')
            .map(|f| {
                let (root, f) = (root.clone(), f.to_string());
                std::thread::spawn(move || (f.clone(), sort(&root, &f)))
            })
            .collect();
        for h in hs {
            let (f, r) = h.join().unwrap();
            eprintln!("{f}: {r:?}");
        }
    }

    #[test]
    fn links_follow_moves_and_deletions() {
        let moved = vec![("docs/runbooks/wip.md".to_string(), "mulpex/saves/wip-x.md".to_string())];
        let deleted = vec!["docs/runbooks/old.md".to_string()];
        let text = "\
| Runbook | When |
|---|---|
| [`wip.md`](runbooks/wip.md) | still going |
| [`old.md`](runbooks/old.md) | gone |
- [old one](./runbooks/old.md#top)
See [the old doc](runbooks/old.md) for history, and [wip](runbooks/wip.md#left).
Full path: docs/runbooks/wip.md
Unrelated [link](https://x.y/old.md) and [other](runbooks/keep.md).
";
        let (out, notes) = fix_text("docs/index.md", text, &moved, &deleted);
        assert_eq!(
            out,
            "\
| Runbook | When |
|---|---|
| [`wip.md`](../mulpex/saves/wip-x.md) | still going |
See the old doc for history, and [wip](../mulpex/saves/wip-x.md#left).
Full path: mulpex/saves/wip-x.md
Unrelated [link](https://x.y/old.md) and [other](runbooks/keep.md).
"
        );
        // Unlinking left the prose with no file name, so nothing to report; the
        // URL still carries the base name and is reported rather than guessed at.
        assert_eq!(notes, vec!["docs/index.md:8 still mentions deleted docs/runbooks/old.md"]);

        // Code: plain paths follow a move; a deleted doc is only reported.
        let (code, notes) = fix_text(
            "backend/x.ts",
            "// see docs/runbooks/wip.md\n// see docs/runbooks/old.md\n",
            &moved,
            &deleted,
        );
        assert_eq!(code, "// see mulpex/saves/wip-x.md\n// see docs/runbooks/old.md\n");
        assert_eq!(notes, vec!["backend/x.ts:2 still mentions deleted docs/runbooks/old.md"]);
    }

    /// `FIX_TEST_FILE=<copy of a repo file> FIX_TEST_AS=<its repo path>
    /// FIX_TEST_MOVED=old=>new,… FIX_TEST_DELETED=a,b cargo test live_fix -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_fix() {
        let text = std::fs::read_to_string(std::env::var("FIX_TEST_FILE").unwrap()).unwrap();
        let moved: Vec<(String, String)> = std::env::var("FIX_TEST_MOVED")
            .unwrap_or_default()
            .split(',')
            .filter_map(|m| m.split_once("=>").map(|(a, b)| (a.into(), b.into())))
            .collect();
        let deleted: Vec<String> =
            std::env::var("FIX_TEST_DELETED").unwrap_or_default().split(',').filter(|s| !s.is_empty()).map(Into::into).collect();
        let (out, notes) = fix_text(&std::env::var("FIX_TEST_AS").unwrap(), &text, &moved, &deleted);
        for (a, b) in text.lines().zip(out.lines()).filter(|(a, b)| a != b) {
            eprintln!("- {a}\n+ {b}");
        }
        eprintln!("lines {} -> {}; notes {notes:?}", text.lines().count(), out.lines().count());
    }

    #[test]
    fn link_paths() {
        assert_eq!(resolve_link(Path::new("docs"), "runbooks/a.md").as_deref(), Some("docs/runbooks/a.md"));
        assert_eq!(resolve_link(Path::new("docs/x"), "../../TODO.md").as_deref(), Some("TODO.md"));
        assert_eq!(resolve_link(Path::new("docs"), "/TODO.md").as_deref(), Some("TODO.md"));
        assert_eq!(resolve_link(Path::new(""), "../out.md"), None);
        assert_eq!(resolve_link(Path::new("docs"), "https://a/b.md"), None);
        assert_eq!(relative(Path::new("docs"), Path::new("mulpex/saves/a.md")), "../mulpex/saves/a.md");
        assert_eq!(relative(Path::new(""), Path::new("mulpex/saves/a.md")), "mulpex/saves/a.md");
        assert_eq!(relative(Path::new("mulpex/saves"), Path::new("mulpex/saves/a.md")), "a.md");
    }

    #[test]
    fn eligibility() {
        assert!(eligible("docs/runbooks/mfa.md"));
        assert!(eligible("TODO.md"));
        for f in ["README.md", "backend/CLAUDE.md", ".claude/skills/x/skill.md", "a/node_modules/b.md", "mulpex/saves/x.md", "x/.github/y.md"] {
            assert!(!eligible(f), "{f}");
        }
    }

    #[test]
    fn parse_sorted_checks_kind() {
        let ok = r#"{"kind":"guide","slug":"mfa","title_he":"ביטול MFA","description_he":"d","reason":"r"}"#;
        assert_eq!(parse_sorted(ok).unwrap().kind, "guide");
        assert!(parse_sorted(&ok.replace("guide", "maybe")).is_err());
        assert!(parse_sorted("nope").is_err());
    }

    #[test]
    fn apply_moves_points_deletes_and_remembers() {
        let root = std::env::temp_dir().join(format!("mulpex-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("docs/runbooks")).unwrap();
        for f in ["wip", "guide", "old", "notes"] {
            std::fs::write(root.join(format!("docs/runbooks/{f}.md")), format!("# {f}\n")).unwrap();
        }
        // A real repo with committed runbooks, plus someone else's staged work
        // that the import's commit must leave alone.
        let g = |args: &[&str]| {
            let ok = std::process::Command::new("git").args(args).current_dir(&root).output().unwrap();
            assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        g(&["init", "-q"]);
        g(&["-c", "user.name=t", "-c", "user.email=t@t", "add", "."]);
        g(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]);
        std::fs::write(root.join("other.txt"), "someone else's work").unwrap();
        g(&["add", "other.txt"]);
        let d = |file: &str, kind: &str| Decision {
            file: format!("docs/runbooks/{file}.md"),
            kind: kind.into(),
            slug: file.into(),
            title: format!("כותרת {file}"),
            description: "תיאור".into(),
        };
        let r = apply(9999, &root, &[d("wip", "save"), d("guide", "guide"), d("old", "stale"), d("notes", "skip"), Decision { file: "../etc/x.md".into(), ..d("x", "stale") }]);
        assert_eq!((r.saves, r.guides, r.deleted, r.skipped, r.errors.len()), (1, 1, 1, 1, 1));
        // One commit with exactly the import's files; the staged stranger stays staged.
        eprintln!("commit: {:?} / {:?}", r.commit, r.commit_error);
        let root = std::fs::canonicalize(&root).unwrap();
        let show = std::process::Command::new("git")
            .args(["show", "--name-status", "--format=%s", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let show = String::from_utf8_lossy(&show.stdout);
        if r.commit.is_some() {
            assert!(show.starts_with("docs: import into Mulpex (1 saves, 1 guides, 1 deleted)"), "{show}");
            for want in ["D\tdocs/runbooks/wip.md", "A\tmulpex/saves/wip.md", "A\tmulpex/guides/guide.md", "D\tdocs/runbooks/old.md", "A\tmulpex/import-skip.txt"] {
                assert!(show.contains(want), "missing {want:?} in:\n{show}");
            }
            assert!(!show.contains("other.txt"), "{show}");
            let staged = std::process::Command::new("git").args(["diff", "--cached", "--name-only"]).current_dir(&root).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&staged.stdout).trim(), "other.txt");
        } else {
            // No git identity on this machine: the commit is refused, and says so.
            assert!(r.commit_error.is_some());
        }

        // save: moved, body kept, Hebrew header added.
        assert!(!root.join("docs/runbooks/wip.md").exists());
        let saved = std::fs::read_to_string(root.join("mulpex/saves/wip.md")).unwrap();
        assert!(saved.starts_with("---\ntitle: \"כותרת wip\"\n") && saved.ends_with("# wip\n"));
        // guide: stays, pointer points at it and lists.
        assert!(root.join("docs/runbooks/guide.md").exists());
        let pbs = saves::list_guides(&root);
        assert_eq!(pbs.len(), 1);
        assert_eq!(pbs[0].source, "docs/runbooks/guide.md");
        assert!(!pbs[0].missing);
        // stale: gone. skip: remembered.
        assert!(!root.join("docs/runbooks/old.md").exists());
        let skip = std::fs::read_to_string(root.join(SKIP_FILE)).unwrap();
        assert!(skip.lines().any(|l| l == "docs/runbooks/notes.md"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
