//! Project-directory selection support: a small recent-projects list persisted
//! under `~/.mulpex/recents.txt`. A desktop app has no cwd to infer the project
//! from (the TUI used `current_dir()`), so the user opens one via a native folder
//! picker (frontend `@tauri-apps/plugin-dialog`) or picks from recents.

use std::path::PathBuf;

const MAX_RECENTS: usize = 12;

fn mulpex_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".mulpex")
}

fn recents_path() -> PathBuf {
    mulpex_dir().join("recents.txt")
}

/// The set of projects that were open when Mulpex last quit, in tab order.
fn open_path() -> PathBuf {
    mulpex_dir().join("open.txt")
}

/// Read a one-dir-per-line file, filtering blank + non-existent dirs.
fn read_dirs(path: &PathBuf) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| std::path::Path::new(l).is_dir())
        .map(String::from)
        .collect()
}

/// The recent project dirs, most-recent first. Non-existent dirs are filtered.
pub fn list_recent() -> Vec<String> {
    read_dirs(&recents_path())
}

/// The projects to reopen on launch (open when Mulpex last quit), in tab order.
/// Non-existent dirs are filtered.
pub fn list_open() -> Vec<String> {
    read_dirs(&open_path())
}

/// Rewrite the open-project set (one dir per line, tab order). Called whenever a
/// project opens or closes — NOT on teardown, so the set survives to next launch.
pub fn save_open(dirs: &[String]) {
    let file = open_path();
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(file, dirs.join("\n"));
}

/// Record `path` as the most-recent project (dedup, cap at `MAX_RECENTS`).
pub fn add_recent(path: &str) {
    let mut list = list_recent();
    list.retain(|p| p != path);
    list.insert(0, path.to_string());
    list.truncate(MAX_RECENTS);
    let file = recents_path();
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(file, list.join("\n"));
}
