//! The `#[tauri::command]` surface the Svelte frontend invokes. Thin wrappers
//! over `Core`; all mutation goes through the `AppState` mutex.

use tauri::ipc::Channel;
use tauri::State;

use crate::project;
use crate::snapshot::{BootstrapInfo, HubSnapshot, SessionInfo};
use crate::state::{AppState, Core};

/// Whether a project is already open (so the frontend can skip the picker).
#[tauri::command]
pub fn current_project(state: State<AppState>) -> Option<BootstrapInfo> {
    state.core.lock().unwrap().as_ref().map(Core::bootstrap_info)
}

/// Recent project dirs, most-recent first.
#[tauri::command]
pub fn list_recent_projects() -> Vec<String> {
    project::list_recent()
}

/// Open `path` as the project: initialize `Core` (spawning restored/fresh
/// sessions), record it in recents, and return the session list to build xterms
/// for. Errors if a project is already open (one project per window in v1) or the
/// path can't be initialized.
#[tauri::command]
pub fn open_project(state: State<AppState>, path: String) -> Result<BootstrapInfo, String> {
    let mut guard = state.core.lock().unwrap();
    if guard.is_some() {
        return Err("a project is already open in this window".into());
    }
    let dir = std::path::PathBuf::from(&path);
    if !dir.is_dir() {
        return Err(format!("not a directory: {path}"));
    }
    let core = Core::open(dir, &state.helper_path).map_err(|e| e.to_string())?;
    let info = core.bootstrap_info();
    *guard = Some(core);
    drop(guard);
    project::add_recent(&path);
    Ok(info)
}

/// Bind a session's frontend terminal channel: flush any pre-attach output, then
/// stream live PTY bytes (base64) to xterm. Called once per session after its
/// xterm mounts.
#[tauri::command]
pub fn attach_session(state: State<AppState>, id: usize, channel: Channel<String>) {
    if let Some(core) = state.core.lock().unwrap().as_mut() {
        if let Some(session) = core.session_mut(id) {
            session.attach(channel);
        }
    }
}

/// Spawn a fresh Claude instance (⌘T) and return its info.
#[tauri::command]
pub fn create_session(state: State<AppState>) -> Result<SessionInfo, String> {
    let mut guard = state.core.lock().unwrap();
    let core = guard.as_mut().ok_or("no project open")?;
    core.spawn_instance().map_err(|e| e.to_string())
}

/// Close an instance (⌘W) — kills its process group and reaps.
#[tauri::command]
pub fn close_session(state: State<AppState>, id: usize) {
    if let Some(core) = state.core.lock().unwrap().as_mut() {
        core.close(id);
    }
}

/// Rename an instance (⌘R). Empty name clears it (auto task line returns).
#[tauri::command]
pub fn rename_session(state: State<AppState>, id: usize, name: String) {
    if let Some(core) = state.core.lock().unwrap().as_mut() {
        core.rename(id, &name);
    }
}

/// Forward raw bytes to a session's PTY (from xterm `onData`).
#[tauri::command]
pub fn send_bytes(state: State<AppState>, id: usize, data: Vec<u8>) {
    if let Some(core) = state.core.lock().unwrap().as_mut() {
        if let Some(session) = core.session_mut(id) {
            session.send(&data);
        }
    }
}

/// Resize all sessions to the shared center-pane geometry (from the fit addon).
#[tauri::command]
pub fn resize_session(state: State<AppState>, cols: u16, rows: u16) {
    if let Some(core) = state.core.lock().unwrap().as_mut() {
        core.resize_all(cols, rows);
    }
}

/// Set the focused instance (⌘1–9 / ⌘[ ⌘] / sidebar click).
#[tauri::command]
pub fn focus_session(state: State<AppState>, id: usize) {
    if let Some(core) = state.core.lock().unwrap().as_mut() {
        core.set_active(id);
    }
}

/// The current hub snapshot for the initial paint (thereafter pushed via the
/// `hub-update` event from the poll loop).
#[tauri::command]
pub fn get_hub_snapshot(state: State<AppState>) -> Option<HubSnapshot> {
    state.core.lock().unwrap().as_ref().map(Core::hub_snapshot)
}
