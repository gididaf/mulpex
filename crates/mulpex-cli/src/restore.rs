//! Bringing a project's claudes back.
//!
//! tmux keeps a session alive across a detach, an ssh drop and a closed laptop —
//! which covers most of what "restore" means elsewhere. What it does not survive
//! is the tmux server itself going away: a reboot, an `mpx down`, a `kill-server`.
//! Before this module, that lost every conversation with no way back, because the
//! `--resume` uuid lived only on the window that had just been destroyed.
//!
//! So the uuids are written to `mulpex_core::persist::SessionStore` — the same
//! store, in the same format, the desktop app has always used, under **the CLI's
//! own home** (`SessionStore::in_home`, not `new`; see `statedir.rs` for why
//! sharing it would be silent corruption).
//!
//! Two rules decide what is in the store, and they are the app's:
//!
//! - **Only a claude that has actually worked**, tested by `state_dir/<id>` — the
//!   file its hook writes on its first turn. An instance opened and never spoken
//!   to has no conversation, and restoring one would resurrect a blank window
//!   every time.
//! - **...or one that was restored and has not yet spoken.** That exception is
//!   load-bearing, not tidiness: a fresh state dir makes every restored claude
//!   look unworked, so without it the first tick after a restore would rewrite the
//!   store empty and delete the very conversations it had just brought back.
//!   `persist_sessions` solves this with an in-memory `sticky` list; here the flag
//!   lives on the tmux window, so it also survives the daemon dying.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use mulpex_core::persist::{SavedSession, SessionStore};

use crate::core::Project;
use crate::tmux::Tmux;

/// This project's store, under the CLI's home rather than the desktop app's.
pub fn store(project_dir: &Path) -> SessionStore {
    SessionStore::in_home(&crate::statedir::cli_home(), project_dir)
}

/// What was saved for this project, in row order.
pub fn load(project_dir: &Path) -> Vec<SavedSession> {
    store(project_dir).load()
}

/// Keeps the last thing written per project, so the 200 ms tick only touches the
/// disk when the set of conversations actually changes.
#[derive(Default)]
pub struct Saver {
    last: HashMap<PathBuf, String>,
}

impl Saver {
    pub fn tick(&mut self, projects: &[Project]) {
        for p in projects {
            self.save(p);
        }
    }

    pub fn save(&mut self, p: &Project) {
        let records = records(p);
        // Cheap identity for "has anything changed", so an idle project costs one
        // string compare per tick rather than a write.
        let fingerprint = records
            .iter()
            .map(|s| format!("{}|{}|{}|{:?}", s.session_id, s.muted, s.id.unwrap_or(0), s.name))
            .collect::<Vec<_>>()
            .join("\n");
        if self.last.get(&p.dir) == Some(&fingerprint) {
            return;
        }
        store(&p.dir).save(&records);
        self.last.insert(p.dir.clone(), fingerprint);
    }
}

/// Save a project's conversations right now, outside the loop.
///
/// `mpx down` calls this before it kills anything. The daemon's last write is
/// almost always current, but "almost" here means a claude created and spoken to
/// inside the same 200 ms tick as the teardown, whose conversation would then be
/// gone with nothing saying so. One extra write removes the race entirely.
pub fn save_now(p: &Project) {
    Saver::default().save(p);
}

/// The rows worth persisting, in id order.
fn records(p: &Project) -> Vec<SavedSession> {
    p.instances
        .iter()
        .filter(|i| i.is_claude() && !i.session_id.is_empty())
        .filter(|i| i.restored || worked(&p.state_dir, i.id))
        .map(|i| SavedSession {
            session_id: i.session_id.clone(),
            name: clean_name(&i.window_name, i.id),
            muted: i.muted,
            id: Some(i.id),
        })
        .collect()
}

/// Has this instance taken a turn? The status file its hook writes on the first
/// one — the same test `restart` makes before offering `--resume`.
fn worked(state_dir: &Path, id: usize) -> bool {
    state_dir.join(id.to_string()).exists()
}

/// The name worth saving, or `None` for one that never had a custom name.
///
/// A window that died wears its epitaph in its name (`✗ claude#3 (exit 1)`), and
/// that is a fact about the *process*, not the instance. Saved literally, a failed
/// restore would come back permanently called `✗ claude#3 (exit 1)` — and would
/// still be called that after succeeding, since nothing ever renames it again.
fn clean_name(window_name: &str, id: usize) -> Option<String> {
    let trimmed = window_name.trim();
    let mut s = trimmed;
    // Only a name `mark_dead` actually wrote, which is `✗ <name>[ (exit <n>)]` —
    // both halves, and the code has to be digits. Matching loosely mangles a real
    // name that merely ends in a parenthesis, which the test below is about: an
    // instance called `ports (exit codes)` is not a dead one.
    if let Some(rest) = trimmed.strip_prefix('✗') {
        s = rest.trim_start();
        if let Some(cut) = s.rfind(" (exit ") {
            let tail = &s[cut + " (exit ".len()..];
            if let Some(code) = tail.strip_suffix(')') {
                if !code.is_empty() && code.chars().all(|c| c.is_ascii_digit()) {
                    s = &s[..cut];
                }
            }
        }
    }
    let s = s.trim();
    if s.is_empty() || s == format!("claude#{id}") {
        return None;
    }
    Some(s.to_string())
}

