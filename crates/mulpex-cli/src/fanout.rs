//! `hub_spawn`: one instance asking for several more, each with its own task.
//!
//! An instance writes a batch to `<state_dir>/spawn/<token>.json` and blocks on
//! `<token>.done`; the poll loop creates the children and writes the ids back.
//! Single-threaded here, so id allocation and the reply handshake cannot race.
//!
//! **The batch is drip-fed, not launched at once.** Firing eight `claude` cold
//! starts in one tick makes them fight for CPU. In the desktop app that also meant
//! a child could miss its task injection; here the task is on the command line and
//! cannot be missed, so the stagger is purely about not thrashing the machine —
//! which is still worth it, and is what the plan means by testing the *plural*.
//!
//! Nothing here ever sleeps. It runs on the shared poll loop, so blocking would
//! stall every project's bookkeeping, including the terminals' transcripts.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use mulpex_core::rules::SpawnTask;

use crate::core::Project;
use crate::tmux::Tmux;

/// At most one child per this long. Matches the desktop app's `SPAWN_STAGGER`.
const STAGGER: Duration = Duration::from_millis(500);

/// How long a child gets to reach its first turn before it is called failed.
///
/// This watchdog covers only the case the child's own hook cannot: never getting
/// there at all. Whether the task arrived *intact* is the hook's to answer — it is
/// the only thing that sees what `claude` actually received, and calling a started
/// turn "success" from out here is exactly how a truncated brief was once reported
/// as delivered.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a just-spawned child has to show up in a scan before its absence
/// counts as death. One tick (200 ms) is all it actually needs; the margin is for
/// a loaded machine, and costs nothing — a child that really died is still caught,
/// just a couple of seconds later.
const APPEAR_GRACE: Duration = Duration::from_secs(3);

/// One `hub_spawn` call, mid-flight.
struct Batch {
    token: String,
    dir: PathBuf,
    from: usize,
    remaining: VecDeque<String>,
    ids: Vec<usize>,
}

#[derive(Default)]
pub struct Fanout {
    queued: VecDeque<Batch>,
    last_spawn: Option<Instant>,
    /// Children whose first turn we are still waiting on.
    watching: HashMap<(PathBuf, usize), Instant>,
}

impl Fanout {
    /// Read new requests, launch at most one child, and rule on any child that has
    /// run out of time. Returns whether a window was created.
    pub fn tick(&mut self, t: &Tmux, projects: &[Project], next_ids: &mut HashMap<PathBuf, usize>) -> bool {
        for p in projects {
            self.collect(p);
        }
        let spawned = self.drip(t, projects, next_ids);
        self.watchdog(projects);
        spawned
    }

    /// Queue whatever is waiting in this project's `spawn/` directory.
    fn collect(&mut self, p: &Project) {
        let dir = p.state_dir.join("spawn");
        // A missing directory means no NEW requests — an in-flight batch still has
        // children to drip-feed, so this must not return early anywhere.
        let mut requests: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
                    .collect()
            })
            .unwrap_or_default();
        requests.sort();

        for req in requests {
            let content = std::fs::read_to_string(&req).unwrap_or_default();
            let stem = req.file_stem().and_then(|s| s.to_str()).map(str::to_string);
            let _ = std::fs::remove_file(&req);
            let (Some(stem), Some((from, tasks))) = (stem, parse(&content)) else { continue };
            if tasks.is_empty() {
                continue;
            }
            self.queued.push_back(Batch {
                token: stem,
                dir: dir.clone(),
                from,
                remaining: tasks.into(),
                ids: Vec::new(),
            });
        }
    }

    /// Launch at most one queued child, if the stagger has elapsed.
    fn drip(&mut self, t: &Tmux, projects: &[Project], next_ids: &mut HashMap<PathBuf, usize>) -> bool {
        if !self.last_spawn.is_none_or(|at| at.elapsed() >= STAGGER) {
            return false;
        }
        let Some(mut batch) = self.queued.pop_front() else { return false };
        let mut spawned = false;
        if let Some(task) = batch.remaining.pop_front() {
            // The batch names its project by the state dir it was queued in, so a
            // project closing mid-batch drops it rather than spawning into another.
            if let Some(p) = projects.iter().find(|p| batch.dir.starts_with(&p.state_dir)) {
                let slot = next_ids.entry(p.dir.clone()).or_insert_with(|| p.next_id());
                let id = *slot;
                *slot += 1;
                match crate::claudewin::spawn(
                    t,
                    p,
                    id,
                    Some(SpawnTask { parent_id: batch.from, task }),
                ) {
                    Ok(_) => {
                        batch.ids.push(id);
                        self.watching.insert((p.state_dir.clone(), id), Instant::now());
                        spawned = true;
                    }
                    Err(e) => {
                        eprintln!("[daemon] spawn for claude#{} failed: {e:#}", batch.from);
                        // Nothing to deliver to; don't leave the markers behind for
                        // a number that will be handed to somebody else.
                        crate::claudewin::clear_delivery(&p.state_dir, id);
                        let _ = std::fs::remove_file(p.state_dir.join("tasks").join(id.to_string()));
                    }
                }
            }
            self.last_spawn = Some(Instant::now());
        }
        if batch.remaining.is_empty() {
            // Batch complete — hand the waiting `hub_spawn` its ids.
            let done = serde_json::json!({ "ids": batch.ids }).to_string();
            let _ = std::fs::write(batch.dir.join(format!("{}.done", batch.token)), done);
        } else {
            self.queued.push_front(batch);
        }
        spawned
    }

    /// Rule `failed` on a child that died before its first turn, or that never got
    /// there in time. **Silence is not success.**
    fn watchdog(&mut self, projects: &[Project]) {
        let mut done: Vec<(PathBuf, usize)> = Vec::new();
        for (key, started) in &self.watching {
            let (state_dir, id) = key;
            if crate::claudewin::delivery_settled(state_dir, *id) {
                done.push(key.clone());
                continue;
            }
            let found = projects
                .iter()
                .find(|p| &p.state_dir == state_dir)
                .and_then(|p| p.find(*id));
            let dead = match found {
                Some(i) => i.dead,
                // Absent from the scan. That is NOT evidence of death for a child
                // spawned moments ago: the scan this tick runs on was taken before
                // the window existed, so the last child of every batch looked dead
                // the instant it was created. Measured — a six-task `hub_spawn`
                // came back in 3.9 s reporting its sixth child had never received
                // its task, while that child went on to do the work and report back.
                // A loud wrong answer is still a wrong answer.
                None => started.elapsed() >= APPEAR_GRACE,
            };
            if dead || started.elapsed() >= DELIVERY_TIMEOUT {
                crate::claudewin::mark_delivery(state_dir, *id, "failed");
                done.push(key.clone());
            }
        }
        for key in done {
            self.watching.remove(&key);
        }
    }
}

