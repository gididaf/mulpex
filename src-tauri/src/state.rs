//! Backend application state: the `Core` (one open project + its live sessions +
//! the on-disk coordination scratch dir) and the Tauri-managed `AppState`.
//!
//! This is the port of the old `App`: session lifecycle, persistence, the hub
//! mirror (now read into a serializable `HubSnapshot` for the frontend rather
//! than rendered), reaping with `bounce_dead_inbox`, and teardown. The event loop
//! that drove it lives in `hub.rs`; the command surface in `commands.rs`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mulpex_core::config::{HOOK_SETTINGS_JSON, MCP_CONFIG_JSON};
use mulpex_core::persist::{self, SessionStore};

use crate::pty::{Session, SpawnTask};
use crate::snapshot::{
    BootstrapInfo, HubSnapshot, LockEntry, MsgEntry, PendingEntry, ProjectHandle, SessionInfo,
    Status,
    StatusEntry, TaskEntry, WaitEntry, WorkspaceInfo,
};

/// Default PTY geometry a session is spawned at; the frontend fits + resizes it
/// as soon as its xterm mounts, so this only affects the first repaint.
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 32;

/// How many of the most recent hub messages we surface in the feed.
const MSG_FEED_MAX: usize = 200;

/// Tauri-managed state. The `Workspace` holds every open project (0..N);
/// `helper_path` is the absolute path of the `mulpex-helper` binary the hooks/MCP
/// invoke.
pub struct AppState {
    pub ws: Mutex<Workspace>,
    pub helper_path: PathBuf,
}