/// Give every saved record the number it had.
///
/// A number is an identity here, not a position: `claude#7` is how the user refers
/// to that conversation, how a peer addresses it, and how they recognise it in the
/// sidebar. Numbering `1..n` on restore would silently rename every conversation
/// in the project. Records from before ids were persisted (`None`), or with a
/// number already taken, fall back to the lowest free one.
pub fn assign_ids(saved: Vec<SavedSession>) -> Vec<(usize, SavedSession)> {
    let mut used: HashSet<usize> = HashSet::new();
    let mut next_free = 1usize;
    let mut out = Vec::with_capacity(saved.len());
    for record in saved {
        let id = match record.id {
            Some(n) if n >= 1 && !used.contains(&n) => n,
            _ => {
                while used.contains(&next_free) {
                    next_free += 1;
                }
                next_free
            }
        };
        used.insert(id);
        out.push((id, record));
    }
    out
}

/// Rebuild a freshly created project's windows from its store. Returns how many
/// came back.
///
/// A spawn that fails is logged and skipped rather than aborting the rest: one
/// unresumable conversation must not cost you the other five, and its record stays
/// in the store for the next attempt.
pub fn spawn_all(t: &Tmux, p: &Project) -> usize {
    let mut n = 0;
    for (id, record) in assign_ids(load(&p.dir)) {
        match crate::claudewin::spawn_restored(t, p, &record, id) {
            Ok(_) => n += 1,
            Err(e) => eprintln!("mpx: could not restore claude#{id}: {e:#}"),
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: Option<usize>) -> SavedSession {
        SavedSession { session_id: format!("u{id:?}"), name: None, muted: false, id }
    }

    /// The numbers are the identities. 2/3/15 must come back as 2/3/15 — the app
    /// learned this the expensive way, and the CLI shares the store format, so it
    /// has to share the rule.
    #[test]
    fn a_restore_hands_back_the_numbers_the_user_knows() {
        let ids: Vec<usize> = assign_ids(vec![rec(Some(2)), rec(Some(3)), rec(Some(15))])
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec![2, 3, 15]);
    }

    /// A store written before ids existed, and one hand-edited into a collision,
    /// both still have to open the project rather than fail it.
    #[test]
    fn a_record_with_no_number_or_a_taken_one_gets_the_lowest_free() {
        let ids: Vec<usize> = assign_ids(vec![rec(Some(3)), rec(None), rec(Some(3)), rec(None)])
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec![3, 1, 2, 4]);
    }

    /// An epitaph is a fact about a process that has exited, not a name for the
    /// conversation. Saved as one it would outlive the failure that produced it.
    #[test]
    fn a_dead_windows_name_is_not_saved_as_the_instances_name() {
        assert_eq!(clean_name("✗ claude#3 (exit 1)", 3), None);
        assert_eq!(clean_name("✗ claude#3 ▸ fix the parser (exit 1)", 3).as_deref(), Some("claude#3 ▸ fix the parser"));
        assert_eq!(clean_name("claude#3", 3), None, "the default name is not a custom one");
        assert_eq!(clean_name("hub work", 3).as_deref(), Some("hub work"));
        // A real name that merely looks like an epitaph keeps every character. Both
        // halves of the pattern have to match, and the code has to be a number.
        assert_eq!(clean_name("ports (exit codes)", 3).as_deref(), Some("ports (exit codes)"));
        assert_eq!(clean_name("✗ ports (exit codes)", 3).as_deref(), Some("ports (exit codes)"));
        assert_eq!(clean_name("✗ ports (exit 3)", 3).as_deref(), Some("ports"));
    }

    /// The CLI's store must not be the desktop app's — two frontends handing the
    /// same `--resume` uuid to two claudes is silent conversation corruption, and
    /// `SessionStore::new` would do exactly that because `mpx` never sets
    /// `MULPEX_HOME`.
    #[test]
    fn the_store_lives_under_the_clis_own_home() {
        std::env::remove_var("MULPEX_HOME");
        let path = store(Path::new("/tmp/some-project")).path().to_path_buf();
        assert!(path.starts_with(crate::statedir::cli_home()), "got {}", path.display());
        assert_ne!(path, SessionStore::new(Path::new("/tmp/some-project")).path());
    }
}