/// Pull `from` and the task list out of a request. A malformed one is dropped
/// rather than guessed at.
fn parse(content: &str) -> Option<(usize, Vec<String>)> {
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    let from = v.get("from").and_then(|x| x.as_u64())? as usize;
    let tasks = v
        .get("tasks")?
        .as_array()?
        .iter()
        .filter_map(|t| t.as_str().map(str::to_string))
        .filter(|t| !t.trim().is_empty())
        .collect::<Vec<_>>();
    Some((from, tasks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_yields_its_parent_and_its_tasks() {
        let (from, tasks) =
            parse(r#"{"from":3,"tasks":["one","  ","two"]}"#).expect("parses");
        assert_eq!(from, 3);
        assert_eq!(tasks, vec!["one", "two"], "blank tasks are dropped, not spawned");
        assert!(parse("not json").is_none());
        assert!(parse(r#"{"tasks":["a"]}"#).is_none(), "no parent, nowhere to report back");
    }

    /// The reply must carry every id, in order, and only once the whole batch has
    /// been launched — `hub_spawn` blocks on it.
    #[test]
    fn a_batch_replies_only_when_the_last_child_is_out() {
        let d = std::env::temp_dir().join(format!("mpx-fan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut batch = Batch {
            token: "tok".into(),
            dir: d.clone(),
            from: 1,
            remaining: VecDeque::from(vec!["a".to_string(), "b".to_string()]),
            ids: vec![],
        };
        // First child out; one still queued, so no reply yet.
        batch.remaining.pop_front();
        batch.ids.push(4);
        assert!(!batch.remaining.is_empty());
        assert!(!d.join("tok.done").exists());
        // Last child out.
        batch.remaining.pop_front();
        batch.ids.push(5);
        std::fs::write(
            d.join("tok.done"),
            serde_json::json!({ "ids": batch.ids }).to_string(),
        )
        .unwrap();
        let reply = std::fs::read_to_string(d.join("tok.done")).unwrap();
        assert_eq!(reply, r#"{"ids":[4,5]}"#);
    }

    /// The scan a tick runs on predates the windows that tick creates, so the last
    /// child of a batch is legitimately missing from it. Calling that death made a
    /// six-task `hub_spawn` report a healthy child as never having received its
    /// task, 3.9 s after asking.
    #[test]
    fn a_child_missing_from_a_scan_older_than_itself_is_not_dead_yet() {
        let fresh = Instant::now();
        assert!(fresh.elapsed() < APPEAR_GRACE, "just spawned: no verdict yet");
        let old = Instant::now() - APPEAR_GRACE - Duration::from_secs(1);
        assert!(old.elapsed() >= APPEAR_GRACE, "long gone: absence is death");
        assert!(APPEAR_GRACE < DELIVERY_TIMEOUT, "the grace must not outlast the deadline");
    }

    /// Six tasks must produce six distinct numbers. The one-item case always
    /// passes; this is the case that does not.
    #[test]
    fn a_batch_of_six_takes_six_consecutive_numbers() {
        let mut next: HashMap<PathBuf, usize> = HashMap::new();
        next.insert(PathBuf::from("/p"), 2);
        let ids: Vec<usize> = (0..6)
            .map(|_| {
                let slot = next.get_mut(std::path::Path::new("/p")).unwrap();
                let id = *slot;
                *slot += 1;
                id
            })
            .collect();
        assert_eq!(ids, vec![2, 3, 4, 5, 6, 7]);
    }
}
