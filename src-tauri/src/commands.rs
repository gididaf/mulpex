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

/// Restart one claude in place (⌘⇧R): kill it and relaunch on the same row with
/// `--resume`, so it re-reads its environment (a rotated auth token, a newly
/// installed skill) without the user losing the conversation, the instance number
/// or its undelivered mail. Refuses — killing nothing — when that instance has no
/// transcript to resume yet; the frontend shows the reason.
#[tauri::command]
pub fn restart_session(
    state: State<AppState>,
    project_handle: ProjectHandle,
    id: usize,
) -> Result<(), String> {
    let mut ws = state.ws.lock().unwrap();
    let core = ws.project_mut(project_handle).ok_or("no such project")?;
    core.restart_instance(id).map_err(|e| e.to_string())
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

/// Drag-to-nest: move a claude (with its family) under another claude, or to the
/// top level with `parent: None`, committing the sidebar order `ids` with it
/// (`Core::reparent`). Refused for terminals and for cycles.
#[tauri::command]
pub fn reparent_session(
    state: State<AppState>,
    project_handle: ProjectHandle,
    id: usize,
    parent: Option<usize>,
    ids: Vec<usize>,
) -> bool {
    match state.ws.lock().unwrap().project_mut(project_handle) {
        Some(core) => core.reparent(id, parent, &ids),
        None => false,
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

/// The project's saved ⌘⇧P pins, by instance id (`pins.rs`).
#[tauri::command]
pub fn get_pins(
    state: State<AppState>,
    project_handle: ProjectHandle,
) -> std::collections::BTreeMap<usize, serde_json::Value> {
    match state.ws.lock().unwrap().project(project_handle) {
        Some(core) => core.pin_store().load(),
        None => Default::default(),
    }
}

/// Save (`Some`) or remove (`None`) one instance's pin.
#[tauri::command]
pub fn set_pin(
    state: State<AppState>,
    project_handle: ProjectHandle,
    id: usize,
    pin: Option<serde_json::Value>,
) {
    if let Some(core) = state.ws.lock().unwrap().project(project_handle) {
        core.pin_store().set(id, pin);
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

/// Fold or unfold a row's `hub_spawn` family in the sidebar; persisted.
#[tauri::command]
pub fn set_session_collapsed(
    state: State<AppState>,
    project_handle: ProjectHandle,
    id: usize,
    collapsed: bool,
) {
    if let Some(core) = state.ws.lock().unwrap().project_mut(project_handle) {
        core.set_collapsed(id, collapsed);
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

/// Save one claude's work as a handoff doc in the repo (⌘S, and the row's retry
/// after a failed save). Returns once the save is under way; progress arrives as
/// `save-progress`. Refuses — for the same reason ⌘⇧R does — an instance with no
/// transcript yet: there is no conversation to fork.
#[tauri::command]
pub fn save_session(
    app: AppHandle,
    state: State<AppState>,
    project_handle: ProjectHandle,
    id: usize,
) -> Result<(), String> {
    let (dir, uuid) = {
        let ws = state.ws.lock().unwrap();
        let core = ws.project(project_handle).ok_or("no such project")?;
        let s = core.sessions.iter().find(|s| s.id == id).ok_or(format!("claude#{id} is not open"))?;
        if s.is_shell() {
            return Err(format!("term#{id} is a terminal — only a claude can be saved"));
        }
        if s.session_id.is_empty() || !core.worked.contains(&id) {
            return Err(format!("claude#{id} has nothing to save yet — send it a prompt first"));
        }
        (core.project_dir.clone(), s.session_id.clone())
    };
    crate::saves::start(app, project_handle, id, dir, uuid)
}

/// ⌘K: write a one-off secrets file for claude#`id` and return its path. The
/// frontend types the reference line itself (`App.svelte::sendSecretsRef`).
#[tauri::command]
pub fn secrets_create_ephemeral(
    state: State<AppState>,
    project_handle: ProjectHandle,
    id: usize,
    rows: Vec<crate::secrets::SecretRow>,
) -> Result<String, String> {
    let (state_dir, _) = secrets_target(&state, project_handle, id)?;
    crate::secrets::create_ephemeral(&state_dir, id, &rows).map(|p| p.to_string_lossy().into_owned())
}

/// The scratch dir and project dir behind claude#`id`, refusing a terminal.
fn secrets_target(
    state: &State<AppState>,
    h: ProjectHandle,
    id: usize,
) -> Result<(std::path::PathBuf, String), String> {
    let ws = state.ws.lock().unwrap();
    let core = ws.project(h).ok_or("no such project")?;
    let s = core.sessions.iter().find(|s| s.id == id).ok_or(format!("claude#{id} is not open"))?;
    if s.is_shell() {
        return Err(format!("term#{id} is a terminal — secrets go to a claude"));
    }
    Ok((core.state_dir.clone(), core.project_dir.to_string_lossy().into_owned()))
}

/// ⌘K: the saved sets usable in this project — names and keys, never values.
#[tauri::command]
pub fn secrets_list(state: State<AppState>, project_handle: ProjectHandle) -> Vec<crate::secrets::SavedSet> {
    match project_dir(&state, project_handle) {
        Ok(dir) => crate::secrets::list_saved(&mulpex_core::mulpex_home(), &dir.to_string_lossy()),
        Err(_) => Vec::new(),
    }
}

/// ⌘K with "Save for reuse": save a new set and, if `send`, hand it to
/// claude#`id`. `project_only` limits it to this project. Returns the keys, for
/// the tag.
#[tauri::command]
pub fn secrets_save(
    state: State<AppState>,
    project_handle: ProjectHandle,
    id: usize,
    name: String,
    project_only: bool,
    rows: Vec<crate::secrets::SecretRow>,
    send: bool,
) -> Result<Vec<String>, String> {
    let (state_dir, project) = secrets_target(&state, project_handle, id)?;
    let home = mulpex_core::mulpex_home();
    let tag = project_only.then_some(project.as_str());
    crate::secrets::save(&home, &name, tag, &rows)?;
    if !send {
        return Ok(Vec::new());
    }
    crate::secrets::attach(&home, &project, &state_dir, id, name.trim())
}

/// ⌘K ✎: a saved set's rows, values included, for the edit form.
#[tauri::command]
pub fn secrets_get(name: String) -> Result<crate::secrets::SavedDetail, String> {
    crate::secrets::get_saved(&mulpex_core::mulpex_home(), &name)
}

/// ⌘K ✎ Save: replace a saved set's rows; `project_only` re-tags it to this project.
#[tauri::command]
pub fn secrets_update(
    state: State<AppState>,
    project_handle: ProjectHandle,
    name: String,
    project_only: bool,
    rows: Vec<crate::secrets::SecretRow>,
) -> Result<(), String> {
    let project = project_dir(&state, project_handle)?.to_string_lossy().into_owned();
    let tag = project_only.then_some(project.as_str());
    crate::secrets::update(&mulpex_core::mulpex_home(), &name, tag, &rows)
}

/// ⌘K 🗑: delete a saved set.
#[tauri::command]
pub fn secrets_delete(name: String) -> Result<(), String> {
    crate::secrets::delete(&mulpex_core::mulpex_home(), &name)
}

/// ⌘K: hand saved set `name` to claude#`id`. Returns its keys, for the tag.
#[tauri::command]
pub fn secrets_attach(
    state: State<AppState>,
    project_handle: ProjectHandle,
    id: usize,
    name: String,
) -> Result<Vec<String>, String> {
    let (state_dir, project) = secrets_target(&state, project_handle, id)?;
    crate::secrets::attach(&mulpex_core::mulpex_home(), &project, &state_dir, id, &name)
}

fn project_dir(state: &State<AppState>, h: ProjectHandle) -> Result<std::path::PathBuf, String> {
    let ws = state.ws.lock().unwrap();
    Ok(ws.project(h).ok_or("no such project")?.project_dir.clone())
}

/// The saves of the project's repo, newest first (the ⌘L list), each marked
/// with whether its conversation can still be continued here — and whether it
/// is already open, in which case "continue" means "go to that row".
#[tauri::command]
pub fn list_saves(
    state: State<AppState>,
    project_handle: ProjectHandle,
) -> Result<Vec<crate::saves::SaveEntry>, String> {
    let dir = project_dir(&state, project_handle)?;
    let mut entries = crate::saves::list(&dir);
    for e in &mut entries {
        let Ok(path) = crate::saves::resolve(&dir, &e.file) else { continue };
        if let Some(uuid) = crate::saves::resumable_uuid(&path, &dir) {
            e.continuable = true;
            e.open_as = open_row_for(&state, project_handle, &uuid);
        }
    }
    Ok(entries)
}

/// The open claude row already running conversation `uuid`, if any.
fn open_row_for(state: &State<AppState>, h: ProjectHandle, uuid: &str) -> Option<usize> {
    let ws = state.ws.lock().unwrap();
    let core = ws.project(h)?;
    core.sessions.iter().find(|s| !s.is_shell() && s.session_id == uuid).map(|s| s.id)
}

/// Delete one save file (the ⌘L list's trash icon, after its confirm).
#[tauri::command]
pub fn delete_save(state: State<AppState>, project_handle: ProjectHandle, file: String) -> Result<(), String> {
    crate::saves::delete(&project_dir(&state, project_handle)?, &file)
}

/// The guides of the project's repo, by title (the ⌘L Guides tab).
#[tauri::command]
pub fn list_guides(
    state: State<AppState>,
    project_handle: ProjectHandle,
) -> Result<Vec<crate::saves::GuideEntry>, String> {
    Ok(crate::saves::list_guides(&project_dir(&state, project_handle)?))
}

/// Retire a guide: its runbook and its pointer (after the list's confirm).
#[tauri::command]
pub fn delete_guide(state: State<AppState>, project_handle: ProjectHandle, file: String) -> Result<(), String> {
    crate::saves::delete_guide(&project_dir(&state, project_handle)?, &file)
}

/// Start a claude on a guide: it reads it and asks what the user needs. The
/// row starts named after the guide; the instance may rename it after the
/// specific incident.
#[tauri::command]
pub fn load_guide(
    state: State<AppState>,
    project_handle: ProjectHandle,
    file: String,
) -> Result<SessionInfo, String> {
    let (pointer, source, title) = crate::saves::guide(&project_dir(&state, project_handle)?, &file)?;
    let mut ws = state.ws.lock().unwrap();
    let core = ws.project_mut(project_handle).ok_or("no such project")?;
    core.spawn_instance_with_prompt(crate::saves::guide_prompt(&pointer, &source), Some(title), false)
        .map_err(|e| e.to_string())
}

/// Start (or return the running) Import Docs job for the project's repo.
/// Progress arrives as `import-update {handle}`; re-read with `import_state`.
#[tauri::command]
pub fn import_start(
    app: AppHandle,
    state: State<AppState>,
    project_handle: ProjectHandle,
) -> Result<crate::docs_import::ImportState, String> {
    let dir = project_dir(&state, project_handle)?;
    Ok(crate::docs_import::start(app, project_handle, &dir))
}

/// The project's import, if one is running or waiting for review.
#[tauri::command]
pub fn import_state(project_handle: ProjectHandle) -> Option<crate::docs_import::ImportState> {
    crate::docs_import::state(project_handle)
}

/// Throw the project's import away.
#[tauri::command]
pub fn import_discard(project_handle: ProjectHandle) {
    crate::docs_import::discard(project_handle);
}

/// Carry out the reviewed decisions (ticked rows only) and end the import.
#[tauri::command]
pub fn import_apply(
    state: State<AppState>,
    project_handle: ProjectHandle,
    decisions: Vec<crate::docs_import::Decision>,
) -> Result<crate::docs_import::ApplyReport, String> {
    let dir = project_dir(&state, project_handle)?;
    Ok(crate::docs_import::apply(project_handle, &dir, &decisions))
}

/// What `load_save` did: started a new row, or (`existing`) found the
/// conversation already open and is handing back that row to focus.
#[derive(serde::Serialize)]
pub struct LoadResult {
    pub info: SessionInfo,
    pub existing: bool,
}

/// Start a claude on a save (⌘L ▸ Enter). `mode`:
/// - `"fresh"`: a new claude reads the doc, checks the repo, reports and waits;
/// - `"continue"`: resume the conversation that last saved it — or, if that
///   conversation is already open, return its row (two claudes on one
///   transcript would corrupt it).
///
/// A new row is named after the save's title, and focused.
#[tauri::command]
pub fn load_save(
    state: State<AppState>,
    project_handle: ProjectHandle,
    file: String,
    mode: String,
) -> Result<LoadResult, String> {
    let dir = project_dir(&state, project_handle)?;
    let path = crate::saves::resolve(&dir, &file)?;
    let title = crate::saves::title_of(&path);
    if mode == "continue" {
        let uuid = crate::saves::resumable_uuid(&path, &dir)
            .ok_or("that conversation is no longer on this machine — start fresh from the doc")?;
        let mut ws = state.ws.lock().unwrap();
        let core = ws.project_mut(project_handle).ok_or("no such project")?;
        if let Some(open) = core.sessions.iter().find(|s| !s.is_shell() && s.session_id == uuid) {
            let id = open.id;
            let info = core.session_infos().into_iter().find(|i| i.id == id).ok_or("row vanished")?;
            core.set_active(id);
            return Ok(LoadResult { info, existing: true });
        }
        let info = core.spawn_instance_resuming(uuid, title).map_err(|e| e.to_string())?;
        return Ok(LoadResult { info, existing: false });
    }
    let mut ws = state.ws.lock().unwrap();
    let core = ws.project_mut(project_handle).ok_or("no such project")?;
    let info = core
        .spawn_instance_with_prompt(crate::saves::load_prompt(&path), title, true)
        .map_err(|e| e.to_string())?;
    // Link the new conversation to the save, so its ⌘S updates this file.
    if let Some(s) = core.sessions.iter().find(|s| s.id == info.id) {
        crate::saves::link("loaded", &s.session_id, &dir, &path);
    }
    Ok(LoadResult { info, existing: false })
}
