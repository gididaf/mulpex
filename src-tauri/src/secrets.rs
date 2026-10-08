//! ⌘K Secrets: hand a claude a password without it entering the transcript.
//!
//! The user types KEY=VALUE rows into a dialog. Mulpex writes them to a 0600
//! `.env` file and types only a `🔑 KEY` tag into the claude's prompt. The path
//! and the rules reach the claude through the hook (`mulpex_core::secrets`). The
//! claude loads the file with `set -a; . <path>; set +a` and refers to `$KEY`, so
//! the value lives only in the shell, never in a tool call or its output.
//!
//! A one-off set lives in the project scratch dir under `secrets/<id>/`. It goes
//! when that instance is reaped (`Core::forget_session_files`) and when the whole
//! scratch dir is removed at teardown.
//!
//! A saved set lives in `<mulpex home>/secrets/<name>.env` (global, optionally
//! limited to one project by a header line) and outlives everything. Handing one
//! to an instance writes `secrets/<id>/<name>.ref`, which is reaped like a
//! one-off set while the saved file stays.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use mulpex_core::secrets::{KEYS_HEADER, PROJECT_HEADER};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SecretRow {
    pub key: String,
    pub value: String,
}

/// A shell identifier, so `set -a; . file` exports it as written.
fn valid_key(k: &str) -> bool {
    let mut chars = k.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// The sourceable body: `KEY='value'`, with `'` written as `'\''`, so `$`,
/// backticks, spaces, `"` and newlines all reach the shell literally.
fn render(rows: &[SecretRow], project: Option<&str>) -> Result<String, String> {
    if rows.is_empty() {
        return Err("Add at least one secret".into());
    }
    let mut seen = std::collections::HashSet::new();
    let mut keys = Vec::new();
    let mut out = String::new();
    for r in rows {
        let k = r.key.trim();
        if !valid_key(k) {
            return Err(format!(
                "\"{k}\" is not a valid name (letters, digits and _, not starting with a digit)"
            ));
        }
        if !seen.insert(k.to_string()) {
            return Err(format!("\"{k}\" appears twice"));
        }
        keys.push(k);
        out.push_str(&format!("{k}='{}'\n", r.value.replace('\'', "'\\''")));
    }
    // The header the hook reads the key names from (`mulpex_core::secrets`).
    let tag = project.map(|p| format!("{PROJECT_HEADER}{p}\n")).unwrap_or_default();
    Ok(format!("{KEYS_HEADER}{}\n{tag}{out}", keys.join(" ")))
}

/// Create `dir` (and its parents) as owner-only.
pub(crate) fn private_dir(dir: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("create {}: {e}", dir.display()))
}

/// Write `body` to `path` as 0600. Atomic: the temp file is 0600 from its first
/// byte and is renamed over the target, so no reader ever sees a half-written
/// file or a moment of wider permissions.
pub(crate) fn write_private(path: &Path, body: &str) -> Result<(), String> {
    let tmp = path.with_extension("env.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    f.write_all(body.as_bytes())
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("write {}: {e}", path.display()))
}

pub use mulpex_core::secrets::instance_dir;

/// Write a one-off set for instance `id`, as the first free `secrets-N.env`.
/// Returns the file's path.
pub fn create_ephemeral(state_dir: &Path, id: usize, rows: &[SecretRow]) -> Result<PathBuf, String> {
    let body = render(rows, None)?;
    let dir = instance_dir(state_dir, id);
    private_dir(&dir)?;
    let path = (1..)
        .map(|n| dir.join(format!("secrets-{n}.env")))
        .find(|p| !p.exists())
        .expect("unbounded range");
    write_private(&path, &body)?;
    Ok(path)
}

/// `<mulpex home>/secrets/`, where saved sets live.
pub fn saved_dir(home: &Path) -> PathBuf {
    home.join("secrets")
}

/// A saved set as the dialog lists it: never its values.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SavedSet {
    pub name: String,
    pub keys: Vec<String>,
    /// Limited to the project this list was asked for (otherwise: everywhere).
    pub project_only: bool,
}

/// A saved set's name doubles as its file name, so it must be a plain one.
fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && !n.starts_with('.')
        && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The two header lines of a saved file: its keys, and the project it is limited to.
fn headers(path: &Path) -> Option<(Vec<String>, Option<String>)> {
    let body = std::fs::read_to_string(path).ok()?;
    let mut lines = body.lines();
    let keys = lines.next()?.strip_prefix(KEYS_HEADER)?;
    let project = lines.next().and_then(|l| l.strip_prefix(PROJECT_HEADER)).map(str::to_string);
    Some((keys.split_whitespace().map(str::to_string).collect(), project))
}

