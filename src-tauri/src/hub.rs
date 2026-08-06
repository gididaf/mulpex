//! The backend heartbeat: a ~200ms poll that, for EVERY open project, reaps dead
//! sessions, fulfils hub_spawn requests, tracks the worked-on set, and emits that
//! project's live hub snapshot to the frontend. Ports the cadence of the old
//! `App::run` loop (minus all the redraw bookkeeping), now fanned out per project.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

use crate::snapshot::{
    HubSnapshot, HubUpdate, ProjectHandle, SessionExited, SessionInfo, SessionsChanged,
};
use crate::state::AppState;

/// Same cadence as the old `STATUS_POLL`. Most changes coincide with PTY output,
/// but the idle notification produces none, so we poll as a backstop.
const POLL: Duration = Duration::from_millis(200);

/// Spawn the poll loop. Runs for the life of the app on its own thread.
pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        // Last-emitted snapshot per project, so we only push on change.
        let mut last: HashMap<ProjectHandle, HubSnapshot> = HashMap::new();
        // Last-emitted session list per project, likewise. Diffing this — rather
        // than emitting only on add/remove — is what publishes a *change of state*
        // in an existing row, which is how a terminal's shell exiting reaches the
        // sidebar. It also closes a standing gap: a rename or a mute changed the
        // backend and emitted nothing.
        let mut last_sessions: HashMap<ProjectHandle, Vec<SessionInfo>> = HashMap::new();
        loop {
            std::thread::sleep(POLL);
            let state = app.state::<AppState>();
            let mut ws = state.ws.lock().unwrap();

            // Collect everything under the lock, then drop it before emitting.
            let mut batch: Vec<(ProjectHandle, Vec<usize>, HubSnapshot, Vec<SessionInfo>)> =
                Vec::new();
            let mut live: HashSet<ProjectHandle> = HashSet::new();
            for core in &mut ws.projects {
                live.insert(core.handle);
                let removed = core.reap_dead();
                // Fulfil the requests this project's instances left on disk:
                // hub_spawn (task-seeded siblings) and the terminal ops
                // (open/send/close). Both can change the session list, which the
                // diff below publishes.
                core.process_spawn_requests();
                core.process_terminal_requests();
                // hub_set_name: an instance labelling its own sidebar row. Cheap
                // (an empty dir read) and it changes `session_infos`, so the diff
                // below is what actually repaints the row.
                core.process_name_requests();
                // Remote claudes calling their driver back. Cheap when there are
                // none (one empty dir read) and it must run every tick: this is
                // the only path by which a machine on the other end of an ssh
                // link can reach a local instance at all.
                core.process_remote_signals();
                core.refresh_worked();
                // A shell can exit at any moment with nothing else happening;
                // this is what stops the manifest instances read from going on
                // advertising it as running. Writes only on change.
                core.sync_terminal_index();
                batch.push((
                    core.handle,
                    removed,
                    core.hub_snapshot(),
                    core.session_infos(),
                ));
            }
            drop(ws);

            for (handle, removed, snap, sessions) in batch {
                for id in &removed {
                    let _ = app.emit("session-exited", SessionExited { handle, id: *id });
                }
                if last_sessions.get(&handle) != Some(&sessions) {
                    let _ = app.emit(
                        "sessions-changed",
                        SessionsChanged {
                            handle,
                            sessions: sessions.clone(),
                        },
                    );
                    last_sessions.insert(handle, sessions);
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
