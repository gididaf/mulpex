//! Project-directory selection support: a small recent-projects list persisted
//! under `~/.mulpex/recents.txt`. A desktop app has no cwd to infer the project
//! from (the TUI used `current_dir()`), so the user opens one via a native folder
//! picker (frontend `@tauri-apps/plugin-dialog`) or picks from recents.

use std::path::PathBuf;

const MAX_RECENTS: usize = 12;

fn recents_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".mulpex").join("recents.txt")
}

/// The recent project dirs, most-recent first. Non-existent dirs are filtered.
pub fn list_recent() -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(recents_path()) else {
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