/// All open projects in tab order, which one is active, and the per-process scratch
/// root (`temp/mulpex-<pid>`) each project's isolated `state_dir` hangs under.
pub struct Workspace {
    pub projects: Vec<Core>,
    pub active: Option<ProjectHandle>,
    pub next_handle: ProjectHandle,
    pub state_root: PathBuf,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace {
    pub fn new() -> Self {
        Self {
            projects: Vec::new(),
            active: None,
            next_handle: 1,
            state_root: std::env::temp_dir().join(format!("mulpex-{}", std::process::id())),
        }
    }

    fn alloc_handle(&mut self) -> ProjectHandle {
        let h = self.next_handle;
        self.next_handle += 1;
        h
    }

    pub fn project(&self, h: ProjectHandle) -> Option<&Core> {
        self.projects.iter().find(|c| c.handle == h)
    }

    pub fn project_mut(&mut self, h: ProjectHandle) -> Option<&mut Core> {
        self.projects.iter_mut().find(|c| c.handle == h)
    }

    /// The handle of an already-open project at `canon` (canonicalized dir), if any.
    fn find_by_dir(&self, canon: &Path) -> Option<ProjectHandle> {
        self.projects
            .iter()
            .find(|c| c.project_dir.as_path() == canon)
            .map(|c| c.handle)
    }

    /// Open `path` as a project and make it active. If it's already open (compared
    /// canonically), just re-activate it. Returns `(handle, newly_opened)`. Does NOT
    /// persist the open-set or emit — the caller does. Canonicalizes so `/foo`,
    /// `/foo/`, and symlinks unify, matching `MULPEX_PROJECT_DIR` in pty.rs.
    pub fn open_or_focus(
        &mut self,
        path: &str,
        helper_path: &Path,
    ) -> anyhow::Result<(ProjectHandle, bool)> {
        let dir = PathBuf::from(path);
        if !dir.is_dir() {
            anyhow::bail!("not a directory: {path}");
        }
        let canon = std::fs::canonicalize(&dir).unwrap_or(dir);
        if let Some(h) = self.find_by_dir(&canon) {
            self.active = Some(h);
            return Ok((h, false));
        }
        let handle = self.alloc_handle();
        let state_dir = self.state_root.join(handle.to_string());
        let core = Core::open(handle, canon, helper_path, state_dir)?;
        self.projects.push(core);
        self.active = Some(handle);
        Ok((handle, true))
    }

    /// Close a project: tear its sessions down (kills PGs, removes its state_dir),
    /// remove it, and re-pick the active project (the neighbor that shifts into its
    /// slot, else the last, else `None`).
    pub fn close_project(&mut self, h: ProjectHandle) {
        let Some(pos) = self.projects.iter().position(|c| c.handle == h) else {
            return;
        };
        let mut core = self.projects.remove(pos);
        core.teardown();
        if self.active == Some(h) {
            self.active = self
                .projects
                .get(pos)
                .or_else(|| self.projects.last())
                .map(|c| c.handle);
        }
    }

    /// Snapshot for the frontend: every open project + the active handle.
    pub fn workspace_info(&self) -> WorkspaceInfo {
        WorkspaceInfo {
            projects: self.projects.iter().map(Core::bootstrap_info).collect(),
            active: self.active,
        }
    }

    /// Persist the open-project set (canonical dirs, tab order) so the next launch
    /// reopens them.
    pub fn persist_open(&self) {
        let dirs: Vec<String> = self
            .projects
            .iter()
            .map(|c| c.project_dir.display().to_string())
            .collect();
        crate::project::save_open(&dirs);
    }

    /// Remove scratch roots belonging to Mulpex processes that are no longer
    /// running. `teardown_all` covers every *graceful* exit; this is the backstop
    /// for the exits no code of ours runs on at all — Force Quit, `kill -9`, a
    /// crash, a power loss — so `temp/` can't accumulate one `mulpex-<pid>` tree
    /// per launch. Errs toward keeping: a recycled pid now owned by an unrelated
    /// process reads as alive and just defers that dir to a later launch, whereas
    /// deleting a *live* Mulpex's scratch root would break its running hub.
    pub fn sweep_stale_state_roots(&self) {
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path == self.state_root || !path.is_dir() {
                continue;
            }
            // `mulpex-<pid>` only — the unit tests' `mulpex-open-test-<uuid>` dirs
            // fail the parse and are left alone.
            let Some(pid) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("mulpex-"))
                .and_then(|n| n.parse::<libc::pid_t>().ok())
            else {
                continue;
            };
            // kill(pid, 0) probes for existence without signalling: 0 => alive,
            // EPERM => alive but another user's, ESRCH => gone.
            let alive = unsafe { libc::kill(pid, 0) } == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
            if !alive {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }

    /// Kill every project's process groups and remove the whole scratch root. The
    /// "no orphaned claude" guarantee, now across all projects.
    pub fn teardown_all(&mut self) {
        for core in &mut self.projects {
            core.teardown();
        }
        self.projects.clear();
        let _ = std::fs::remove_dir_all(&self.state_root);
    }
}

/// One open project and everything backing its live Claude sessions.
pub struct Core {
    pub handle: ProjectHandle,
    pub project_dir: PathBuf,
    pub project_name: String,
    pub sessions: Vec<Session>,
    pub active: usize,
    pub next_id: usize,
    pub state_dir: PathBuf,
    pub settings_path: PathBuf,
    pub store: SessionStore,
    /// Custom per-instance names (id → name), persisted alongside session ids.
    pub names: HashMap<usize, String>,
    /// Muted instance ids (⌘M), persisted alongside session ids. Presentation
    /// only — a muted instance runs and coordinates exactly like any other; the
    /// frontend just dims it, sorts it last, and leaves it out of the badges.
    pub muted: HashSet<usize>,
    /// Instance ids that have been "worked on" (restored, or fired a hook this
    /// run). Only these are persisted for restore.
    pub worked: HashSet<usize>,
}

impl Core {
    /// Open `project_dir` under its own isolated `state_dir` (so its hub is scoped
    /// to just this project): create the scratch dir, write the `--settings` /
    /// `--mcp-config` files (pointing hooks + MCP at `helper_path`), and restore
    /// the sessions worked on last time (spawning each with `--resume`). If there
    /// is nothing to restore the project opens **empty** — no `claude` is started
    /// until the user asks for one with ⌘T. Ports `App::new`.
    pub fn open(
        handle: ProjectHandle,
        project_dir: PathBuf,
        helper_path: &Path,
        state_dir: PathBuf,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&state_dir)?;
        let settings_path = state_dir.join("settings.json");
        let helper = helper_path.to_string_lossy();
        std::fs::write(
            &settings_path,
            HOOK_SETTINGS_JSON.replace("__MULPEX_BIN__", &helper),
        )?;
        std::fs::write(
            state_dir.join("mcp.json"),
            MCP_CONFIG_JSON.replace("__MULPEX_BIN__", &helper),
        )?;
        for sub in ["locks", "history", "tasks", "inbox", "waiting", "spawn", "armed"] {
            std::fs::create_dir_all(state_dir.join(sub))?;
        }

