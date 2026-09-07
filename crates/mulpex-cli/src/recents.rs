//! The projects you have opened before, so the picker is useful on day one.
//!
//! Two files, read as one list:
//!
//! - **`~/.mulpex-cli/recents.txt`** — mpx's own, written every time a project
//!   opens. This is the one that works on a server.
//! - **`~/.mulpex/recents.txt`** — the desktop app's, read-only.
//!
//! Reading the app's file is deliberate and is **not** the collision `statedir.rs`
//! goes out of its way to avoid. That one is about `persist::SessionStore`, which
//! is keyed by project directory: two frontends sharing it would hand the same
//! `--resume` uuid to two claudes and silently interleave one transcript. A recents
//! file is a list of directory paths — nothing resumes off it, nothing is written
//! back to it, and the worst it can do is offer a project you did not want. On a
//! Mac that means the first `p` you ever press already knows your projects; on a
//! server the file does not exist and the list is simply mpx's own.

use std::path::{Path, PathBuf};

const MAX: usize = 20;

fn ours() -> PathBuf {
    crate::statedir::cli_home().join("recents.txt")
}

/// The desktop app's list. `mulpex_home()` is the app's own resolver, so a debug
/// build reads the debug app's file and `MULPEX_HOME` still overrides — in which
/// case both paths may be the same file, which `list` dedups anyway.
fn theirs() -> PathBuf {
    mulpex_core::mulpex_home().join("recents.txt")
}

fn read(path: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Most-recent first, mpx's own list ahead of the app's, deduped, and filtered to
/// directories that still exist — a picker offering a folder you deleted last
/// month is a picker you stop trusting.
pub fn list() -> Vec<PathBuf> {
    merge(&ours(), &theirs())
}

/// Record `dir` as the most recent. Writes **only** mpx's own file; the app's is
/// never touched.
///
/// Best effort throughout: failing to open a project because its path could not be
/// remembered would be the wrong trade entirely.
pub fn add(dir: &Path) {
    push(&ours(), dir);
}

// The bodies, taking their files as arguments so the tests do not have to set
// `MULPEX_HOME` — an env var is process-wide, and cargo runs these tests in
// parallel threads of one process.

fn merge(ours: &Path, theirs: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in read(ours).into_iter().chain(read(theirs)) {
        if dir.is_dir() && !out.contains(&dir) {
            out.push(dir);
        }
    }
    out
}

fn push(file: &Path, dir: &Path) {
    let mut list = read(file);
    list.retain(|p| p != dir);
    list.insert(0, dir.to_path_buf());
    list.truncate(MAX);
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text: Vec<String> = list.iter().map(|p| p.to_string_lossy().to_string()).collect();
    let _ = std::fs::write(file, text.join("\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Most-recent first, no duplicates, capped — and re-opening something already
    /// in the list moves it to the top rather than adding it twice.
    #[test]
    fn opening_a_project_puts_it_at_the_top() {
        let home = std::env::temp_dir().join(format!("mpx-recents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let file = home.join("recents.txt");
        let none = home.join("absent.txt");

        let dirs: Vec<PathBuf> = (0..3)
            .map(|i| {
                let d = home.join(format!("proj{i}"));
                std::fs::create_dir_all(&d).unwrap();
                d
            })
            .collect();
        for d in &dirs {
            push(&file, d);
        }
        push(&file, &dirs[0]);

        let got = merge(&file, &none);
        assert_eq!(got[0], dirs[0], "the one just opened is first");
        assert_eq!(got.iter().filter(|p| **p == dirs[0]).count(), 1, "no duplicate");
        assert_eq!(got.len(), 3);

        // A directory that has since been deleted must not be offered.
        std::fs::remove_dir_all(&dirs[1]).unwrap();
        assert!(!merge(&file, &none).contains(&dirs[1]));

        let _ = std::fs::remove_dir_all(&home);
    }

    /// The desktop app's list is appended, not merged blindly: mpx's own entries
    /// come first, and a project in both appears once.
    #[test]
    fn the_apps_list_fills_in_behind_ours() {
        let home = std::env::temp_dir().join(format!("mpx-recents2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let (mine, app) = (home.join("mine.txt"), home.join("app.txt"));
        let (a, b) = (home.join("a"), home.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        push(&mine, &a);
        std::fs::write(&app, format!("{}\n{}", b.display(), a.display())).unwrap();

        assert_eq!(merge(&mine, &app), vec![a.clone(), b.clone()]);
        // With no list of our own, the app's is the whole answer.
        assert_eq!(merge(&home.join("none.txt"), &app), vec![b, a]);

        let _ = std::fs::remove_dir_all(&home);
    }
}
