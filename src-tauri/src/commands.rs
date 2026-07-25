//! The `#[tauri::command]` surface the Svelte frontend invokes. Thin wrappers over
//! the `Workspace`; all mutation goes through the `AppState` mutex. Every
//! session-scoped command carries a `project_handle` (JS `projectHandle`) so
//! per-project instance ids (each project numbers 1,2,3…) never collide.

use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, State};

use crate::project;
use crate::snapshot::{BootstrapInfo, HubSnapshot, ProjectHandle, SessionInfo, WorkspaceInfo};
use crate::state::{AppState, Core};

/// Every open project + the active handle, for the frontend's initial paint.
/// Projects are already restored (spawned) during app `setup`. Replaces
/// `current_project`.
#[tauri::command]
pub fn bootstrap(state: State<AppState>) -> WorkspaceInfo {
    state.ws.lock().unwrap().workspace_info()
}

/// Recent project dirs, most-recent first.
#[tauri::command]
pub fn list_recent_projects() -> Vec<String> {
    project::list_recent()
}

/// Open `path` as a project (or re-activate it if already open), record it in
/// recents + the open-set, and return its session list to build xterms for. Emits
/// `projects-changed` so the tab bar updates.
#[tauri::command]
pub fn open_project(
    state: State<AppState>,
    app: AppHandle,
    path: String,
) -> Result<BootstrapInfo, String> {
    let mut ws = state.ws.lock().unwrap();
    let (handle, _newly) = ws
        .open_or_focus(&path, &state.helper_path)
        .map_err(|e| e.to_string())?;
    let info = ws.project(handle).unwrap().bootstrap_info();
    ws.persist_open();
    let wsinfo = ws.workspace_info();
    drop(ws);
    project::add_recent(&path);
    let _ = app.emit("projects-changed", wsinfo);
    Ok(info)
}

/// Close a project: kill its sessions, remove its scratch dir, drop it from the
/// open-set, and re-pick the active project. Emits `projects-changed` and returns
/// the new workspace.
#[tauri::command]
pub fn close_project(
    state: State<AppState>,
    app: AppHandle,
    project_handle: ProjectHandle,
) -> WorkspaceInfo {
    let mut ws = state.ws.lock().unwrap();
    ws.close_project(project_handle);
    ws.persist_open();
    let wsinfo = ws.workspace_info();
    drop(ws);
    let _ = app.emit("projects-changed", wsinfo.clone());
    wsinfo
}

/// Make `project_handle` the active project (tab click / ⌘P / project cycling).
#[tauri::command]
pub fn switch_project(state: State<AppState>, project_handle: ProjectHandle) {
    let mut ws = state.ws.lock().unwrap();
    if ws.project(project_handle).is_some() {
        ws.active = Some(project_handle);
    }
}

/// Bind a session's frontend terminal channel: flush any pre-attach output, then
/// stream live PTY bytes (base64) to xterm. Called once per session after its
/// xterm mounts.
#[tauri::command]
pub fn attach_session(
    state: State<AppState>,
    project_handle: ProjectHandle,
    id: usize,
    channel: Channel<String>,
) {
    if let Some(core) = state.ws.lock().unwrap().project_mut(project_handle) {
        if let Some(session) = core.session_mut(id) {
            session.attach(channel);
        }
    }
}

/// Spawn a fresh Claude instance (⌘T) in the given project and return its info.
#[tauri::command]
pub fn create_session(
    state: State<AppState>,
    project_handle: ProjectHandle,
) -> Result<SessionInfo, String> {
    let mut ws = state.ws.lock().unwrap();
    let core = ws.project_mut(project_handle).ok_or("no such project")?;
    core.spawn_instance().map_err(|e| e.to_string())
}

/// Close an instance (⌘W) — kills its process group and reaps.
#[tauri::command]
pub fn close_session(state: State<AppState>, project_handle: ProjectHandle, id: usize) {
    if let Some(core) = state.ws.lock().unwrap().project_mut(project_handle) {
        core.close(id);
    }
}

/// Rename an instance (⌘R). Empty name clears it (auto task line returns).
#[tauri::command]
pub fn rename_session(
    state: State<AppState>,
    project_handle: ProjectHandle,
    id: usize,
    name: String,
) {
    if let Some(core) = state.ws.lock().unwrap().project_mut(project_handle) {
        core.rename(id, &name);
    }
}

/// Forward raw bytes to a session's PTY (from xterm `onData`).
#[tauri::command]
pub fn send_bytes(
    state: State<AppState>,
    project_handle: ProjectHandle,
    id: usize,
    data: Vec<u8>,
) {
    if let Some(core) = state.ws.lock().unwrap().project_mut(project_handle) {
        if let Some(session) = core.session_mut(id) {
            session.send(&data);
        }
    }
}

/// Resize all of a project's sessions to the shared center-pane geometry. The
/// frontend calls this per open project so background projects' PTYs aren't left
/// at spawn size (garbled on switch).
#[tauri::command]
pub fn resize_session(
    state: State<AppState>,
    project_handle: ProjectHandle,
    cols: u16,
    rows: u16,
) {
    if let Some(core) = state.ws.lock().unwrap().project_mut(project_handle) {
        core.resize_all(cols, rows);
    }
}

/// Set the focused instance (⌘1–9 / ⌘[ ⌘] / sidebar click); also makes its project
/// active.
#[tauri::command]
pub fn focus_session(state: State<AppState>, project_handle: ProjectHandle, id: usize) {
    let mut ws = state.ws.lock().unwrap();
    if ws.project(project_handle).is_some() {
        ws.active = Some(project_handle);
        if let Some(core) = ws.project_mut(project_handle) {
            core.set_active(id);
        }
    }
}

/// A project's current hub snapshot for the initial HubPanel paint (thereafter
/// pushed via the scoped `hub-update` event from the poll loop).
#[tauri::command]
pub fn get_hub_snapshot(
    state: State<AppState>,
    project_handle: ProjectHandle,
) -> Option<HubSnapshot> {
    state
        .ws
        .lock()
        .unwrap()
        .project(project_handle)
        .map(Core::hub_snapshot)
}