        let store = SessionStore::new(&project_dir);
        let mut sessions: Vec<Session> = Vec::new();
        let mut worked: HashSet<usize> = HashSet::new();
        let mut names: HashMap<usize, String> = HashMap::new();
        let mut muted: HashSet<usize> = HashSet::new();
        for saved in store.load() {
            let id = sessions.len() + 1;
            if let Ok(session) = Session::spawn(
                id,
                &project_dir,
                DEFAULT_ROWS,
                DEFAULT_COLS,
                &settings_path,
                &state_dir,
                &saved.session_id,
                true,
                None,
            ) {
                worked.insert(id);
                if let Some(name) = saved.name {
                    names.insert(id, name);
                }
                if saved.muted {
                    muted.insert(id);
                }
                sessions.push(session);
            }
        }

        // Deliberately no fallback spawn: a project with nothing to restore opens
        // with **zero** sessions and waits for the user to press ⌘T. Opening a
        // project should not start a `claude` the user didn't ask for. This is the
        // single path for both startup restore and newly opened projects, so they
        // behave identically. Zero sessions is an already-supported state (closing
        // the last session with ⌘W produces it): `active` stays 0 and indexes
        // nothing, `bootstrap_info` yields `activeSessionId: null`, the frontend
        // hides every terminal and `TerminalPane` shows its ⌘T empty state.
        let next_id = sessions.len() + 1;
        let project_name = project_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| project_dir.display().to_string());

