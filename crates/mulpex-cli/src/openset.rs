//! Which projects were open, so `mpx` on a rebooted machine puts them all back.
//!
//! `~/.mulpex-cli/open.txt`, one absolute path per line — the CLI's own copy of
//! the desktop app's `open.txt`, and distinct from `recents.txt`: recents is
//! everywhere you have *ever* been, this is what was on screen.
//!
//! The set changes on exactly two events, and both are deliberate acts:
//! **opening** a project adds it, **closing** one removes it (`mpx down`, or the
//! last instance being closed, which destroys the session). Nothing else touches
//! it — which is the whole point. A reboot, a `kill-server`, an ssh drop and a
//! crashed daemon all leave the file exactly as it was, so what comes back is what
//! you had, not what happened to survive.
//!
//! The app reads its own file on launch and reopens everything in it. `mpx` does
//! the same, but only when nothing is already running: with a tmux server up, the
//! sessions themselves are the better answer.

use std::path::{Path, PathBuf};

fn file() -> PathBuf {
    crate::statedir::cli_home().join("open.txt")
}

/// The projects that were open, in the order they were opened. Filtered to
/// directories that still exist — see `read`.
pub fn list() -> Vec<PathBuf> {
    read(&file())
}

pub fn add(dir: &Path) {
    insert(&file(), dir);
}

pub fn remove(dir: &Path) {
    drop_one(&file(), dir);
}

// Bodies take their file as an argument so the tests need no process-wide
// `MULPEX_HOME`; cargo runs them in parallel threads of one process.

/// A directory that no longer exists is dropped on read, not on write. Written
/// that way round because the usual cause is a disk that is not mounted *yet* —
/// deleting the entry would make a temporary absence permanent, and silently.
fn read(path: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .collect()
}

/// Every line, existing or not — `insert` and `drop_one` must not quietly discard
/// an unmounted project just because something else was opened.
fn read_raw(path: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn insert(path: &Path, dir: &Path) {
    let mut list = read_raw(path);
    if list.iter().any(|p| p == dir) {
        return;
    }
    // Appended, not prepended: this is the tab strip's order, and a project
    // jumping to the front every time you reopen it would reshuffle the tabs.
    list.push(dir.to_path_buf());
    write(path, &list);
}

fn drop_one(path: &Path, dir: &Path) {
    let mut list = read_raw(path);
    let before = list.len();
    list.retain(|p| p != dir);
    if list.len() != before {
        write(path, &list);
    }
}

fn write(path: &Path, list: &[PathBuf]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text: Vec<String> = list.iter().map(|p| p.to_string_lossy().to_string()).collect();
    let _ = std::fs::write(path, text.join("\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_set_keeps_its_order_and_holds_each_project_once() {
        let home = std::env::temp_dir().join(format!("mpx-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let f = home.join("open.txt");
        let dirs: Vec<PathBuf> = (0..3)
            .map(|i| {
                let d = home.join(format!("p{i}"));
                std::fs::create_dir_all(&d).unwrap();
                d
            })
            .collect();

        for d in &dirs {
            insert(&f, d);
        }
        insert(&f, &dirs[0]);
        assert_eq!(read(&f), dirs, "reopening one must not reorder the tabs or duplicate it");

        drop_one(&f, &dirs[1]);
        assert_eq!(read(&f), vec![dirs[0].clone(), dirs[2].clone()]);

        let _ = std::fs::remove_dir_all(&home);
    }

    /// An unmounted or renamed project is skipped when the set is *used* and kept
    /// when it is written. Dropping it on write would turn "the disk is not up
    /// yet" into "that project is gone", with nothing anywhere saying so.
    #[test]
    fn a_missing_directory_is_skipped_but_not_forgotten() {
        let home = std::env::temp_dir().join(format!("mpx-open2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let f = home.join("open.txt");
        let (here, gone) = (home.join("here"), home.join("gone"));
        std::fs::create_dir_all(&here).unwrap();
        std::fs::create_dir_all(&gone).unwrap();

        insert(&f, &gone);
        insert(&f, &here);
        std::fs::remove_dir_all(&gone).unwrap();

        assert_eq!(read(&f), vec![here.clone()], "not offered while it is missing");
        // ...and opening something else must not have erased it.
        let other = home.join("other");
        std::fs::create_dir_all(&other).unwrap();
        insert(&f, &other);
        assert!(read_raw(&f).contains(&gone), "still remembered, ready for the disk to come back");

        let _ = std::fs::remove_dir_all(&home);
    }
}
