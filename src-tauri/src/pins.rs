//! The ⌘⇧P pins, kept on disk so a pin comes back with its restored claude.
//!
//! One JSON file per project, `<mulpex home>/pins/<key>.json`, where `<key>` is
//! the session store's own filename stem — the two files describe the same
//! instances, so they are named alike. Shaped `{"dir": …, "pins": {"<id>": pin}}`.
//! A pin is the frontend's data (`src/lib/pins.ts`), stored here as opaque JSON.
//!
//! Keyed by the instance NUMBER, not the session uuid: the number is what the
//! store restores the row as, and the uuid can change under a running claude
//! (`reconcile_session_ids`).
//!
//! A pin lives exactly as long as its row in the session store: `Core::
//! persist_sessions` calls `retain` with the ids it just saved. That is why a
//! closed claude loses its pin while a quit does not — teardown never rewrites
//! the store — and why a later claude handed the same number cannot inherit one.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Default)]
struct PinFile {
    dir: PathBuf,
    pins: BTreeMap<usize, Value>,
}

pub struct PinStore {
    path: PathBuf,
    project_dir: PathBuf,
}

impl PinStore {
    /// The pin file beside `session_store_path` (`<home>/sessions/<key>.txt` →
    /// `<home>/pins/<key>.json`).
    pub fn beside(session_store_path: &Path, project_dir: &Path) -> Self {
        let home = session_store_path
            .parent()
            .and_then(Path::parent)
            .unwrap_or_else(|| Path::new("."));
        let stem = session_store_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self {
            path: home.join("pins").join(format!("{stem}.json")),
            project_dir: project_dir.to_path_buf(),
        }
    }

    /// Every saved pin, by instance id. Empty on any error, and when the file
    /// belongs to another project (a hash collision, as the session store guards).
    pub fn load(&self) -> BTreeMap<usize, Value> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return BTreeMap::new();
        };
        match serde_json::from_str::<PinFile>(&text) {
            Ok(f) if f.dir == self.project_dir => f.pins,
            _ => BTreeMap::new(),
        }
    }

    /// Set (`Some`) or clear (`None`) one instance's pin.
    pub fn set(&self, id: usize, pin: Option<Value>) {
        let mut pins = self.load();
        match pin {
            Some(p) => pins.insert(id, p),
            None => pins.remove(&id),
        };
        self.save(pins);
    }

    /// Drop the pins of every instance not in `ids`. Writes only on a change, as
    /// this runs every time the session store is saved.
    pub fn retain(&self, ids: &HashSet<usize>) {
        let mut pins = self.load();
        let before = pins.len();
        pins.retain(|id, _| ids.contains(id));
        if pins.len() != before {
            self.save(pins);
        }
    }

    /// Best-effort, atomic (tmp + rename); an empty set removes the file.
    fn save(&self, pins: BTreeMap<usize, Value>) {
        if pins.is_empty() {
            let _ = std::fs::remove_file(&self.path);
            return;
        }
        let Some(dir) = self.path.parent() else { return };
        let _ = std::fs::create_dir_all(dir);
        let file = PinFile { dir: self.project_dir.clone(), pins };
        let Ok(text) = serde_json::to_string(&file) else { return };
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tempdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mulpex-pins-test-{}", mulpex_core::persist::new_uuid()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn store(home: &Path, project: &str) -> PinStore {
        PinStore::beside(&home.join("sessions").join("proj-abc.txt"), Path::new(project))
    }

    #[test]
    fn set_load_retain_round_trip() {
        let home = tempdir();
        let s = store(home.as_path(), "/p");
        assert!(s.load().is_empty());

        s.set(3, Some(json!({"text": "a"})));
        s.set(7, Some(json!({"text": "b"})));
        assert!(home.as_path().join("pins/proj-abc.json").exists());
        assert_eq!(s.load().len(), 2);

        // A replaced pin overwrites; a cleared one is gone.
        s.set(3, Some(json!({"text": "c"})));
        assert_eq!(s.load()[&3], json!({"text": "c"}));
        s.set(7, None);
        assert_eq!(s.load().keys().copied().collect::<Vec<_>>(), vec![3]);

        // A closed instance's pin goes when the store forgets its row.
        s.retain(&HashSet::from([5]));
        assert!(s.load().is_empty());
        assert!(!home.as_path().join("pins/proj-abc.json").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn another_projects_file_is_ignored() {
        let home = tempdir();
        store(home.as_path(), "/a").set(1, Some(json!(1)));
        assert!(store(home.as_path(), "/b").load().is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }
}