        let core = Self {
            handle,
            project_dir,
            project_name,
            sessions,
            active: 0,
            next_id,
            state_dir,
            settings_path,
            store,
            names,
            muted,
            worked,
        };
        core.persist_sessions();
        core.write_live_instances();
        Ok(core)
    }

    /// `BootstrapInfo` for the frontend to build one xterm per session.
    pub fn bootstrap_info(&self) -> BootstrapInfo {
        BootstrapInfo {
            handle: self.handle,
            project_dir: self.project_dir.display().to_string(),
            project_name: self.project_name.clone(),
            sessions: self.session_infos(),
            active: self.active,
        }
    }

    /// The live sessions as the sidebar shows them, in order.
    pub fn session_infos(&self) -> Vec<SessionInfo> {
        self.sessions
            .iter()
            .map(|s| SessionInfo {
                id: s.id,
                name: self.names.get(&s.id).cloned(),
                muted: self.muted.contains(&s.id),
            })
            .collect()
    }

    /// Spawn a fresh Claude in the project dir and focus it (⌘T / the frontend
    /// `create_session` command). Ports `App::spawn_instance`.
    pub fn spawn_instance(&mut self) -> anyhow::Result<SessionInfo> {
        self.spawn_with(None, true)
    }

    /// Spawn a fresh Claude that starts immediately on `task`, assigned by
    /// `parent_id` (an instance's `hub_spawn` call, dispatched by the poll loop).
    /// The child is auto-named from the task so the sidebar labels it, is told its
    /// parent (so it can report back via `hub_send`), and is NOT focused — the user
    /// stays on their current pane while children appear in the sidebar.
    pub fn spawn_instance_with_task(
        &mut self,
        parent_id: usize,
        task: String,
    ) -> anyhow::Result<SessionInfo> {
        let name = name_from_task(&task);
        let info = self.spawn_with(Some(SpawnTask { parent_id, task }), false)?;
        if let Some(name) = name {
            self.names.insert(info.id, name.clone());
            return Ok(SessionInfo {
                id: info.id,
                name: Some(name),
                muted: false,
            });
        }
        Ok(info)
    }

    /// Shared spawn path: allocate an id, spawn the session (optionally with an
    /// initial task), append it, optionally focus it, and republish the peer list.
    fn spawn_with(
        &mut self,
        initial_task: Option<SpawnTask>,
        focus: bool,
    ) -> anyhow::Result<SessionInfo> {
        let id = self.next_id;
        let session_id = persist::new_uuid();
        let session = Session::spawn(
            id,
            &self.project_dir,
            DEFAULT_ROWS,
            DEFAULT_COLS,
            &self.settings_path,
            &self.state_dir,
            &session_id,
            false,
            initial_task,
        )?;
        self.next_id += 1;
        self.sessions.push(session);
        if focus {
            self.active = self.sessions.len() - 1;
        }
        self.write_live_instances();
        Ok(SessionInfo {
            id,
            name: None,
            muted: false,
        })
    }

    /// Fulfil `hub_spawn` requests instances left on disk: for each `spawn/*.json`
    /// request, spawn one session per task, write the assigned ids back to a
    /// `<uuid>.done` file (the waiting MCP tool reads it), and delete the request.
    /// Returns whether any session was created (so the poll loop re-emits the
    /// session list). Runs single-threaded in the poll loop, so id allocation and
    /// the response handshake are race-free.
    pub fn process_spawn_requests(&mut self) -> bool {
        let dir = self.state_dir.join("spawn");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return false;
        };
        let mut requests: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
            .collect();
        requests.sort();

        let mut spawned_any = false;
        for req in requests {
            let Ok(content) = std::fs::read_to_string(&req) else {
                let _ = std::fs::remove_file(&req);
                continue;
            };
            let parsed: Option<(usize, Vec<String>)> = serde_json::from_str::<serde_json::Value>(
                &content,
            )
            .ok()
            .and_then(|v| {
                let from = v.get("from").and_then(|x| x.as_u64())? as usize;
                let tasks = v
                    .get("tasks")?
                    .as_array()?
                    .iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .filter(|t| !t.trim().is_empty())
                    .collect::<Vec<_>>();
                Some((from, tasks))
            });
            let _ = std::fs::remove_file(&req);
            let Some((from, tasks)) = parsed else { continue };

            let mut ids = Vec::new();
            for task in tasks {
                if let Ok(info) = self.spawn_instance_with_task(from, task) {
                    ids.push(info.id);
                    spawned_any = true;
                }
            }
            // Response the waiting `hub_spawn` reads: the ids it should return.
            if let Some(stem) = req.file_stem().and_then(|s| s.to_str()) {
                let done = serde_json::json!({ "ids": ids }).to_string();
                let _ = std::fs::write(dir.join(format!("{stem}.done")), done);
            }
        }
        spawned_any
    }

    /// Find a session by id.
    pub fn session_mut(&mut self, id: usize) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    /// Rename an instance: a non-empty (trimmed) name sets it, empty clears it.
    /// Persists so it survives restart. Ports `App::commit_rename`.
    pub fn rename(&mut self, id: usize, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.names.remove(&id);
        } else {
            self.names.insert(id, name.to_string());
        }
        self.persist_sessions();
    }

    /// Mute or unmute an instance (⌘M / the sidebar's 🔇). Persists so it
    /// survives restart, like a rename. Nothing else changes: the `claude` keeps
    /// running, keeps its inbox, and stays in the peer list — mute is entirely a
    /// statement about how loudly the *sidebar* should talk about it.
    pub fn set_muted(&mut self, id: usize, muted: bool) {
        if muted {
            self.muted.insert(id);
        } else {
            self.muted.remove(&id);
        }
        self.persist_sessions();
    }

    /// Close an instance by id: kill its process group. Removal, focus-fixing,
    /// persistence, peer-list rewrite and mail-bounce all happen uniformly on the
    /// next `reap_dead` pass (which also emits `session-exited`), so an explicit
    /// close and a self-exit follow the identical single-sourced path.
    pub fn close(&mut self, id: usize) {
        if let Some(session) = self.session_mut(id) {
            session.kill();
        }
    }

    /// Set the focused instance (persistence/order bookkeeping).
    pub fn set_active(&mut self, id: usize) {
        if let Some(pos) = self.sessions.iter().position(|s| s.id == id) {
            self.active = pos;
        }
    }

    /// Resize every session's PTY to the shared center-pane geometry (all
    /// sessions share one size, as the TUI did).
    pub fn resize_all(&mut self, cols: u16, rows: u16) {
        for s in &mut self.sessions {
            s.resize(rows, cols);
        }
    }

    /// Drop sessions whose `claude` has exited; return their ids. Runs the full
    /// reap: fix `active`, drop from `worked`/`names`, persist, rewrite the peer
    /// list, and bounce each closed instance's undelivered mail. Ports
    /// `App::reap_dead` (+ the inbox reaping half of `refresh_hub`).
    pub fn reap_dead(&mut self) -> Vec<usize> {
        if self.sessions.is_empty() || self.sessions.iter().all(|s| s.is_alive()) {
            return Vec::new();
        }
        let old_active = self.active;
        let mut removed = Vec::new();
        let mut kept: Vec<Session> = Vec::with_capacity(self.sessions.len());
        let mut new_active: Option<usize> = None;
        for (idx, session) in std::mem::take(&mut self.sessions).into_iter().enumerate() {
            if session.is_alive() {
                if idx == old_active {
                    new_active = Some(kept.len());
                }
                kept.push(session);
            } else {
                removed.push(session.id);
                // Dropping the dead session here kills its process group.
            }
        }
        self.sessions = kept;
        self.active = match new_active {
            Some(a) => a,
            None if self.sessions.is_empty() => 0,
            None => old_active.min(self.sessions.len() - 1),
        };
        self.worked.retain(|id| self.sessions.iter().any(|s| s.id == *id));
        self.names.retain(|id, _| self.sessions.iter().any(|s| s.id == *id));
        self.muted.retain(|id| self.sessions.iter().any(|s| s.id == *id));
        self.persist_sessions();
        self.write_live_instances();
        removed
    }

    /// Mark instances "worked on" once their hook state file appears (a prompt
    /// was submitted). Persists if the set grew. Ports the worked half of
    /// `App::refresh_statuses`.
    pub fn refresh_worked(&mut self) {
        let mut newly = false;
        let ids: Vec<usize> = self.sessions.iter().map(|s| s.id).collect();
        for id in ids {
            if self.state_dir.join(id.to_string()).exists() && self.worked.insert(id) {
                newly = true;
            }
        }
        if newly {
            self.persist_sessions();
        }
    }

    /// Read the live coordination state off disk into a serializable snapshot,
    /// reaping dead instances' tasks/inboxes/waiting entries. Ports
    /// `refresh_statuses`/`refresh_locks`/`refresh_hub`/`refresh_messages`.
    pub fn hub_snapshot(&self) -> HubSnapshot {
        let live: HashSet<usize> = self.sessions.iter().map(|s| s.id).collect();

        // Statuses.
        let mut statuses: Vec<StatusEntry> = self
            .sessions
            .iter()
            .map(|s| {
                let status = std::fs::read_to_string(self.state_dir.join(s.id.to_string()))
                    .ok()
                    .and_then(|w| Status::from_word(w.trim()))
                    .unwrap_or(Status::Waiting);
                StatusEntry { id: s.id, status }
            })
            .collect();
        statuses.sort_by_key(|e| e.id);

        // Locks (reap dead holders' tokens).
        let mut locks: Vec<LockEntry> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.state_dir.join("locks")) {
            for entry in entries.flatten() {
                let file = entry.path();
                let Some((holder, path)) = read_lock(&file) else {
                    continue;
                };
                if live.contains(&holder) {
                    locks.push(LockEntry {
                        path: path.display().to_string(),
                        holder,
                    });
                } else {
                    let _ = std::fs::remove_file(&file);
                }
            }
        }
        locks.sort_by(|a, b| a.path.cmp(&b.path));

        // Tasks (prune dead).
        let mut tasks: Vec<TaskEntry> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.state_dir.join("tasks")) {
            for entry in entries.flatten() {
                let Some(id) = entry.file_name().to_str().and_then(|n| n.parse::<usize>().ok())
                else {
                    continue;
                };
                if !live.contains(&id) {
                    let _ = std::fs::remove_file(entry.path());
                    continue;
                }
                if let Ok(t) = std::fs::read_to_string(entry.path()) {
                    let t = t.trim().to_string();
                    if !t.is_empty() {
                        tasks.push(TaskEntry { id, task: t });
                    }
                }
            }
        }
        tasks.sort_by_key(|e| e.id);

        // Inboxes: count queued, bounce dead recipients' undelivered mail. The
        // per-instance counts ride along with the total so the frontend can
        // subtract a muted instance's mail out of the unread badges.
        let mut pending = 0usize;
        let mut pending_by_id: Vec<PendingEntry> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.state_dir.join("inbox")) {
            for entry in entries.flatten() {
                let Some(id) = entry.file_name().to_str().and_then(|n| n.parse::<usize>().ok())
                else {
                    continue;
                };
                if !live.contains(&id) {
                    bounce_dead_inbox(&self.state_dir, id, &entry.path(), &live);
                    continue;
                }
                let count = std::fs::read_dir(entry.path())
                    .map(|d| d.flatten().count())
                    .unwrap_or(0);
                pending += count;
                if count > 0 {
                    pending_by_id.push(PendingEntry { id, count });
                }
            }
        }
        pending_by_id.sort_by_key(|e| e.id);

        // Waiting indicators (prune dead).
        let mut waiting: Vec<WaitEntry> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.state_dir.join("waiting")) {
            for entry in entries.flatten() {
                let Some(id) = entry.file_name().to_str().and_then(|n| n.parse::<usize>().ok())
                else {
                    continue;
                };
                if !live.contains(&id) {
                    let _ = std::fs::remove_file(entry.path());
                    continue;
                }
                if let Ok(body) = std::fs::read_to_string(entry.path()) {
                    let mut parts = body.trim().splitn(2, '\t');
                    if let (Some(file), Some(holder)) = (parts.next(), parts.next()) {
                        if let Ok(holder) = holder.trim().parse::<usize>() {
                            waiting.push(WaitEntry {
                                id,
                                file: file.to_string(),
                                holder,
                            });
                        }
                    }
                }
            }
        }
        waiting.sort_by_key(|e| e.id);

        let messages = read_messages(&self.state_dir.join("messages.log"), MSG_FEED_MAX);

        HubSnapshot {
            statuses,
            tasks,
            locks,
            waiting,
            messages,
            pending_messages: pending,
            pending: pending_by_id,
        }
    }

    /// Persist the worked-on sessions' ids (+ names + mute), preserving order.
    fn persist_sessions(&self) {
        let sessions: Vec<persist::SavedSession> = self
            .sessions
            .iter()
            .filter(|s| self.worked.contains(&s.id))
            .map(|s| persist::SavedSession {
                session_id: s.session_id.clone(),
                name: self.names.get(&s.id).cloned(),
                muted: self.muted.contains(&s.id),
            })
            .collect();
        self.store.save(&sessions);
    }

    /// Publish the live instance ids to `state_dir/instances` (the peer list the
    /// hub reads).
    fn write_live_instances(&self) {
        let mut out = String::new();
        for s in &self.sessions {
            out.push_str(&s.id.to_string());
            out.push('\n');
        }
        let _ = std::fs::write(self.state_dir.join("instances"), out);
    }

    /// Deterministic teardown: kill every session's process group, then remove
    /// the scratch dir. Ports `impl Drop for App`; called from the app's
    /// `RunEvent` exit handler (managed-state Drop isn't guaranteed on quit).
    pub fn teardown(&mut self) {
        for s in &mut self.sessions {
            s.kill();
        }
        self.sessions.clear();
        let _ = std::fs::remove_dir_all(&self.state_dir);
    }
}

