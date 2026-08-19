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

/// One persisted instance: the Claude session id to `--resume`, plus the bits of
/// sidebar state that must survive a restart (custom name, muted flag).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SavedSession {
    pub session_id: String,
    pub name: Option<String>,
    pub muted: bool,
    /// The instance NUMBER this session had — the `claude#15` the user reads,
    /// types into `hub_send` and remembers a conversation by.
    ///
    /// Persisted because it is an identity, not an index. Without it a restore
    /// hands out `sessions.len() + 1`, so #2/#3/#15 silently come back as
    /// #1/#2/#3: every number now names a different conversation than it did
    /// before the restart, and nothing on screen says so. `None` is a store
    /// written before this column existed — those still number sequentially.
    pub id: Option<usize>,
}

/// The tab-separated flag marking a muted instance in the store file.
const MUTED_FLAG: &str = "muted";

/// A per-project file recording the session ids to restore.
pub struct SessionStore {
    path: PathBuf,
    project_dir: PathBuf,
}

impl SessionStore {
    /// The file this store reads and writes.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

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

    /// Load saved sessions for this project, in order. Returns empty on any
    /// error, if there is no store yet, or if the recorded project path doesn't
    /// match (guards against a hash collision clobbering another project).
    ///
    /// Each line is `<uuid>[\t<name>[\tmuted[\t<id>]]]`, so every older format
    /// still loads: a bare uuid (before names existed) yields no name and
    /// unmuted, a `<uuid>\t<name>` line (before mute existed) yields unmuted, and
    /// a three-column line (before ids were persisted) yields `id: None` and is
    /// numbered sequentially on restore, exactly as it used to be. The columns
    /// are positional, so an instance that has only some of them writes the
    /// earlier ones empty — a muted unnamed instance is `<uuid>\t\tmuted`, and
    /// an unnamed unmuted instance with an id is `<uuid>\t\t\t15`.
    pub fn load(&self) -> Vec<SavedSession> {
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
                let mut parts = l.splitn(4, '\t');
                let session_id = parts.next().unwrap_or("").to_string();
                let name = parts
                    .next()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(String::from);
                let muted = parts.next().map(str::trim) == Some(MUTED_FLAG);
                let id = parts.next().map(str::trim).and_then(|v| v.parse().ok());
                SavedSession {
                    session_id,
                    name,
                    muted,
                    id,
                }
            })
            .filter(|s| !s.session_id.is_empty())
            .collect()
    }

    /// Persist `sessions` (in order) for this project as
    /// `<uuid>[\t<name>[\tmuted[\t<id>]]]`. Trailing empty columns are dropped, so
    /// a store with nothing new in it is written byte-identically to the older
    /// format. Best-effort: any I/O failure is silently ignored.
    pub fn save(&self, sessions: &[SavedSession]) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut out = format!("# {}\n", self.project_dir.display());
        for s in sessions {
            out.push_str(&s.session_id);
            // The name shares the line after a tab; strip tabs/newlines so it
            // can't corrupt the one-record-per-line format. The muted flag sits
            // in a third field, so a muted-but-unnamed instance still needs the
            // (empty) name field written to keep the columns aligned.
            let name = s
                .name
                .as_deref()
                .map(|n| n.replace(['\t', '\n', '\r'], " "))
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty());
            let mut cols = [
                name.unwrap_or_default(),
                if s.muted { MUTED_FLAG.into() } else { String::new() },
                s.id.map(|i| i.to_string()).unwrap_or_default(),
            ];
            // Positional columns, so only trailing empties can be dropped — an
            // id with no name still needs its empty name and muted fields
            // written or it would be read back as the name.
            let keep = cols.iter().rposition(|c| !c.is_empty()).map_or(0, |i| i + 1);
            for col in cols.iter_mut().take(keep) {
                out.push('\t');
                out.push_str(col);
            }
            out.push('\n');
        }
        let _ = std::fs::write(&self.path, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(id: &str, name: Option<&str>, muted: bool) -> SavedSession {
        SavedSession {
            session_id: id.to_string(),
            name: name.map(String::from),
            muted,
            id: None,
        }
    }

    fn numbered(uuid: &str, name: Option<&str>, muted: bool, id: usize) -> SavedSession {
        SavedSession {
            id: Some(id),
            ..saved(uuid, name, muted)
        }
    }

    /// The instance number survives a save/load, including the awkward shapes:
    /// an unnamed unmuted instance still needs its two empty columns written or
    /// the number would be read back as the name.
    #[test]
    fn the_instance_number_round_trips_through_every_column_shape() {
        let dir = std::env::temp_dir().join(format!("mulpex-idcols-{}", new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);
        let project = dir.join("proj");
        let store = SessionStore::new(&project);

        let rows = vec![
            numbered("u1", Some("named"), false, 2),
            numbered("u2", None, false, 15),
            numbered("u3", None, true, 7),
            numbered("u4", Some("both"), true, 9),
            saved("u5", Some("no id at all"), false),
        ];
        store.save(&rows);
        assert_eq!(store.load(), rows, "a column shape did not survive the round trip");

        // The unnamed-with-id line must not read its number back as a name.
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(text.contains("u2\t\t\t15\n"), "columns misaligned:\n{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A store with nothing new in it must be written byte-identically to the
    /// older format, so upgrading Mulpex doesn't rewrite every project's file
    /// into something an older build would misread.
    #[test]
    fn a_store_with_no_ids_is_written_in_the_old_format() {
        let dir = std::env::temp_dir().join(format!("mulpex-oldfmt-{}", new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);
        let project = dir.join("proj");
        let store = SessionStore::new(&project);

        store.save(&[
            saved("u1", None, false),
            saved("u2", Some("named"), false),
            saved("u3", None, true),
        ]);
        let text = std::fs::read_to_string(store.path()).unwrap();
        let lines: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(lines, vec!["u1", "u2\tnamed", "u3\t\tmuted"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_load_round_trips_ids_names_and_mute() {
        let dir = std::env::temp_dir().join(format!("mulpex-persisttest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::new(&dir);

        // A tab/newline in a name is sanitized to spaces so the line format holds.
        // `uuid-d` is the awkward case: muted with no name, so the name column is
        // written empty to keep the muted flag in the third field.
        store.save(&[
            saved("uuid-a", Some("editor"), false),
            saved("uuid-b", None, false),
            saved("uuid-c", Some("weird\tname\nhere"), true),
            saved("uuid-d", None, true),
        ]);
        let loaded = store.load();
        assert_eq!(loaded.len(), 4);
        assert_eq!(loaded[0], saved("uuid-a", Some("editor"), false));
        assert_eq!(loaded[1], saved("uuid-b", None, false));
        assert_eq!(loaded[2], saved("uuid-c", Some("weird name here"), true));
        assert_eq!(loaded[3], saved("uuid-d", None, true));

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
        assert_eq!(loaded, vec![saved("uuid-1", None, false), saved("uuid-2", None, false)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_reads_pre_mute_name_lines() {
        // Files written before mute existed are `<uuid>\t<name>` — two fields,
        // which must still load as unmuted rather than being rejected.
        let dir = std::env::temp_dir().join(format!("mulpex-premutetest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::new(&dir);
        if let Some(parent) = store.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(
            &store.path,
            format!("# {}\nuuid-1\teditor\nuuid-2\n", dir.display()),
        )
        .unwrap();

        let loaded = store.load();
        assert_eq!(
            loaded,
            vec![saved("uuid-1", Some("editor"), false), saved("uuid-2", None, false)]
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
