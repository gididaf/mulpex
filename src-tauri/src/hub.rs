//! The backend heartbeat: a ~200ms poll that reaps dead sessions, tracks the
//! worked-on set, and emits the live hub snapshot to the frontend. Ports the
//! cadence of the old `App::run` loop (minus all the redraw bookkeeping).

use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

use crate::snapshot::HubSnapshot;
use crate::state::AppState;

/// Same cadence as the old `STATUS_POLL`. Most changes coincide with PTY output,
/// but the idle notification produces none, so we poll as a backstop.
const POLL: Duration = Duration::from_millis(200);

/// Spawn the poll loop. Runs for the life of the app on its own thread.
pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last: Option<HubSnapshot> = None;
        loop {
            std::thread::sleep(POLL);
            let state = app.state::<AppState>();
            let mut guard = state.core.lock().unwrap();
            let Some(core) = guard.as_mut() else {
                // No project open yet — nothing to poll.
                continue;
            };
            let removed = core.reap_dead();
            core.refresh_worked();
            let snap = core.hub_snapshot();
            let sessions = core.session_infos();
            drop(guard);

            for id in &removed {
                let _ = app.emit("session-exited", id);
            }
            if !removed.is_empty() {
                let _ = app.emit("sessions-changed", &sessions);
            }
            if last.as_ref() != Some(&snap) {
                let _ = app.emit("hub-update", &snap);
                last = Some(snap);
            }
        }
    });
}
