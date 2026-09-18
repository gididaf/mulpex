//! The backend heartbeat: a ~200ms poll that, for EVERY open project, reaps dead
//! sessions, fulfils hub_spawn requests, tracks the worked-on set, and emits that
//! project's live hub snapshot to the frontend. Ports the cadence of the old
//! `App::run` loop (minus all the redraw bookkeeping), now fanned out per project.
//!
//! It also publishes the **workspace registry** (`mulpex_core::registry`), which
//! is what makes cross-project messaging possible: this loop is the only context
//! that holds every `Core` at once, so it is the only thing that can say what the
//! other projects are and where their hubs live on disk.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use mulpex_core::registry::Registry;
use tauri::{AppHandle, Emitter, Manager};

use crate::explainer;
use crate::snapshot::{
    HubSnapshot, HubUpdate, ProjectHandle, SessionExited, SessionInfo, SessionsChanged,
};
use crate::state::AppState;

/// Same cadence as the old `STATUS_POLL`. Most changes coincide with PTY output,
/// but the idle notification produces none, so we poll as a backstop.
const POLL: Duration = Duration::from_millis(200);

/// How often to sweep the process table for hub listeners.
///
/// Launch and teardown are not enough. An instance that arms one *while Mulpex is
/// running* — which `warweb#75` did every 30 minutes all night, out of 141 arm
/// calls in its own history and against rules telling it not to — would otherwise
/// keep waking itself until the user happened to restart the app. That is the
/// whole failure this reaper now exists to end, so it has to run while the app is
/// up, not only at its edges.
///
/// A minute, not a tick: the sweep walks every process on the machine and reads
/// each one's argv, which is far too expensive at 200 ms and pointless at any
/// speed — the cost of catching a stray listener a minute late is one wake-up that
/// was going to happen anyway.
const LISTENER_SWEEP: Duration = Duration::from_secs(60);

/// Spawn the poll loop. Runs for the life of the app on its own thread.
pub fn start(app: AppHandle) {
    // The Explainer's Sonnet workers: fed below from `take_explain_requests`,
    // emitting `explain-update` on their own once a summary lands.
    explainer::init(app.clone());
    std::thread::spawn(move || {
        // Last-emitted snapshot per project, so we only push on change.
        let mut last: HashMap<ProjectHandle, HubSnapshot> = HashMap::new();
        // Last-emitted session list per project, likewise. Diffing this — rather
        // than emitting only on add/remove — is what publishes a *change of state*
        // in an existing row, which is how a terminal's shell exiting reaches the
        // sidebar. It also closes a standing gap: a rename or a mute changed the
        // backend and emitted nothing.
        let mut last_sessions: HashMap<ProjectHandle, Vec<SessionInfo>> = HashMap::new();
        // Deliberately in the past, so the first sweep runs on the first tick
        // rather than a minute in.
        let mut last_sweep = std::time::Instant::now() - LISTENER_SWEEP;
        loop {
            std::thread::sleep(POLL);
            if last_sweep.elapsed() >= LISTENER_SWEEP {
                last_sweep = std::time::Instant::now();
                // Outside the workspace lock: it is a process-table walk that
                // touches no Mulpex state, and holding the lock across it would
                // stall every session command for its duration.
                let reaped = crate::pty::reap_orphaned_listeners();
                if reaped > 0 {
                    eprintln!("mulpex: reaped {reaped} hub listener(s)");
                }
            }
            let state = app.state::<AppState>();
            let mut ws = state.ws.lock().unwrap();

            // Collect everything under the lock, then drop it before emitting.
            let mut batch: Vec<(ProjectHandle, Vec<usize>, HubSnapshot, Vec<SessionInfo>)> =
                Vec::new();
            // What every project publishes about itself to the others. This loop
            // is the only context that holds every `Core` at once, so it is the
            // only thing that can describe the workspace honestly.
            let mut reg = Registry::default();
            let state_root = ws.state_root.clone();
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
                // ⌘M is undone by the user simply talking to the row again. Same
                // shape as the name requests above: an empty dir read per tick,
                // and the session diff below is what redraws the sidebar.
                core.process_user_prompts();
                // The idle wake: type a doorbell into any instance holding unread
                // mail. This is the ONLY thing that can start a turn in an idle
                // claude — `hub_send` just writes a file, and no hook reaches a
                // session that is sitting at its prompt. It must run every tick,
                // for the same reason `process_remote_signals` does.
                core.ring_doorbells();
                // Remote claudes calling their driver back. Cheap when there are
                // none (one empty dir read) and it must run every tick: this is
                // the only path by which a machine on the other end of an ssh
                // link can reach a local instance at all.
                core.process_remote_signals();
                // Turns the hooks handed to the Explainer (a transcript path per
                // finished turn or pending dialog) → the Sonnet worker queue.
                // Cheap here (one dir read + a queue push); the summarizing
                // happens off-thread.
                for (id, body) in core.take_explain_requests() {
                    explainer::submit(core.handle, id, body, core.state_dir.clone());
                }
                core.refresh_worked();
                // A shell can exit at any moment with nothing else happening;
                // this is what stops the manifest instances read from going on
                // advertising it as running. Writes only on change.
                core.sync_terminal_index();
                let snap = core.hub_snapshot();
                // An instance that ended a turn still unnamed gets a provisional
                // label from its own task, so the row stops showing the user's
                // raw prompt. In-memory only, and superseded by the instance's
                // own `hub_set_name` — it must run *before* the two readers
                // below, which are what publish the label.
                core.apply_fallback_names(&snap);
                reg.projects.push(core.registry_entry(&snap));
                batch.push((core.handle, removed, snap, core.session_infos()));
            }
            drop(ws);

            // Cross-project addressing rests on this file. Written outside the
            // lock and only when the bytes change, so a quiet tick costs one
            // string compare — and a closed project simply stops appearing.
            Registry::write_if_changed(&state_root, &reg);

            for (handle, removed, snap, sessions) in batch {
                for id in &removed {
                    // A reaped instance's row is gone, so its feed is unreachable.
                    explainer::forget(handle, *id);
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