/// The saved sets usable in `project`: every untagged one, plus those tagged to
/// it. Sorted by name.
pub fn list_saved(home: &Path, project: &str) -> Vec<SavedSet> {
    let Ok(entries) = std::fs::read_dir(saved_dir(home)) else {
        return Vec::new();
    };
    let mut out: Vec<SavedSet> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "env"))
        .filter_map(|p| {
            let (keys, tag) = headers(&p)?;
            if tag.as_deref().is_some_and(|t| t != project) {
                return None;
            }
            let name = p.file_stem()?.to_string_lossy().into_owned();
            Some(SavedSet { name, keys, project_only: tag.is_some() })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Save a new set as `<home>/secrets/<name>.env`. Refuses a name already taken:
/// replacing a saved password is Edit's job, not a side effect of a typo.
pub fn save(home: &Path, name: &str, project: Option<&str>, rows: &[SecretRow]) -> Result<PathBuf, String> {
    let name = name.trim();
    if !valid_name(name) {
        return Err("Name: letters, digits, - _ . only".into());
    }
    let body = render(rows, project)?;
    let dir = saved_dir(home);
    private_dir(&dir)?;
    let path = dir.join(format!("{name}.env"));
    if path.exists() {
        return Err(format!("A saved set named \"{name}\" already exists"));
    }
    write_private(&path, &body)?;
    Ok(path)
}

/// Hand saved set `name` to instance `id`, if it is usable in `project`.
pub fn attach(home: &Path, project: &str, state_dir: &Path, id: usize, name: &str) -> Result<Vec<String>, String> {
    let set = list_saved(home, project)
        .into_iter()
        .find(|s| s.name == name)
        .ok_or(format!("No saved set \"{name}\" here"))?;
    let dir = instance_dir(state_dir, id);
    private_dir(&dir)?;
    let target = saved_dir(home).join(format!("{name}.env"));
    std::fs::write(dir.join(format!("{name}.ref")), target.to_string_lossy().as_bytes())
        .map_err(|e| format!("attach {name}: {e}"))?;
    Ok(set.keys)
}

/// A saved set opened for editing: the one place values leave a file, and only
/// back into the dialog that wrote them.
#[derive(Debug, Clone, Serialize)]
pub struct SavedDetail {
    pub rows: Vec<SecretRow>,
    pub project_only: bool,
}

/// Read back what `render` wrote: `KEY='…'` lines, where a `'` inside a value
/// is `'\''`. Header comments are skipped. Anything else is an error rather
/// than a guess, so a hand-edited file is never silently half-loaded and then
/// saved back over itself.
fn parse(body: &str) -> Result<Vec<SecretRow>, String> {
    let mut rows = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        if rest.starts_with('#') || rest.starts_with('\n') {
            rest = rest.split_once('\n').map_or("", |(_, r)| r);
            continue;
        }
        let (key, after) = rest.split_once("='").ok_or("This file was not written by Mulpex")?;
        if !valid_key(key) {
            return Err(format!("This file was not written by Mulpex (\"{key}\")"));
        }
        let mut value = String::new();
        let mut tail = after;
        loop {
            let end = tail.find('\'').ok_or("Unterminated value")?;
            value.push_str(&tail[..end]);
            tail = &tail[end + 1..];
            match tail.strip_prefix("\\''") {
                Some(t) => {
                    value.push('\'');
                    tail = t;
                }
                None => break,
            }
        }
        rows.push(SecretRow { key: key.to_string(), value });
        rest = tail.strip_prefix('\n').unwrap_or(tail);
    }
    Ok(rows)
}

fn saved_path(home: &Path, name: &str) -> Result<PathBuf, String> {
    if !valid_name(name) {
        return Err(format!("No saved set \"{name}\""));
    }
    Ok(saved_dir(home).join(format!("{name}.env")))
}

/// Open saved set `name` for editing.
pub fn get_saved(home: &Path, name: &str) -> Result<SavedDetail, String> {
    let path = saved_path(home, name)?;
    let body = std::fs::read_to_string(&path).map_err(|e| format!("{name}: {e}"))?;
    let (_, tag) = headers(&path).ok_or("This file was not written by Mulpex")?;
    Ok(SavedDetail { rows: parse(&body)?, project_only: tag.is_some() })
}

/// Replace saved set `name`'s rows and tag. Every instance holding a ref to it
/// sees the new values the next time it sources the file.
pub fn update(home: &Path, name: &str, project: Option<&str>, rows: &[SecretRow]) -> Result<(), String> {
    let path = saved_path(home, name)?;
    if !path.is_file() {
        return Err(format!("No saved set \"{name}\""));
    }
    write_private(&path, &render(rows, project)?)
}

/// Delete saved set `name`. Refs to it go dangling and drop out of the hook's
/// note (`mulpex_core::secrets::files_of`).
pub fn delete(home: &Path, name: &str) -> Result<(), String> {
    std::fs::remove_file(saved_path(home, name)?).map_err(|e| format!("{name}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mulpex-secrets-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn row(k: &str, v: &str) -> SecretRow {
        SecretRow { key: k.into(), value: v.into() }
    }

    #[test]
    fn values_survive_sourcing_literally() {
        let dir = scratch("roundtrip");
        let nasty = "it's $HOME `id` \"q\" \\ back\nline2 ;&| *";
        let rows = vec![row("PASS", nasty), row("USER", "root")];
        let path = create_ephemeral(&dir, 3, &rows).unwrap();
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(r#"set -a; . "$1"; set +a; printf %s "$PASS""#)
            .arg("_")
            .arg(&path)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), nasty);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_and_dir_are_owner_only_and_numbered() {
        let dir = scratch("mode");
        let a = create_ephemeral(&dir, 1, &[row("A", "1")]).unwrap();
        let b = create_ephemeral(&dir, 1, &[row("B", "2")]).unwrap();
        assert!(a.ends_with("secrets/1/secrets-1.env"));
        assert!(b.ends_with("secrets/1/secrets-2.env"));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&a), 0o600);
        assert_eq!(mode(&instance_dir(&dir, 1)), 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_bad_and_duplicate_keys() {
        assert!(render(&[], None).is_err());
        assert!(render(&[row("1A", "x")], None).is_err());
        assert!(render(&[row("A-B", "x")], None).is_err());
        assert!(render(&[row("", "x")], None).is_err());
        assert!(render(&[row("A", "x"), row("A", "y")], None).is_err());
        assert!(render(&[row("_a1", "x"), row(" B ", "y")], None).unwrap().starts_with("# keys: _a1 B\n"));
    }

    #[test]
    fn saved_sets_filter_by_project_and_attach_by_ref() {
        let home = scratch("saved");
        let state = home.join("state");
        save(&home, "shared", None, &[row("A", "1")]).unwrap();
        save(&home, "prod", Some("/proj/a"), &[row("PASS", "it's")]).unwrap();
        assert!(save(&home, "prod", None, &[row("X", "1")]).is_err(), "a taken name was overwritten");
        assert!(save(&home, "../x", None, &[row("X", "1")]).is_err());

        let names = |p: &str| list_saved(&home, p).into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(names("/proj/a"), ["prod", "shared"]);
        assert_eq!(names("/proj/b"), ["shared"]);
        let mode = std::fs::metadata(saved_dir(&home).join("prod.env")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        assert!(attach(&home, "/proj/b", &state, 4, "prod").is_err(), "attached outside its project");
        assert_eq!(attach(&home, "/proj/a", &state, 4, "prod").unwrap(), ["PASS"]);
        let note = mulpex_core::secrets::context(&state, 4).unwrap();
        assert!(note.contains("secrets/prod.env (keys: PASS)"), "{note}");

        // The tagged file still sources cleanly, header lines and all.
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(r#"set -a; . "$1"; set +a; printf %s "$PASS""#)
            .arg("_")
            .arg(saved_dir(&home).join("prod.env"))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "it's");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_saved_set_reads_back_edits_and_deletes() {
        let home = scratch("edit");
        let state = home.join("state");
        let nasty = "it's '' \\'' $X\nline2\n";
        save(&home, "s", Some("/p"), &[row("A", nasty), row("B", "")]).unwrap();
        let d = get_saved(&home, "s").unwrap();
        assert!(d.project_only);
        assert_eq!(d.rows, vec![row("A", nasty), row("B", "")]);

        attach(&home, "/p", &state, 1, "s").unwrap();
        update(&home, "s", None, &[row("C", "new")]).unwrap();
        let d = get_saved(&home, "s").unwrap();
        assert!(!d.project_only);
        assert_eq!(d.rows, vec![row("C", "new")]);
        assert!(update(&home, "nope", None, &[row("C", "x")]).is_err());

        delete(&home, "s").unwrap();
        assert!(list_saved(&home, "/p").is_empty());
        assert!(mulpex_core::secrets::context(&state, 1).is_none(), "a deleted set is still announced");
        assert!(get_saved(&home, "../x").is_err());
        let _ = std::fs::remove_dir_all(&home);
    }
}
