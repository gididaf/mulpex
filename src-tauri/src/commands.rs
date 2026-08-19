//! The `#[tauri::command]` surface the Svelte frontend invokes. Thin wrappers over
//! the `Workspace`; all mutation goes through the `AppState` mutex. Every
//! session-scoped command carries a `project_handle` (JS `projectHandle`) so
//! per-project instance ids (each project numbers 1,2,3…) never collide.

use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, State};

use crate::project;
use crate::snapshot::{
    BootstrapInfo, ClaudeStatus, HubSnapshot, ProjectHandle, SessionInfo, WorkspaceInfo,
};
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

/// Whether the user's `claude` CLI could be located, and where.
///
/// The frontend calls this on startup: without it, a missing `claude` surfaces
/// only as every `open_project` failing, which used to look like the app simply
/// ignoring the click. See `claude_bin` for why a GUI launch can miss a `claude`
/// that works fine in the user's terminal.
#[tauri::command]
pub fn claude_status() -> ClaudeStatus {
    match crate::claude_bin::resolve_claude() {
        Some(p) => ClaudeStatus {
            found: true,
            path: Some(p.to_string_lossy().into_owned()),
            searched_path: crate::claude_bin::merged_path().to_string(),
        },
        None => ClaudeStatus {
            found: false,
            path: None,
            searched_path: crate::claude_bin::merged_path().to_string(),
        },
    }
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

/// Commit a new tab order after the user drags a project tab. `handles` is the
/// full left-to-right order; the backend is the source of truth for persistence,
/// so this also rewrites `open.txt` (see `Workspace::reorder_projects`).
#[tauri::command]
pub fn reorder_projects(state: State<AppState>, handles: Vec<ProjectHandle>) {
    state.ws.lock().unwrap().reorder_projects(&handles);
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

/// Open a plain shell terminal (⌘⇧T) in the given project and focus it.
#[tauri::command]
pub fn create_terminal(
    state: State<AppState>,
    project_handle: ProjectHandle,
) -> Result<SessionInfo, String> {
    let mut ws = state.ws.lock().unwrap();
    let core = ws.project_mut(project_handle).ok_or("no such project")?;
    core.spawn_terminal(None, None, true).map_err(|e| e.to_string())
}

/// Close a session (⌘W) — kills its process group and reaps. Works for both
/// kinds; for a terminal this is also what removes an already-exited row.
#[tauri::command]
pub fn close_session(state: State<AppState>, project_handle: ProjectHandle, id: usize) {
    if let Some(core) = state.ws.lock().unwrap().project_mut(project_handle) {
        core.close(id);
    }
}

/// Commit a new sidebar order after the user drags a session row. `ids` is the
/// full top-to-bottom order within that project; the backend is the source of
/// truth for persistence, so this also rewrites the session store (see
/// `Core::reorder_sessions`).
#[tauri::command]
pub fn reorder_sessions(state: State<AppState>, project_handle: ProjectHandle, ids: Vec<usize>) {
    if let Some(core) = state.ws.lock().unwrap().project_mut(project_handle) {
        core.reorder_sessions(&ids);
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

/// Mute or unmute a session (⌘M / the sidebar's 🔇). Presentation only — the
/// instance is untouched; only how the sidebar and tab badges treat it changes.
#[tauri::command]
pub fn set_session_muted(
    state: State<AppState>,
    project_handle: ProjectHandle,
    id: usize,
    muted: bool,
) {
    if let Some(core) = state.ws.lock().unwrap().project_mut(project_handle) {
        core.set_muted(id, muted);
    }
}

/// Sync the "Mute Session" menu tick to the focused session's state. Called by
/// the frontend whenever focus or the flag moves — the menu has no view of which
/// session is active, so the tick has to be pushed to it.
#[tauri::command]
pub fn set_mute_menu_checked(app: AppHandle, checked: bool) {
    crate::menu::set_mute_checked(&app, checked);
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

/// Resize **every** session in **every** open project to the center pane's
/// geometry, and record it as the size later spawns start at.
///
/// One call for the whole workspace, not one per project: all PTYs share a single
/// size, and the frontend builds every xterm at that same size, so the two must
/// be updated together or not at all. A project left at the old size would spawn
/// its next session there and hand the frontend a PTY whose emulator is a
/// different shape — which corrupts the session permanently, not transiently
/// (see `WorkspaceInfo::cols`).
#[tauri::command]
pub fn resize_terminals(state: State<AppState>, cols: u16, rows: u16) {
    state.ws.lock().unwrap().resize_all(cols, rows);
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

/// Relaunch Mulpex — the second half of applying an update, called once the
/// updater has swapped the bundle on disk.
///
/// This deliberately does NOT use `@tauri-apps/plugin-process`'s `relaunch`: we
/// need the restart to run our teardown, which only happens if the restart fires
/// `RunEvent::ExitRequested`/`Exit` on the way out. That routes through `lib.rs`'s
/// handler, so every project's `claude` process group is killed and the scratch
/// root removed before the new binary starts — without it an update would orphan
/// process groups and leak a scratch dir on every single release.
///
/// `request_restart`, NOT `restart`: `AppHandle::restart` documents that when it
/// is called *on the main thread* it "cannot guarantee the delivery of those
/// events, so we skip them" and re-execs immediately — which would silently skip
/// teardown. Whether a command body lands on the main thread is Tauri's
/// scheduling choice, not ours (measured today: it does not, and the events do
/// fire), so relying on it would be a latent bug one runtime update away.
/// `request_restart` always goes through `request_exit(RESTART_EXIT_CODE)`.
#[tauri::command]
pub fn restart_app(app: AppHandle) {
    app.request_restart()
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