// ---- module helpers (ported from app.rs) ----

/// A short sidebar label derived from a spawned instance's task: collapse
/// whitespace and cap the length. `None` for an all-whitespace task.
fn name_from_task(task: &str) -> Option<String> {
    let one_line = task.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return None;
    }
    let mut s: String = one_line.chars().take(48).collect();
    if one_line.chars().count() > 48 {
        s.push('…');
    }
    Some(s)
}

/// Parse a lock file into `(holder id, locked path)`.
fn read_lock(file: &Path) -> Option<(usize, PathBuf)> {
    let content = std::fs::read_to_string(file).ok()?;
    let mut holder: Option<usize> = None;
    let mut path: Option<PathBuf> = None;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("instance=") {
            holder = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("path=") {
            path = Some(PathBuf::from(v.trim()));
        }
    }
    Some((holder?, path?))
}

/// Read the last `max` records from the conversation log, newest first.
fn read_messages(path: &Path, max: usize) -> Vec<MsgEntry> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL_BYTES: u64 = 128 * 1024;
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let tail = len.min(TAIL_BYTES);
    if f.seek(SeekFrom::End(-(tail as i64))).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if tail < len && !lines.is_empty() {
        lines.remove(0);
    }
    let mut out = Vec::new();
    for line in lines.iter().rev() {
        if out.len() >= max {
            break;
        }
        let mut parts = line.splitn(4, '\t');
        let (Some(ts), Some(from), Some(to), Some(body)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (Ok(ts), Ok(from)) = (ts.parse::<u64>(), from.parse::<usize>()) else {
            continue;
        };
        out.push(MsgEntry {
            from,
            to: to.to_string(),
            body: unescape_msg(body),
            ts,
        });
    }
    out
}

