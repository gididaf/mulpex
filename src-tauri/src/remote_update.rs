//! An update ordered by another Mulpex driving this one (Remote Hosts).
//!
//! Remote Hosts needs both sides on the same version, so the Mac updates every
//! connected host *before* itself: it sends `{"t":"update","to":"<version>"}`
//! and this module does, headless, what a person at this screen would do with
//! the update banner's "Checkpoint & update" — ask every claude to stop at a
//! checkpoint, wait until they have, download and install, write a checkpoint
//! that wakes them by itself after the relaunch (`auto_wake`), and restart.
//!
//! Progress goes back to the ordering device as `update-progress` messages:
//! the relay loop (`remote/mod.rs`) sends the latest one whenever `seq` moves.
//! The order is accepted regardless of version: a host one release behind is
//! exactly the one that refuses the Mac's `hello`, and it still has to be
//! reachable here. Design: `docs/remote-hosts.md`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tauri_plugin_updater::UpdaterExt;

use crate::state::AppState;

/// How long everyone must stay stopped before the restart — the status and the
/// read inbox come from different writers, so a claude can look stopped for a
/// beat before its `working` lands (same reason as `checkpoint.ts`'s SETTLE_MS).
const SETTLE: Duration = Duration::from_millis(1500);
const POLL: Duration = Duration::from_secs(1);

struct Run {
    /// The paired device that ordered it; progress goes to its sessions.
    device: String,
    progress: Value,
    seq: u64,
    cancel: bool,
    /// Ended (error / cancelled). A new order then starts a fresh run.
    done: bool,
}

static RUN: Mutex<Option<Run>> = Mutex::new(None);

/// `update` from a paired device. A second order while one runs only re-sends
/// the current progress (to whichever device asked last).
pub fn order(app: AppHandle, device: &str, to: &str) {
    {
        let mut g = RUN.lock().unwrap();
        if let Some(r) = g.as_mut().filter(|r| !r.done) {
            r.device = device.to_string();
            r.seq += 1;
            return;
        }
        *g = Some(Run {
            device: device.to_string(),
            progress: json!({"t": "update-progress", "step": "checking", "to": to}),
            seq: 1,
            cancel: false,
            done: false,
        });
    }
    let to = to.to_string();
    std::thread::spawn(move || {
        if let Err(e) = drive(&app, &to) {
            set("error", json!({ "error": e }));
        }
    });
}

/// `update-cancel`: honoured until the download finishes installing.
pub fn cancel() {
    if let Some(r) = RUN.lock().unwrap().as_mut() {
        r.cancel = true;
    }
}

/// For the relay loop: the latest progress, its sequence number, and the device
/// it is for.
pub fn progress() -> Option<(u64, String, String)> {
    let g = RUN.lock().unwrap();
    g.as_ref().map(|r| (r.seq, r.device.clone(), r.progress.to_string()))
}

fn set(step: &str, extra: Value) {
    let mut g = RUN.lock().unwrap();
    let Some(r) = g.as_mut() else { return };
    let to = r.progress.get("to").cloned().unwrap_or(Value::Null);
    let mut p = json!({"t": "update-progress", "step": step, "to": to});
    if let (Some(p), Some(extra)) = (p.as_object_mut(), extra.as_object()) {
        p.extend(extra.clone());
    }
    if p == r.progress {
        return;
    }
    r.progress = p;
    r.seq += 1;
    r.done = matches!(step, "error" | "cancelled");
}

fn cancelled() -> bool {
    RUN.lock().unwrap().as_ref().is_some_and(|r| r.cancel)
}

fn drive(app: &AppHandle, to: &str) -> Result<(), String> {
    let mine = env!("CARGO_PKG_VERSION");
    let update = tauri::async_runtime::block_on(async {
        app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())
    })
    .map_err(|e| format!("update check failed: {e}"))?;
    let Some(update) = update else {
        return Err(format!("this Mulpex (v{mine}) finds no update to v{to}"));
    };
    if update.version != to {
        return Err(format!("this Mulpex is offered v{}, not v{to}", update.version));
    }

    let state = app.state::<AppState>();
    state.ws.lock().unwrap().checkpoint_stop();
    // From here on, any way out that does not restart must un-stop them.
    let undo = |why: String| {
        state.ws.lock().unwrap().checkpoint_cancel();
        why
    };
    let mut clear_since: Option<Instant> = None;
    loop {
        if cancelled() {
            state.ws.lock().unwrap().checkpoint_cancel();
            set("cancelled", json!({}));
            return Ok(());
        }
        let left = state.ws.lock().unwrap().checkpoint_not_stopped();
        let pending: Vec<Value> = left
            .iter()
            .map(|t| json!({"project": t.project, "id": t.id, "name": t.name}))
            .collect();
        set("stopping", json!({ "pending": pending }));
        if left.is_empty() {
            let since = *clear_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= SETTLE {
                break;
            }
        } else {
            clear_since = None;
        }
        std::thread::sleep(POLL);
    }

    set("downloading", json!({ "percent": 0 }));
    let mut total = 0u64;
    let mut got = 0u64;
    let mut last_pct = 0u64;
    let bytes = tauri::async_runtime::block_on(update.download(
        |chunk, len| {
            got += chunk as u64;
            total = len.unwrap_or(total);
            if total > 0 {
                let pct = got * 100 / total;
                if pct >= last_pct + 5 {
                    last_pct = pct;
                    set("downloading", json!({ "percent": pct }));
                }
            }
        },
        || {},
    ))
    .map_err(|e| undo(format!("download failed: {e}")))?;
    if cancelled() {
        state.ws.lock().unwrap().checkpoint_cancel();
        set("cancelled", json!({}));
        return Ok(());
    }
    set("installing", json!({}));
    update.install(bytes).map_err(|e| undo(format!("install failed: {e}")))?;
    state
        .ws
        .lock()
        .unwrap()
        .checkpoint_commit(true)
        .map_err(|e| undo(format!("couldn't save the checkpoint: {e}")))?;

    set("restarting", json!({}));
    // Let the relay loop send that before teardown takes the connection down.
    std::thread::sleep(Duration::from_millis(1500));
    app.request_restart();
    Ok(())
}
