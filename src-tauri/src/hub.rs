//! The backend heartbeat: a ~200ms poll that, for EVERY open project, reaps dead
//! sessions, fulfils hub_spawn requests, tracks the worked-on set, and emits that
//! project's live hub snapshot to the frontend. Ports the cadence of the old
//! `App::run` loop (minus all the redraw bookkeeping), now fanned out per project.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

use crate::snapshot::{HubSnapshot, HubUpdate, ProjectHandle, SessionExited, SessionsChanged};
use crate::state::AppState;

/// Same cadence as the old `STATUS_POLL`. Most changes coincide with PTY output,
/// but the idle notification produces none, so we poll as a backstop.
const POLL: Duration = Duration::from_millis(200);

/// Spawn the poll loop. Runs for the life of the app on its own thread.
pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        // Last-emitted snapshot per project, so we only push on change.
        let mut last: HashMap<ProjectHandle, HubSnapshot> = HashMap::new();
        loop {
            std::thread::sleep(POLL);
            let state = app.state::<AppState>();
            let mut ws = state.ws.lock().unwrap();

            // Collect everything under the lock, then drop it before emitting.
            let mut batch: Vec<(ProjectHandle, Vec<usize>, bool, HubSnapshot, _)> = Vec::new();
            let mut live: HashSet<ProjectHandle> = HashSet::new();
            for core in &mut ws.projects {
                live.insert(core.handle);
                let removed = core.reap_dead();
                // Fulfil any hub_spawn requests this project's instances left on disk
                // (creates task-seeded siblings). New sessions must be published so
                // the frontend builds their xterms, same as a reap republishes after
                // removals.
                let spawned = core.process_spawn_requests();
                core.refresh_worked();
                batch.push((
                    core.handle,
                    removed,
                    spawned,
                    core.hub_snapshot(),
                    core.session_infos(),
                ));
            }
            drop(ws);

            for (handle, removed, spawned, snap, sessions) in batch {
                for id in &removed {
                    let _ = app.emit("session-exited", SessionExited { handle, id: *id });
                }
                if !removed.is_empty() || spawned {
                    let _ = app.emit("sessions-changed", SessionsChanged { handle, sessions });
                }
                if last.get(&handle) != Some(&snap) {
                    let _ = app.emit(
                        "hub-update",
                        HubUpdate {
                            handle,
                            snapshot: snap.clone(),
                        },
                    );
                    last.insert(handle, snap);
                }
            }
            // Forget closed projects so their handles don't linger in the map.
            last.retain(|h, _| live.contains(h));
        }
    });
}