fn unescape_msg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Bounce a closed instance's undelivered mail back to live senders, then remove
/// the inbox dir. Ports `app::bounce_dead_inbox`.
fn bounce_dead_inbox(state_dir: &Path, dead_id: usize, inbox: &Path, live: &HashSet<usize>) -> usize {
    let mut bounced = 0usize;
    if let Ok(entries) = std::fs::read_dir(inbox) {
        for entry in entries.flatten() {
            let file = entry.path();
            if file.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some((from, body)) = std::fs::read_to_string(&file).ok().and_then(|c| {
                let v: serde_json::Value = serde_json::from_str(&c).ok()?;
                let from = v.get("from").and_then(|x| x.as_u64())? as usize;
                let body = v
                    .get("body")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                Some((from, body))
            }) else {
                continue;
            };
            if from == dead_id || !live.contains(&from) {
                continue;
            }
            let snippet: String = body.chars().take(80).collect();
            let ellipsis = if body.chars().count() > 80 { "…" } else { "" };
            let notice = format!(
                "[Mulpex hub — automated] Your message to claude #{dead_id} was NOT delivered: \
                 that instance closed before reading it. Original: \"{snippet}{ellipsis}\""
            );
            let dir = state_dir.join("inbox").join(from.to_string());
            if std::fs::create_dir_all(&dir).is_ok() {
                let payload = serde_json::json!({
                    "from": dead_id, "ts": now_secs(), "body": notice,
                });
                if std::fs::write(
                    dir.join(format!("{}.json", persist::new_uuid())),
                    payload.to_string(),
                )
                .is_ok()
                {
                    bounced += 1;
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(inbox);
    bounced
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opening a project must never start a `claude` the user didn't ask for.
    /// With nothing to restore the project comes up empty and waits for ⌘T.
    #[test]
    fn open_with_nothing_to_restore_spawns_no_session() {
        let root = std::env::temp_dir().join(format!("mulpex-open-test-{}", persist::new_uuid()));
        let project_dir = root.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        // Point the session store at a throwaway HOME so we never read (or write)
        // the real ~/.mulpex/sessions and accidentally restore a live project.
        std::env::set_var("HOME", root.join("home"));

        let core = Core::open(
            1,
            project_dir,
            Path::new("/nonexistent/mulpex-helper"),
            root.join("state"),
        )
        .expect("open should succeed with no sessions to restore");

        assert!(
            core.sessions.is_empty(),
            "opening a project spawned {} session(s); it must wait for ⌘T",
            core.sessions.len()
        );
        assert_eq!(core.next_id, 1, "first ⌘T should hand out instance id 1");

        let _ = std::fs::remove_dir_all(&root);
    }
}
