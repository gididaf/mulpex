//! Per-project persistence of the Claude Code sessions Mulpex had open, so that
//! reopening Mulpex in the same project restores the sessions you were working
//! on.
//!
//! Each instance is assigned a Claude Code **session id** (a UUID) at spawn via
//! `--session-id`; we record the ids of the instances that were actually worked
//! on into a small per-project file. On the next launch we relaunch each saved
//! id with `--resume <id>`. Instances that were never used (no session of
//! substance) are simply never recorded, so they don't come back.
//!
//! The store is Mulpex's own — `~/.mulpex/sessions/<key>.txt` — and does not
//! touch Claude Code's own session storage.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Generate a random RFC-4122 v4 UUID string, dependency-free, from
/// `/dev/urandom`. Used as the `--session-id` for a new Claude instance so we
/// can later `--resume` exactly that session. If the random read fails (very
/// unlikely on macOS) the bytes stay zero, still yielding a well-formed UUID.
pub fn new_uuid() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut bytes);
    }
    // RFC 4122: set the version (4) and variant (10xx) bits.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32],
    )
}

/// A per-project file recording the session ids to restore.
pub struct SessionStore {
    path: PathBuf,
    project_dir: PathBuf,
}

impl SessionStore {
    /// Locate the store file for `project_dir`. The filename is a readable tail
    /// of the path plus a stable FNV-1a hash of the full path, so it is unique
    /// per project, bounded in length, and stable across Mulpex rebuilds.
    pub fn new(project_dir: &Path) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = home.join(".mulpex").join("sessions");

        let raw = project_dir.to_string_lossy();
        let sanitized: String = raw
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        // Keep the distinctive tail of the path for human legibility.
        let tail: String = {
            let chars: Vec<char> = sanitized.chars().collect();
            let start = chars.len().saturating_sub(80);
            chars[start..].iter().collect()
        };
        let key = format!("{tail}-{:016x}", fnv1a(raw.as_bytes()));

        Self {
            path: dir.join(format!("{key}.txt")),
            project_dir: project_dir.to_path_buf(),
        }
    }

    /// Load saved sessions for this project, in order, each as
    /// `(session id, optional custom name)`. Returns empty on any error, if
    /// there is no store yet, or if the recorded project path doesn't match
    /// (guards against a hash collision clobbering another project).
    ///
    /// Each line is `<uuid>` optionally followed by `\t<name>` — so files
    /// written before names existed (bare uuids) still load, with `None`.
    pub fn load(&self) -> Vec<(String, Option<String>)> {
        let Ok(content) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let mut lines = content.lines();
        // First line is `# <project dir>`, for verification.
        let Some(first) = lines.next() else {
            return Vec::new();
        };
        let stored = first.strip_prefix("# ").unwrap_or(first);
        if Path::new(stored) != self.project_dir {
            return Vec::new();
        }
        lines
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| {
                let mut parts = l.splitn(2, '\t');
                let id = parts.next().unwrap_or("").to_string();
                let name = parts
                    .next()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(String::from);
                (id, name)
            })
            .filter(|(id, _)| !id.is_empty())
            .collect()
    }

    /// Persist `sessions` (in order) for this project, each as its session id
    /// plus an optional custom name (`<uuid>\t<name>`). Best-effort: any I/O
    /// failure is silently ignored.
    pub fn save(&self, sessions: &[(&str, Option<&str>)]) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut out = format!("# {}\n", self.project_dir.display());
        for (id, name) in sessions {
            out.push_str(id);
            // A name shares the line after a tab; strip tabs/newlines so it
            // can't corrupt the one-record-per-line format.
            if let Some(n) = name {
                let n = n.replace(['\t', '\n', '\r'], " ");
                if !n.trim().is_empty() {
                    out.push('\t');
                    out.push_str(n.trim());
                }
            }
            out.push('\n');
        }
        let _ = std::fs::write(&self.path, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_round_trips_ids_and_names() {
        let dir = std::env::temp_dir().join(format!("mulpex-persisttest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::new(&dir);

        // A tab/newline in a name is sanitized to spaces so the line format holds.
        store.save(&[
            ("uuid-a", Some("editor")),
            ("uuid-b", None),
            ("uuid-c", Some("weird\tname\nhere")),
        ]);
        let loaded = store.load();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0], ("uuid-a".to_string(), Some("editor".to_string())));
        assert_eq!(loaded[1], ("uuid-b".to_string(), None));
        assert_eq!(loaded[2].0, "uuid-c");
        assert_eq!(loaded[2].1.as_deref(), Some("weird name here"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_reads_legacy_bare_uuid_lines() {
        // Files written before names existed are just `# dir` + bare uuids.
        let dir = std::env::temp_dir().join(format!("mulpex-legacytest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::new(&dir);
        if let Some(parent) = store.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&store.path, format!("# {}\nuuid-1\nuuid-2\n", dir.display())).unwrap();

        let loaded = store.load();
        assert_eq!(
            loaded,
            vec![
                ("uuid-1".to_string(), None),
                ("uuid-2".to_string(), None),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// FNV-1a 64-bit hash — small, stable, and dependency-free, used to make the
/// store filename unique per project path without relying on `DefaultHasher`
/// (whose output isn't guaranteed stable across builds). Also reused by the
/// lock coordinator (`hook.rs`) to key a file path to a lock/history entry.
pub(crate) fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}
