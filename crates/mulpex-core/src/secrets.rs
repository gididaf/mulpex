//! The ⌘K secrets layout, shared by the app (which writes the files) and the
//! hook (which tells the claude about them).
//!
//! `secrets/<id>/*.env` holds the one-off sets handed to instance `id`, and
//! `secrets/<id>/<name>.ref` points at a saved set in `<mulpex home>/secrets/`
//! that was handed to it. Reaping the instance removes both; the saved file
//! stays. The first line of
//! each file is `# keys: A B`, so the hook can name the keys without parsing the
//! quoted values. A value can span lines, so a line-by-line `KEY=` scan could be
//! fooled by one.
//!
//! The user's prompt carries only a short tag (`🔑 A, B`). The path and the rules
//! reach the claude as hidden `additionalContext` on every turn while the file
//! exists, so they survive `/compact` and stop the moment the instance is reaped.

use std::path::{Path, PathBuf};

pub const SECRETS_DIR: &str = "secrets";

/// Prefix of each file's first line. The rest of the line is the space-separated keys.
pub const KEYS_HEADER: &str = "# keys: ";

/// Optional second line of a saved set: the project it is limited to.
pub const PROJECT_HEADER: &str = "# mulpex-project: ";

/// `secrets/<id>/` for one instance.
pub fn instance_dir(state_dir: &Path, id: usize) -> PathBuf {
    state_dir.join(SECRETS_DIR).join(id.to_string())
}

/// The keys named in a file's header line, or None if the header is missing.
fn keys_of(path: &Path) -> Option<Vec<String>> {
    let body = std::fs::read_to_string(path).ok()?;
    let first = body.lines().next()?;
    let keys = first.strip_prefix(KEYS_HEADER)?;
    Some(keys.split_whitespace().map(str::to_string).collect())
}

/// The sets handed to instance `id`: its one-off `*.env` files, plus the saved
/// sets its `*.ref` files point at (a ref holds the saved file's absolute path).
/// A ref whose target is gone is skipped, so deleting a saved set needs no
/// sweep through every instance.
pub fn files_of(state_dir: &Path, id: usize) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(instance_dir(state_dir, id)) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter_map(|p| match p.extension().and_then(|x| x.to_str()) {
            Some("env") => Some(p),
            Some("ref") => std::fs::read_to_string(&p)
                .ok()
                .map(|s| PathBuf::from(s.trim()))
                .filter(|t| t.is_file()),
            _ => None,
        })
        .collect();
    files.sort();
    files.dedup();
    files
}

/// The hidden note for instance `id`: one line per set it holds, then the rules.
/// None when it holds none.
pub fn context(state_dir: &Path, id: usize) -> Option<String> {
    let files = files_of(state_dir, id);
    if files.is_empty() {
        return None;
    }
    let mut out = String::from(
        "[Mulpex secrets] The user gave you secrets in files. A \"🔑 KEY\" tag in their prompt \
         refers to these:\n",
    );
    for f in &files {
        let keys = keys_of(f).unwrap_or_default();
        out.push_str(&format!("  - {} (keys: {})\n", f.display(), keys.join(", ")));
    }
    out.push_str(
        "Use a value only through the shell: `set -a; . <file>; set +a`, then refer to $KEY. \
         Never print, echo, cat or Read a value, and never write one into a file or a command \
         line in clear text. Keeping the values out of this transcript is the point.",
    );
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_lists_each_env_with_its_keys() {
        let dir = std::env::temp_dir().join(format!("mulpex-core-secrets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(context(&dir, 2).is_none(), "no dir means no note");
        let d = instance_dir(&dir, 2);
        std::fs::create_dir_all(&d).unwrap();
        assert!(context(&dir, 2).is_none(), "an empty dir means no note");
        std::fs::write(d.join("secrets-1.env"), "# keys: A B\nA='x'\nB='y\nC=z'\n").unwrap();
        std::fs::write(d.join("secrets-1.env.tmp"), "# keys: HALF\n").unwrap();
        let note = context(&dir, 2).unwrap();
        assert!(note.contains("secrets-1.env (keys: A, B)"), "{note}");
        assert!(!note.contains("HALF"), "a half-written temp file leaked: {note}");
        assert!(!note.contains("'x'"), "a value leaked: {note}");

        // A saved set is reached through a ref; a dangling ref is skipped.
        let saved = dir.join("prod.env");
        std::fs::write(&saved, "# keys: PASS\n# mulpex-project: /p\nPASS='z'\n").unwrap();
        std::fs::write(d.join("prod.ref"), format!("{}\n", saved.display())).unwrap();
        std::fs::write(d.join("gone.ref"), dir.join("gone.env").display().to_string()).unwrap();
        let note = context(&dir, 2).unwrap();
        assert!(note.contains("prod.env (keys: PASS)"), "{note}");
        assert!(!note.contains("gone"), "a dangling ref was listed: {note}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
