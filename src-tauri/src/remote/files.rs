//! Files the phone attaches to a chat message (`file-put`).
//!
//! Each file arrives in chunks (a whole one would not fit a relay frame once
//! sealed) and is written to `<mulpex home>/remote/phone-files/<uuid>/<name>`,
//! 0700 dirs, outside any repo. The phone then names the finished paths in the
//! message it types, and the claude reads them itself. Kept a week.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

/// The most one file may be. The phone also caps a message's files at this.
pub const MAX_FILE: u64 = 25 * 1024 * 1024;
/// Finished files are kept this long, then pruned.
const TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// An upload no chunk has reached for this long is dropped.
const STALE: Duration = Duration::from_secs(10 * 60);

struct Up {
    path: PathBuf,
    file: std::fs::File,
    bytes: u64,
    at: Instant,
}

static UPLOADS: Mutex<Option<HashMap<String, Up>>> = Mutex::new(None);

fn files_dir() -> PathBuf {
    super::remote_dir().join("phone-files")
}

/// A file name the phone sent, made safe: its last component only, no control
/// characters, not `.`/`..`. `None` when nothing is left.
fn safe_name(name: &str) -> Option<String> {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let s: String = last.chars().filter(|c| !c.is_control()).collect();
    let s = s.trim().chars().take(120).collect::<String>();
    (!s.is_empty() && s != "." && s != "..").then_some(s)
}

fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > TTL);
        if old {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// One chunk. The first has no `upload` and names the file; it returns the
/// upload token the rest carry. The `done` one returns the file's full path.
pub fn put(name: Option<&str>, upload: Option<&str>, data: &str, done: bool) -> Result<Value, String> {
    put_in(&files_dir(), name, upload, data, done)
}

fn put_in(dir: &Path, name: Option<&str>, upload: Option<&str>, data: &str, done: bool) -> Result<Value, String> {
    use base64::Engine;
    // Standard base64, as the phone's `btoa` writes it (the Board's images too).
    let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).map_err(|_| "bad upload".to_string())?;
    let mut g = UPLOADS.lock().unwrap();
    let ups = g.get_or_insert_with(HashMap::new);
    let token = match upload {
        Some(t) => {
            if !ups.contains_key(t) {
                return Err("that upload expired — attach the file again".into());
            }
            t.to_string()
        }
        None => {
            ups.retain(|_, u| u.at.elapsed() < STALE);
            prune(dir);
            let name = name.and_then(safe_name).ok_or("bad file name")?;
            let token = mulpex_core::persist::new_uuid();
            let sub = dir.join(&token);
            crate::secrets::private_dir(dir)?;
            crate::secrets::private_dir(&sub)?;
            let path = sub.join(name);
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|e| format!("create {}: {e}", path.display()))?;
            ups.insert(token.clone(), Up { path, file, bytes: 0, at: Instant::now() });
            token
        }
    };
    let up = ups.get_mut(&token).expect("checked above");
    up.bytes += bytes.len() as u64;
    up.at = Instant::now();
    let wrote = if up.bytes > MAX_FILE {
        Err(format!("too big (over {} MB)", MAX_FILE / 1024 / 1024))
    } else {
        up.file.write_all(&bytes).map_err(|e| e.to_string())
    };
    if let Err(e) = wrote {
        if let Some(u) = ups.remove(&token) {
            if let Some(d) = u.path.parent() {
                let _ = std::fs::remove_dir_all(d);
            }
        }
        return Err(e);
    }
    if done {
        let u = ups.remove(&token).expect("checked above");
        return Ok(json!({ "path": u.path.display().to_string() }));
    }
    Ok(json!({ "upload": token }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn b64(b: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(b)
    }

    #[test]
    fn names_are_made_safe() {
        assert_eq!(safe_name("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(safe_name("C:\\x\\photo.jpg").as_deref(), Some("photo.jpg"));
        assert_eq!(safe_name("a\nb.txt").as_deref(), Some("ab.txt"));
        assert_eq!(safe_name(".."), None);
        assert_eq!(safe_name("dir/"), None);
    }

    #[test]
    fn a_file_arrives_in_chunks() {
        let dir = std::env::temp_dir().join(format!("mpx-files-{}", std::process::id()));
        let r = put_in(&dir, Some("../notes.txt"), None, &b64(b"hello "), false).unwrap();
        let t = r["upload"].as_str().unwrap().to_string();
        let r = put_in(&dir, None, Some(&t), &b64(b"world"), true).unwrap();
        let path = PathBuf::from(r["path"].as_str().unwrap());
        assert!(path.starts_with(&dir));
        assert_eq!(path.file_name().unwrap(), "notes.txt");
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
        // The token is spent once done.
        assert!(put_in(&dir, None, Some(&t), &b64(b"x"), true).is_err());
        // Too big is refused and leaves nothing behind.
        let big = vec![0u8; (MAX_FILE + 1) as usize];
        assert!(put_in(&dir, Some("big.bin"), None, &b64(&big), true).is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
