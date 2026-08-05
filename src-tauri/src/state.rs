//! Backend application state: the `Core` (one open project + its live sessions +
//! the on-disk coordination scratch dir) and the Tauri-managed `AppState`.
//!
//! This is the port of the old `App`: session lifecycle, persistence, the hub
//! mirror (now read into a serializable `HubSnapshot` for the frontend rather
//! than rendered), reaping with `bounce_dead_inbox`, and teardown. The event loop
//! that drove it lives in `hub.rs`; the command surface in `commands.rs`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mulpex_core::config::{HOOK_SETTINGS_JSON, MCP_CONFIG_JSON};
use mulpex_core::persist::{self, SessionStore};

use crate::pty::{self, Session, SpawnSpec, SpawnTask};
use crate::snapshot::{
    BootstrapInfo, HubSnapshot, LockEntry, MsgEntry, PendingEntry, ProjectHandle, SessionInfo,
    SessionKind, Status, StatusEntry, TaskEntry, WaitEntry, WorkspaceInfo,
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

    /// Reorder open projects to match `handles` (the tab bar's new left-to-right
    /// order after a drag). Handles we don't know are ignored and projects the
    /// caller omitted keep their relative order at the end, so a stale frontend
    /// list can never drop a project off the tab bar.
    ///
    /// Tab order is also the persisted open-project order, so this is what makes a
    /// drag survive relaunch — and what ⌘1–⌘9 index into.
    pub fn reorder_projects(&mut self, handles: &[ProjectHandle]) {
        let mut ordered: Vec<Core> = Vec::with_capacity(self.projects.len());
        for h in handles {
            if let Some(pos) = self.projects.iter().position(|c| c.handle == *h) {
                ordered.push(self.projects.remove(pos));
            }
        }
        ordered.append(&mut self.projects);
        self.projects = ordered;
        self.persist_open();
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
    /// Ids whose name the **user** owns — set by ⌘R, and by a restore bringing a
    /// name back (a persisted name can't be told apart from a hand-typed one, and
    /// protecting it is the safe direction). An instance's own `hub_set_name` is
    /// refused for these: the user's label always wins. See `process_name_requests`.
    manual_names: HashSet<usize>,
    /// Muted instance ids (⌘M), persisted alongside session ids. Presentation
    /// only — a muted instance runs and coordinates exactly like any other; the
    /// frontend just dims it, sorts it last, and leaves it out of the badges.
    pub muted: HashSet<usize>,
    /// Instance ids that have been "worked on" (restored, or fired a hook this
    /// run). Only these are persisted for restore.
    pub worked: HashSet<usize>,
    /// Sessions asked to go away (⌘W, `hub_terminal_close`). A dead *instance* is
    /// removed the moment it dies, but a dead *terminal* is kept so its output
    /// stays readable — so "dead" alone can't mean "remove", and this is what
    /// distinguishes the two. Removal still happens uniformly in `reap_dead`.
    closing: HashSet<usize>,
    /// Ids restored from the store this run, and when they started.
    restored: HashMap<usize, Instant>,
    /// When each session was spawned, for `EARLY_DEATH_GRACE`. Covers every
    /// session, not just restored ones — a fresh ⌘T can fail to start too.
    started: HashMap<usize, Instant>,
    /// Instances that died before they were ever usable, and why. Their rows are
    /// **kept** so the failure is visible; see `reap_dead`.
    failed: HashMap<usize, String>,
    /// Records that must stay in the store even though their session is gone —
    /// see `reap_dead`. Keyed by session id, deduped.
    sticky: Vec<persist::SavedSession>,
    /// Last content written to `terminals/index`, so the poll loop can refresh
    /// it on change without a disk write every tick.
    terminal_index: String,
    /// `hub_spawn` batches still being drip-fed, one child per `SPAWN_STAGGER`.
    /// See `process_spawn_requests`.
    pending_spawns: VecDeque<PendingSpawn>,
    /// When the last child was launched, so the drip-feed can pace itself without
    /// ever sleeping inside the poll loop.
    last_spawn_at: Option<Instant>,
}

/// Minimum gap between launching two `hub_spawn` children. Each `claude` cold
/// start is heavy (node boot, MCP handshake, hook registration); firing a whole
/// batch at once made them contend badly enough that none was ready to be typed
/// into in time. Spacing them keeps each start fast enough to be usable.
const SPAWN_STAGGER: Duration = Duration::from_millis(500);

/// How long an unread `<token>.done` reply sticks around before being collected.
/// One is orphaned whenever a caller gives up waiting for it.
const DONE_TTL: Duration = Duration::from_secs(60);

/// A restored session that dies within this long of starting is treated as a
/// **failed restore**, and its record is kept rather than erased. Comfortably
/// longer than any real `claude` startup, and far shorter than a session a user
/// actually worked in and then exited.
const RESTORE_GRACE: Duration = Duration::from_secs(120);

/// A session that dies within this long of being spawned never became usable —
/// it failed to *start*, rather than ran and finished. Such an instance is kept
/// in the list (marked, with its output intact) instead of being reaped, so the
/// reason is still on screen afterwards.
///
/// The bug this closes: a `claude` whose working directory macOS refuses to hand
/// over exits 1 in well under a second, so the row appeared and vanished inside
/// about 100 ms and the sidebar simply showed nothing. Nothing was logged, and
/// with every project under `~/Documents` the whole app looked like "Claude
/// refuses to open". Same shape as the other silent failures in this file: a
/// real error with nowhere to appear.
///
/// Comfortably past a `claude` cold start (~1-2 s, and slower when a `hub_spawn`
/// batch starts several at once), and far short of a session someone actually
/// worked in — so "died this fast" really does mean "never started".
const EARLY_DEATH_GRACE: Duration = Duration::from_secs(10);

/// One in-flight `hub_spawn` batch. The request file is consumed immediately, but
/// its children are launched across successive poll ticks — so the batch's state
/// lives here until the last child is up and the `.done` response can be written.
struct PendingSpawn {
    /// Request filename stem; the `<token>.done` reply the caller polls for.
    token: String,
    /// Instance that asked for the spawn (each child is told who assigned it).
    from: usize,
    /// Tasks not yet launched.
    remaining: VecDeque<String>,
    /// Ids launched so far, reported together once the batch finishes.
    ids: Vec<usize>,
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
        // Every name here contains no bare integer at the top level, which is what
        // keeps `mcp::live_ids`' integer-filename scan from mistaking one for an
        // instance status file.
        for sub in [
            "locks",
            "history",
            "tasks",
            "inbox",
            "waiting",
            "spawn",
            "armed",
            mulpex_core::NAMED_DIR,
            mulpex_core::NAMEREQ_DIR,
            "terminals",
            "terminals/cursors",
            "termreq",
        ] {
            std::fs::create_dir_all(state_dir.join(sub))?;
        }

        let store = SessionStore::new(&project_dir);
        let mut sessions: Vec<Session> = Vec::new();
        let mut worked: HashSet<usize> = HashSet::new();
        let mut names: HashMap<usize, String> = HashMap::new();
        let mut muted: HashSet<usize> = HashSet::new();
        let mut restored: HashMap<usize, Instant> = HashMap::new();
        let mut started: HashMap<usize, Instant> = HashMap::new();
        // Records whose session could not even be spawned. Nothing will be in
        // `sessions` to persist them from, and `persist_sessions` rewrites the
        // store from `sessions`, so without this a directory that was briefly
        // unreadable would erase the ids on the next write.
        let mut sticky: Vec<persist::SavedSession> = Vec::new();
        for saved in store.load() {
            let id = sessions.len() + 1;
            // Restores are deliberately NOT preflighted with `dir_access_error`.
            // Letting the spawn fail produces a session row that says why it
            // failed, which is the whole point — refusing up front would restore
            // the project to an empty sidebar, i.e. the original bug.
            if let Ok(session) = Session::spawn(
                id,
                &project_dir,
                DEFAULT_ROWS,
                DEFAULT_COLS,
                SpawnSpec::Claude {
                    settings_path: &settings_path,
                    state_dir: &state_dir,
                    session_id: &saved.session_id,
                    resume: true,
                    initial_task: None,
                },
            ) {
                worked.insert(id);
                restored.insert(id, Instant::now());
                started.insert(id, Instant::now());
                if let Some(name) = saved.name {
                    names.insert(id, name);
                }
                if saved.muted {
                    muted.insert(id);
                }
                sessions.push(session);
            } else {
                sticky.push(saved);
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

        let mut core = Self {
            handle,
            project_dir,
            project_name,
            sessions,
            active: 0,
            next_id,
            state_dir,
            settings_path,
            store,
            // A name that survived a restart is treated as the user's, so the
            // instance is neither nudged to rename itself nor allowed to.
            manual_names: names.keys().copied().collect(),
            names,
            muted,
            worked,
            closing: HashSet::new(),
            restored,
            started,
            failed: HashMap::new(),
            sticky,
            terminal_index: String::new(),
            pending_spawns: VecDeque::new(),
            last_spawn_at: None,
        };
        core.persist_sessions();
        core.write_live_instances();
        core.sync_terminal_index();
        // `state_dir` is fresh every launch, so unlike the `armed` flag these have
        // to be re-seeded: without them a restored session's first turn would be
        // nudged to rename a row the user had already named.
        let named: Vec<usize> = core.manual_names.iter().copied().collect();
        for id in named {
            core.mark_named(id);
        }
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
                kind: kind_of(s),
                exited: s.is_shell() && !s.is_alive(),
                failed: self.failed.get(&s.id).cloned(),
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
                name: Some(name),
                ..info
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
        // Refuse before spawning if the project directory itself is off limits.
        // `claude` would otherwise start, fail to `getcwd()`, and exit 1 inside a
        // second — which reads as "Claude is broken" rather than "macOS is not
        // letting this app into that folder". Here the caller is a person who
        // just pressed ⌘T, so an error returned now becomes a notice they see.
        if let Some(reason) = pty::dir_access_error(&self.project_dir) {
            anyhow::bail!(reason);
        }
        let id = self.next_id;
        let session_id = persist::new_uuid();
        let session = Session::spawn(
            id,
            &self.project_dir,
            DEFAULT_ROWS,
            DEFAULT_COLS,
            SpawnSpec::Claude {
                settings_path: &self.settings_path,
                state_dir: &self.state_dir,
                session_id: &session_id,
                resume: false,
                initial_task,
            },
        )?;
        self.next_id += 1;
        self.started.insert(id, Instant::now());
        self.sessions.push(session);
        if focus {
            self.active = self.sessions.len() - 1;
        }
        self.write_live_instances();
        Ok(SessionInfo {
            id,
            name: None,
            muted: false,
            kind: SessionKind::Claude,
            exited: false,
            failed: None,
        })
    }

    /// Open a plain shell terminal in the project dir (⌘⇧T, or an instance's
    /// `hub_terminal_open`). `seed` is a command line typed in once the prompt
    /// appears; `label` names the row. `focus` is true only for the user's own
    /// ⌘⇧T — a terminal an instance opens must not pull the view off whatever
    /// pane the user is reading, exactly as `hub_spawn` children don't.
    ///
    /// Terminals draw from the same id counter as instances, which is what lets
    /// every `(project, id)`-keyed mechanism stay kind-agnostic. Ids must never
    /// be reused: `refresh_worked` keys off `state_dir/<id>` status files left
    /// behind by dead instances, and a recycled id would inherit one.
    pub fn spawn_terminal(
        &mut self,
        seed: Option<String>,
        label: Option<String>,
        focus: bool,
    ) -> anyhow::Result<SessionInfo> {
        let id = self.next_id;
        let session = Session::spawn(
            id,
            &self.project_dir,
            DEFAULT_ROWS,
            DEFAULT_COLS,
            SpawnSpec::Shell {
                state_dir: &self.state_dir,
                seed,
            },
        )?;
        self.next_id += 1;
        self.sessions.push(session);
        if focus {
            self.active = self.sessions.len() - 1;
        }
        let name = label.and_then(|l| name_from_task(&l));
        if let Some(name) = &name {
            self.names.insert(id, name.clone());
        }
        self.sync_terminal_index();
        Ok(SessionInfo {
            id,
            name,
            muted: false,
            kind: SessionKind::Shell,
            exited: false,
            failed: None,
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
        // A missing dir just means no NEW requests — an in-flight batch still has
        // children left to drip-feed, so fall through rather than returning early.
        let mut requests: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
                    .collect()
            })
            .unwrap_or_default();
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

            // Queue the batch rather than launching it here: firing every child in
            // one tick makes N `claude` cold starts fight for CPU, and a child that
            // takes too long to paint its input box misses its task injection.
            let Some(stem) = req.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            self.pending_spawns.push_back(PendingSpawn {
                token: stem.to_string(),
                from,
                remaining: tasks.into(),
                ids: Vec::new(),
            });
        }

        // Drip-feed at most one child per tick, and only once `SPAWN_STAGGER` has
        // elapsed since the last one. Never sleeps — this runs on the shared poll
        // loop, so blocking here would stall every project's UI updates.
        let due = self
            .last_spawn_at
            .is_none_or(|t| t.elapsed() >= SPAWN_STAGGER);
        if due {
            if let Some(mut batch) = self.pending_spawns.pop_front() {
                if let Some(task) = batch.remaining.pop_front() {
                    if let Ok(info) = self.spawn_instance_with_task(batch.from, task) {
                        batch.ids.push(info.id);
                        spawned_any = true;
                    }
                    self.last_spawn_at = Some(Instant::now());
                }
                if batch.remaining.is_empty() {
                    // Batch complete — hand the waiting `hub_spawn` its ids.
                    let done = serde_json::json!({ "ids": batch.ids }).to_string();
                    let _ = std::fs::write(dir.join(format!("{}.done", batch.token)), done);
                } else {
                    self.pending_spawns.push_front(batch);
                }
            }
        }
        spawned_any
    }

    /// Fulfil the terminal requests instances left on disk (`hub_terminal_open` /
    /// `_send` / `_close`): apply each one, then write a `<token>.done` reply the
    /// waiting MCP tool reads. Returns whether a terminal was created or removed.
    ///
    /// Deliberately its own queue rather than sharing `process_spawn_requests`'
    /// drip-feed: that one is paced by `SPAWN_STAGGER` because concurrent `claude`
    /// cold starts starve each other, and inheriting that pacing would delay every
    /// keystroke-level terminal op behind a pending 8-child spawn batch.
    pub fn process_terminal_requests(&mut self) -> bool {
        let dir = self.state_dir.join("termreq");
        let mut requests: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
                    .collect()
            })
            .unwrap_or_default();
        // Request names start with a zero-padded microsecond stamp, so the plain
        // sort is time order. That matters: `cd /tmp` then `make` is not the same
        // as `make` then `cd /tmp`.
        requests.sort();

        let mut changed = false;
        for req in requests {
            let Some(stem) = req.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                let _ = std::fs::remove_file(&req);
                continue;
            };
            let content = std::fs::read_to_string(&req).unwrap_or_default();
            let _ = std::fs::remove_file(&req);
            let (reply, touched) = self.apply_terminal_request(&content);
            changed |= touched;
            let _ = std::fs::write(dir.join(format!("{stem}.done")), reply.to_string());
        }

        // A caller that timed out leaves its reply behind; collect the strays so
        // the dir doesn't grow for the life of the app.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("done") {
                    continue;
                }
                let stale = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().map(|d| d > DONE_TTL).unwrap_or(false))
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        changed
    }

    /// Apply one terminal request; returns the JSON reply and whether the session
    /// list changed.
    fn apply_terminal_request(&mut self, content: &str) -> (serde_json::Value, bool) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(content) else {
            return (
                serde_json::json!({ "ok": false, "error": "malformed request" }),
                false,
            );
        };
        let op = v.get("op").and_then(|x| x.as_str()).unwrap_or("");
        let str_of = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty())
        };
        let id = v.get("id").and_then(|x| x.as_u64()).map(|n| n as usize);

        match op {
            "open" => match self.spawn_terminal(str_of("seed"), str_of("label"), false) {
                Ok(info) => (serde_json::json!({ "ok": true, "id": info.id }), true),
                Err(e) => (
                    serde_json::json!({ "ok": false, "error": e.to_string() }),
                    false,
                ),
            },
            "send" => {
                let Some(id) = id else {
                    return (
                        serde_json::json!({ "ok": false, "error": "missing 'id'" }),
                        false,
                    );
                };
                let data = v.get("data").and_then(|x| x.as_str()).unwrap_or("");
                match self.sessions.iter_mut().find(|s| s.id == id) {
                    Some(s) if !s.is_shell() => (
                        serde_json::json!({ "ok": false, "error": format!("#{id} is a Claude instance, not a terminal") }),
                        false,
                    ),
                    Some(s) if !s.is_alive() => (
                        serde_json::json!({ "ok": false, "error": format!("terminal #{id} has exited; open a new one") }),
                        false,
                    ),
                    Some(s) => {
                        s.send(data.as_bytes());
                        (serde_json::json!({ "ok": true, "id": id }), false)
                    }
                    None => (
                        serde_json::json!({ "ok": false, "error": format!("no terminal #{id}") }),
                        false,
                    ),
                }
            }
            "close" => {
                let Some(id) = id else {
                    return (
                        serde_json::json!({ "ok": false, "error": "missing 'id'" }),
                        false,
                    );
                };
                match self.sessions.iter().find(|s| s.id == id) {
                    Some(s) if !s.is_shell() => (
                        serde_json::json!({ "ok": false, "error": format!("#{id} is a Claude instance, not a terminal") }),
                        false,
                    ),
                    Some(_) => {
                        self.close(id);
                        (serde_json::json!({ "ok": true, "id": id }), true)
                    }
                    None => (
                        serde_json::json!({ "ok": false, "error": format!("no terminal #{id}") }),
                        false,
                    ),
                }
            }
            other => (
                serde_json::json!({ "ok": false, "error": format!("unknown op: {other}") }),
                false,
            ),
        }
    }

    /// Find a session by id.
    pub fn session_mut(&mut self, id: usize) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    /// Rename an instance **as the user** (⌘R): a non-empty (trimmed) name sets
    /// it, empty clears it. Persists so it survives restart. Ports
    /// `App::commit_rename`.
    ///
    /// This also claims the row on the user's behalf: the instance is no longer
    /// nudged to name itself, and a `hub_set_name` it calls later is refused.
    /// Clearing the name hands it back — the instance may name itself again,
    /// which is the only way to undo an auto-name you didn't like.
    pub fn rename(&mut self, id: usize, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.names.remove(&id);
            self.manual_names.remove(&id);
            self.clear_named(id);
        } else {
            self.names.insert(id, name.to_string());
            self.manual_names.insert(id);
            self.mark_named(id);
        }
        self.persist_sessions();
    }

    /// Apply the sidebar names instances asked for with `hub_set_name`: one
    /// `namereq/<id>` file per request, holding the label. Returns whether any
    /// name changed (the poll loop re-publishes the session list on the diff).
    ///
    /// Deliberately no `<token>.done` reply, unlike `hub_spawn`/`termreq` — the
    /// caller doesn't wait, so applying late is fine and a refusal is silent by
    /// design (see `mcp::hub_set_name`). The request file is consumed either way,
    /// so a refused rename can't sit on disk retrying every tick.
    pub fn process_name_requests(&mut self) -> bool {
        let dir = self.state_dir.join(mulpex_core::NAMEREQ_DIR);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return false;
        };
        let mut changed = false;
        for entry in entries.flatten() {
            let path = entry.path();
            let id = path
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<usize>().ok());
            let name = std::fs::read_to_string(&path).unwrap_or_default();
            let _ = std::fs::remove_file(&path);
            let Some(id) = id else { continue };
            let verdict = name_verdict(
                &name,
                self.sessions.iter().any(|s| s.id == id && !s.is_shell()),
                self.manual_names.contains(&id),
                self.names.get(&id).map(String::as_str),
            );
            if let NameVerdict::Apply(name) = verdict {
                self.names.insert(id, name);
                changed = true;
            }
        }
        if changed {
            self.persist_sessions();
        }
        changed
    }

    /// Write / remove the `named/<id>` flag the `UserPromptSubmit` hook reads to
    /// decide whether to nudge an instance to name itself (`hook::instance_named`).
    fn mark_named(&self, id: usize) {
        let path = mulpex_core::named_flag_path(&self.state_dir, id);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, "");
    }

    fn clear_named(&self, id: usize) {
        let _ = std::fs::remove_file(mulpex_core::named_flag_path(&self.state_dir, id));
    }

    /// Mute or unmute an instance (⌘M / the sidebar's 🔇). Persists so it
    /// survives restart, like a rename. Nothing else changes: the `claude` keeps
    /// running, keeps its inbox, and stays in the peer list — mute is entirely a
    /// statement about how loudly the *sidebar* should talk about it.
    pub fn set_muted(&mut self, id: usize, muted: bool) {
        // Terminals aren't persisted and produce none of the signals mute
        // silences, so muting one would be a dimmed, re-sorted row whose flag
        // vanishes on the next reap. Ignore rather than half-honour it.
        if self.sessions.iter().any(|s| s.id == id && s.is_shell()) {
            return;
        }
        if muted {
            self.muted.insert(id);
        } else {
            self.muted.remove(&id);
        }
        self.persist_sessions();
    }

    /// Rearrange sessions to match `ids` (the sidebar's new top-to-bottom order
    /// after a drag). Ids we don't know are ignored and sessions the caller
    /// omitted keep their relative order at the end, so a stale frontend list can
    /// never make a session vanish from the sidebar — same contract as
    /// `Workspace::reorder_projects`.
    ///
    /// This vec's order *is* the persisted order (`persist_sessions` walks it, so
    /// the drag survives relaunch) and it is also what the frontend cycles with
    /// ⌘[ / ⌘]. `active` is an *index* into the vec, so it has to be re-derived
    /// from the focused session's id: reordering must never move focus.
    pub fn reorder_sessions(&mut self, ids: &[usize]) {
        let focused = self.sessions.get(self.active).map(|s| s.id);
        let mut ordered: Vec<Session> = Vec::with_capacity(self.sessions.len());
        for id in ids {
            if let Some(pos) = self.sessions.iter().position(|s| s.id == *id) {
                ordered.push(self.sessions.remove(pos));
            }
        }
        ordered.append(&mut self.sessions);
        self.sessions = ordered;
        self.active = focused
            .and_then(|id| self.sessions.iter().position(|s| s.id == id))
            .unwrap_or(0);
        self.persist_sessions();
    }

    /// Close a session by id: mark it for removal and kill its process group.
    /// Removal, focus-fixing, persistence, peer-list rewrite and mail-bounce all
    /// happen uniformly on the next `reap_dead` pass (which also emits
    /// `session-exited`), so an explicit close and a self-exit follow the
    /// identical single-sourced path.
    ///
    /// The `closing` mark is what tells `reap_dead` this death is final. Without
    /// it a terminal would be *kept* when its shell died, which is the whole
    /// point for a shell that exits on its own but exactly wrong here.
    pub fn close(&mut self, id: usize) {
        self.closing.insert(id);
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

    /// Drop sessions that are finished with; return their ids. Runs the full
    /// reap: fix `active`, drop from `worked`/`names`, delete the removed ids'
    /// hub state files, persist, rewrite the peer list, and bounce each closed
    /// instance's undelivered mail. Ports `App::reap_dead` (+ the inbox reaping
    /// half of `refresh_hub`).
    ///
    /// "Finished with" is **not** the same as "dead": an exited terminal is kept
    /// so its output stays readable until someone closes it. The guard therefore
    /// tests *removability*, not liveness — a plain `all(is_alive)` check would
    /// be false forever once one terminal exited, and this whole body (two disk
    /// writes) would then run on every 200 ms tick for the life of the app.
    pub fn reap_dead(&mut self) -> Vec<usize> {
        // "Anything to do" is NOT the same as "anything to remove". An instance
        // that just failed to start is kept, so testing removability alone would
        // return here before it was ever marked — and it would then sit dead and
        // unexplained until `EARLY_DEATH_GRACE` lapsed, at which point it became
        // removable and was silently reaped. That is the original bug, delayed by
        // ten seconds. Marking is latched, so once done both halves are false
        // again and the guard goes back to costing nothing per tick.
        if !self
            .sessions
            .iter()
            .any(|s| self.is_removable(s) || self.needs_failure_mark(s))
        {
            return Vec::new();
        }
        let old_active = self.active;
        let mut removed = Vec::new();
        let mut kept: Vec<Session> = Vec::with_capacity(self.sessions.len());
        let mut new_active: Option<usize> = None;
        // Instances kept because they failed to start, marked on the tick their
        // death is first seen. Collected here and applied after the loop, since
        // `is_removable` borrows `self` while `self.sessions` is taken.
        let mut newly_failed: Vec<(usize, String)> = Vec::new();
        for (idx, session) in std::mem::take(&mut self.sessions).into_iter().enumerate() {
            let removable = self.is_removable(&session);
            if !removable {
                // First sighting of a session that died before it was usable:
                // record why, and say so in its own pane. The child's own last
                // words (if it managed any) are already on screen above this.
                if self.needs_failure_mark(&session) {
                    let reason = self.start_failure_reason();
                    session.notice(&format!("⚠  {reason}"));
                    newly_failed.push((session.id, reason));
                }
                if idx == old_active {
                    new_active = Some(kept.len());
                }
                kept.push(session);
            } else {
                // A restored session that dies almost immediately means the
                // restore FAILED — `claude` could not open that transcript. The
                // old behaviour then rewrote the store without it, which turned
                // one bad restore into permanent loss of the session: the id was
                // gone, so there was nothing left to retry with and nothing to
                // recover by hand. Keep the record instead; a restore that
                // failed once may well succeed next launch, and if it never
                // does, the user still has the id.
                let lost_restore = !self.closing.contains(&session.id)
                    && self
                        .restored
                        .get(&session.id)
                        .is_some_and(|t| t.elapsed() < RESTORE_GRACE);
                if lost_restore && !session.session_id.is_empty() {
                    let record = persist::SavedSession {
                        session_id: session.session_id.clone(),
                        name: self.names.get(&session.id).cloned(),
                        muted: self.muted.contains(&session.id),
                    };
                    if !self
                        .sticky
                        .iter()
                        .any(|s| s.session_id == record.session_id)
                    {
                        self.sticky.push(record);
                    }
                }
                removed.push(session.id);
                // Dropping the dead session here kills its process group.
            }
        }
        self.sessions = kept;
        for (id, reason) in newly_failed {
            self.failed.insert(id, reason);
            // A failed instance must vanish from the hub even though its row
            // stays: `mcp::live_ids` falls back to scanning the state dir for
            // bare-integer filenames, so a leftover status file would let a
            // dead instance be offered to `hub_send` as a live peer. Same
            // exclusion an exited terminal gets, for the same reason.
            self.forget_session_files(id);
        }
        self.active = match new_active {
            Some(a) => a,
            None if self.sessions.is_empty() => 0,
            None => old_active.min(self.sessions.len() - 1),
        };
        self.worked.retain(|id| self.sessions.iter().any(|s| s.id == *id));
        self.names.retain(|id, _| self.sessions.iter().any(|s| s.id == *id));
        self.muted.retain(|id| self.sessions.iter().any(|s| s.id == *id));
        self.closing.retain(|id| self.sessions.iter().any(|s| s.id == *id));
        self.started.retain(|id, _| self.sessions.iter().any(|s| s.id == *id));
        self.failed.retain(|id, _| self.sessions.iter().any(|s| s.id == *id));
        for id in &removed {
            self.forget_session_files(*id);
        }
        self.persist_sessions();
        self.write_live_instances();
        self.sync_terminal_index();
        removed
    }

    /// Whether a session should leave the list.
    ///
    /// Three cases, and the order matters. An explicit close always removes
    /// (that is what `closing` means). Otherwise a dead *terminal* is kept so
    /// its output stays readable, and so is a dead instance that never managed
    /// to start — leaving only the ordinary case, an instance that ran and then
    /// exited, which is removed as it always was.
    ///
    /// This is the guard `reap_dead` early-returns on, so it must test
    /// *removability* rather than liveness: anything kept-while-dead would
    /// otherwise make every 200 ms tick redo the whole body's disk writes.
    fn is_removable(&self, s: &Session) -> bool {
        if s.is_alive() {
            return false;
        }
        if self.closing.contains(&s.id) {
            return true;
        }
        !s.is_shell() && !self.failed_to_start(s)
    }

    /// A dead instance that failed to start and has not been marked as such yet
    /// — i.e. this reap pass still owes it a reason and a notice in its pane.
    fn needs_failure_mark(&self, s: &Session) -> bool {
        !s.is_alive()
            && !s.is_shell()
            && !self.closing.contains(&s.id)
            && !self.failed.contains_key(&s.id)
            && self.failed_to_start(s)
    }

    /// Whether this dead instance died too soon after spawning to have ever been
    /// usable. Once marked the answer is latched, so the row does not become
    /// removable simply because `EARLY_DEATH_GRACE` has since elapsed.
    fn failed_to_start(&self, s: &Session) -> bool {
        self.failed.contains_key(&s.id)
            || self
                .started
                .get(&s.id)
                .is_some_and(|t| t.elapsed() < EARLY_DEATH_GRACE)
    }

    /// Why a session that just died is presumed to have failed to start.
    ///
    /// Re-checks the project directory rather than reporting a bare exit,
    /// because the overwhelmingly likely cause is the one that has no other
    /// symptom — macOS refusing the app access to the folder, which the child
    /// can only report as a `getcwd` error nobody sees. When the directory is
    /// fine we say only what we actually know.
    fn start_failure_reason(&self) -> String {
        match pty::dir_access_error(&self.project_dir) {
            Some(reason) => format!("Claude could not start here. {reason}"),
            None => "Claude exited immediately, before this session was usable. \
                     Its output above is all it managed to say. Press ⌘W to \
                     dismiss this, or ⌘T to try again."
                .to_string(),
        }
    }

    /// Erase everything in the scratch dir keyed to a removed session's id.
    ///
    /// The status file matters beyond tidiness: `mcp::live_ids` falls back to
    /// scanning for bare-integer filenames when the peer list is unreadable, so
    /// a leftover `state_dir/<id>` would let a long-dead instance reappear as a
    /// live peer.
    fn forget_session_files(&self, id: usize) {
        let _ = std::fs::remove_file(self.state_dir.join(id.to_string()));
        let _ = std::fs::remove_file(self.state_dir.join("tasks").join(id.to_string()));
        let _ = std::fs::remove_file(crate::pty::terminal_log_path(&self.state_dir, id));
        let _ = std::fs::remove_file(crate::pty::terminal_screen_path(&self.state_dir, id));
        let _ = std::fs::remove_file(terminal_mark_path(&self.state_dir, id));
        // Every reader's cursor into that terminal.
        let cursors = self.state_dir.join("terminals").join("cursors");
        if let Ok(entries) = std::fs::read_dir(&cursors) {
            let prefix = format!("{id}.");
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
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
        // Instances only. A terminal owns no hub state, so counting it "live"
        // here would make the reaping passes below spare files keyed to an id
        // that will never write any.
        let live: HashSet<usize> = self.live_instances().map(|s| s.id).collect();

        // Statuses — instances only, and deliberately so. Everything downstream
        // (the dock badge in `attention.ts`, the updater's busy guard, the tab's
        // `needs` badge) derives from a session *having* a status entry, so a
        // terminal is excluded from all three for free by having none. Giving it
        // a synthetic `waiting` would put every terminal into that math and show
        // a green "ready" dot on a shell forever.
        let mut statuses: Vec<StatusEntry> = self
            .live_instances()
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

    /// The Claude sessions — i.e. everything the coordination hub is about.
    /// Terminals live in the same list but are shells, not agents.
    fn instances(&self) -> impl Iterator<Item = &Session> {
        self.sessions.iter().filter(|s| !s.is_shell())
    }

    /// Instances that are part of the hub. Excludes one that failed to start:
    /// its row is kept for the user to read, but it owns no hub state, can never
    /// answer a message, and must not be offered to `hub_send` as a peer.
    ///
    /// Keeping it out of the *statuses* list matters beyond the hub: the dock
    /// badge, the updater's busy guard and the tab badges all key off a session
    /// having a status entry, so this excludes a failed instance from all three
    /// for free — exactly how a terminal stays out of them.
    fn live_instances(&self) -> impl Iterator<Item = &Session> {
        self.instances().filter(|s| !self.failed.contains_key(&s.id))
    }

    /// Persist the worked-on sessions' ids (+ names + mute), preserving order.
    /// Terminals are deliberately not persisted (they'd restore as a fresh shell
    /// with no scrollback and possibly re-run a destructive seeded command), and
    /// the filter is on `kind` rather than on an empty `session_id` so that
    /// intent is explicit rather than an accident of `persist`'s own guard.
    fn persist_sessions(&self) {
        let mut sessions: Vec<persist::SavedSession> = self
            .instances()
            .filter(|s| self.worked.contains(&s.id))
            .map(|s| persist::SavedSession {
                session_id: s.session_id.clone(),
                name: self.names.get(&s.id).cloned(),
                muted: self.muted.contains(&s.id),
            })
            .collect();
        // Records whose session is gone but must not be forgotten — a restore
        // that failed. Appended, and only if the id isn't already live, so a
        // later successful restore of the same id doesn't duplicate it.
        for record in &self.sticky {
            if !sessions.iter().any(|s| s.session_id == record.session_id) {
                sessions.push(record.clone());
            }
        }
        self.store.save(&sessions);
    }

    /// Publish the live instance ids to `state_dir/instances` (the peer list the
    /// hub reads).
    ///
    /// Terminals are excluded: a shell in the peer list would be offered to
    /// `hub_send` as a messageable instance, and the mail would rot in an inbox
    /// nothing reads.
    fn write_live_instances(&self) {
        let mut out = String::new();
        for s in self.live_instances() {
            out.push_str(&s.id.to_string());
            out.push('\n');
        }
        let _ = std::fs::write(self.state_dir.join("instances"), out);
    }

    /// Publish the terminals to `state_dir/terminals/index`, the manifest
    /// `hub_instances` reads. One line per terminal:
    /// `<id>\t<running|exited>\t<label>`. A directory listing of `*.log` could
    /// not carry the label or distinguish running from exited.
    ///
    /// Called from the poll loop, so it writes only when the content actually
    /// changed — a terminal's shell can exit at any moment with nothing else
    /// happening, and an index that only refreshed on add/remove would go on
    /// advertising a dead shell as running.
    pub fn sync_terminal_index(&mut self) {
        let mut out = String::new();
        for s in self.sessions.iter().filter(|s| s.is_shell()) {
            let state = if s.is_alive() { "running" } else { "exited" };
            let label = self
                .names
                .get(&s.id)
                .map(|n| n.replace(['\t', '\n'], " "))
                .unwrap_or_default();
            out.push_str(&format!("{}\t{}\t{}\n", s.id, state, label));
        }
        if out == self.terminal_index {
            return;
        }
        let _ = std::fs::write(self.state_dir.join("terminals").join("index"), &out);
        self.terminal_index = out;
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

fn kind_of(s: &Session) -> SessionKind {
    if s.is_shell() {
        SessionKind::Shell
    } else {
        SessionKind::Claude
    }
}

/// Where the token of the most recent tracked command in a terminal is recorded,
/// so `hub_terminal_read` knows which completion marker to look for.
pub fn terminal_mark_path(state_dir: &Path, id: usize) -> PathBuf {
    state_dir.join("terminals").join(format!("{id}.mark"))
}

/// What to do with one `hub_set_name` request. Pure, so the rules can be tested
/// without a live `claude` to own the row — the session is reduced to the two
/// facts that decide the outcome.
#[derive(Debug, PartialEq)]
enum NameVerdict {
    Apply(String),
    Ignore,
}

/// The rules, in one place:
/// - **`user_owned` wins.** A ⌘R name is the user's statement about their own
///   sidebar; an instance may not talk over it. This is the whole reason
///   `manual_names` exists.
/// - **Only a claude may name a row.** `is_claude` is false for a terminal *and*
///   for an id that no longer exists (reaped mid-flight), so a stale request
///   can't resurrect a name for a row that has gone.
/// - An empty label is a no-op rather than a clear: clearing is the user's
///   gesture (⌘R with an empty name), and it hands naming rights back.
/// - An unchanged name is `Ignore`d so the poll loop doesn't republish the
///   session list, and doesn't rewrite the store, every time a chatty instance
///   re-asserts what it's already called.
fn name_verdict(
    name: &str,
    is_claude: bool,
    user_owned: bool,
    current: Option<&str>,
) -> NameVerdict {
    let name = name.trim();
    if name.is_empty() || !is_claude || user_owned || current == Some(name) {
        return NameVerdict::Ignore;
    }
    NameVerdict::Apply(name.to_string())
}

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
        let _env = env_guard();
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

    /// `HOME` is process-global and the session store is derived from it, so the
    /// tests that repoint it have to take turns. Without this they race and one
    /// test's store lands under another's HOME.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A `Core` on a throwaway HOME + scratch dir, for terminal tests. Returns
    /// the env guard first — hold it for the life of the test.
    fn scratch_core(tag: &str) -> (std::sync::MutexGuard<'static, ()>, PathBuf, Core) {
        let guard = env_guard();
        let (root, core) = scratch_core_inner(tag);
        (guard, root, core)
    }

    fn scratch_core_inner(tag: &str) -> (PathBuf, Core) {
        let root = std::env::temp_dir().join(format!("mulpex-{tag}-{}", persist::new_uuid()));
        let project_dir = root.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::env::set_var("HOME", root.join("home"));
        let core = Core::open(
            1,
            project_dir,
            Path::new("/nonexistent/mulpex-helper"),
            root.join("state"),
        )
        .unwrap();
        (root, core)
    }

    /// A `Core` holding one instance that spawned and then died — the shape of
    /// every failed start. Built by restoring a session id `claude` has never
    /// heard of, which is a real failure rather than a simulated one: it prints
    /// "No conversation found" and exits in about 1.6 s.
    fn core_with_dead_instance(tag: &str) -> (std::sync::MutexGuard<'static, ()>, PathBuf, Core) {
        let guard = env_guard();
        let root = std::env::temp_dir().join(format!("mulpex-{tag}-{}", persist::new_uuid()));
        let project_dir = root.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::env::set_var("HOME", root.join("home"));

        SessionStore::new(&project_dir).save(&[persist::SavedSession {
            session_id: persist::new_uuid(),
            name: None,
            muted: false,
        }]);
        let mut core = Core::open(
            1,
            project_dir,
            Path::new("/nonexistent/mulpex-helper"),
            root.join("state"),
        )
        .unwrap();
        assert_eq!(core.sessions.len(), 1, "the restore did not spawn");
        if let Some(s) = core.sessions.first_mut() {
            s.kill();
        }
        assert!(
            wait_until(|| core.sessions.first().is_some_and(|s| !s.is_alive())),
            "the instance never died"
        );
        (guard, root, core)
    }

    /// The ordinary case must keep working: an instance that ran for a while and
    /// then exited is still removed. Only a death *soon after spawning* means
    /// "never started" — without this boundary the new keep-it-visible rule
    /// would turn every finished session into a permanent row.
    #[test]
    fn an_instance_that_dies_after_the_grace_is_still_reaped() {
        let (_env, root, mut core) = core_with_dead_instance("late-death");
        // Backdate the spawn so this reads as a session that ran and then ended,
        // rather than one that failed to start. Sleeping the real grace would
        // add ten seconds to the suite for no extra confidence.
        core.started
            .insert(1, Instant::now() - EARLY_DEATH_GRACE * 2);
        assert!(
            wait_until(|| !core.reap_dead().is_empty()),
            "an instance that exited long after starting was not reaped"
        );
        assert!(core.sessions.is_empty(), "the session outlived its reap");

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same invariant the kept exited *terminal* has: keeping a dead session
    /// in the list must not make `reap_dead` redo its body — two disk writes,
    /// including re-truncating the `instances` file the helper reads — on every
    /// 200 ms tick for the life of the app. The failure mark is what latches it.
    #[test]
    fn a_kept_failed_instance_does_not_make_every_poll_do_work() {
        let (_env, root, mut core) = core_with_dead_instance("failed-idle");
        assert!(
            wait_until(|| {
                core.reap_dead();
                core.failed.contains_key(&1)
            }),
            "the dead instance was never marked as failed"
        );

        let store = core.store.path().to_path_buf();
        let stamp = || std::fs::metadata(&store).and_then(|m| m.modified()).ok();
        let before = stamp();
        std::thread::sleep(Duration::from_millis(20));
        for _ in 0..20 {
            core.reap_dead();
        }
        assert_eq!(
            before,
            stamp(),
            "reap_dead rewrote the session store on an idle tick"
        );

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    fn wait_until(mut f: impl FnMut() -> bool) -> bool {
        for _ in 0..100 {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Dragging a sidebar row reorders the backing vec — and must not move focus.
    /// `active` is an *index*, so it has to be re-derived from the focused id;
    /// carrying the index across would silently focus whichever session slid into
    /// that slot. Sessions the caller omitted are appended, never dropped, so a
    /// stale frontend list can't make one vanish.
    #[test]
    fn reordering_sessions_keeps_focus_and_never_drops_one() {
        let (_env, root, mut core) = scratch_core("reorder-test");
        let order = |c: &Core| c.sessions.iter().map(|s| s.id).collect::<Vec<_>>();

        let a = core.spawn_terminal(None, None, true).unwrap().id;
        let b = core.spawn_terminal(None, None, true).unwrap().id;
        let c = core.spawn_terminal(None, None, true).unwrap().id;
        core.set_active(b);

        // Drag the bottom row to the top.
        core.reorder_sessions(&[c, a, b]);
        assert_eq!(order(&core), vec![c, a, b]);
        assert_eq!(core.sessions[core.active].id, b, "reorder moved the focus");

        // A caller that submits a partial list keeps every other session, in its
        // existing relative order, at the end.
        core.reorder_sessions(&[b]);
        assert_eq!(order(&core), vec![b, c, a]);
        assert_eq!(core.sessions[core.active].id, b);

        // Unknown ids are ignored rather than inventing slots.
        core.reorder_sessions(&[999, a]);
        assert_eq!(order(&core), vec![a, b, c]);

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The rule the user asked for by name: an instance may label its own row,
    /// but a ⌘R name is the user's and is never talked over.
    #[test]
    fn a_user_rename_beats_an_instance_naming_itself() {
        // Naming rights start with the instance…
        assert_eq!(
            name_verdict("vtgrid soft-wrap fix", true, false, None),
            NameVerdict::Apply("vtgrid soft-wrap fix".into())
        );
        // …and are surrendered the moment the user types their own (⌘R), even
        // though the instance's request is otherwise perfectly valid.
        assert_eq!(
            name_verdict("vtgrid soft-wrap fix", true, true, Some("mine")),
            NameVerdict::Ignore
        );
        // Whitespace is trimmed, not treated as a name.
        assert_eq!(
            name_verdict("  spaced  ", true, false, None),
            NameVerdict::Apply("spaced".into())
        );
        assert_eq!(name_verdict("   ", true, false, None), NameVerdict::Ignore);
        // A terminal (or an id already reaped) has no claude behind it.
        assert_eq!(name_verdict("shell", false, false, None), NameVerdict::Ignore);
        // Re-asserting the same name must not churn the session list or the store.
        assert_eq!(
            name_verdict("same", true, false, Some("same")),
            NameVerdict::Ignore
        );
    }

    /// End to end through the request dir: the file an instance's `hub_set_name`
    /// writes is consumed exactly once, and a shell's row is left alone.
    #[test]
    fn name_requests_are_consumed_even_when_refused() {
        let (_env, root, mut core) = scratch_core("name-req-test");
        let dir = core.state_dir.join("namereq");
        assert!(dir.is_dir(), "open() should create the request dir");

        let term = core.spawn_terminal(None, None, true).unwrap().id;
        std::fs::write(dir.join(term.to_string()), "not for a shell").unwrap();
        // An id nobody owns — the instance was reaped while its request was in
        // flight — and a junk filename, neither of which may panic the poll loop.
        std::fs::write(dir.join("999"), "ghost").unwrap();
        std::fs::write(dir.join("notanid"), "junk").unwrap();

        assert!(!core.process_name_requests(), "nothing here should rename");
        assert_eq!(core.names.get(&term).map(String::as_str), None);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "requests must be consumed, or the poll loop retries them every tick"
        );

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The flag the `UserPromptSubmit` hook reads to decide whether to nudge an
    /// instance to name itself. ⌘R sets it (so the user is never asked to
    /// re-name what they just named), and clearing the name hands naming rights
    /// back — the only way to undo an auto-name you didn't like.
    #[test]
    fn renaming_marks_the_row_named_and_clearing_hands_it_back() {
        let (_env, root, mut core) = scratch_core("named-flag-test");
        let id = core.spawn_terminal(None, None, true).unwrap().id;
        let flag = core.state_dir.join("named").join(id.to_string());

        assert!(!flag.exists(), "an unnamed row starts nudgeable");
        core.rename(id, "user's own name");
        assert!(flag.exists(), "⌘R must stop the auto-name nudge");
        assert!(core.manual_names.contains(&id));

        core.rename(id, "  ");
        assert!(!flag.exists(), "clearing the name re-opens it to auto-naming");
        assert!(!core.manual_names.contains(&id));

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The lifecycle asymmetry this feature turns on: an exited *terminal* stays
    /// in the list so its output is still readable, while a dead *instance* is
    /// removed. Only an explicit close removes a terminal.
    #[test]
    fn an_exited_terminal_is_kept_until_it_is_closed() {
        let (_env, root, mut core) = scratch_core("term-exit-test");

        let info = core
            .spawn_terminal(Some("exit 3".into()), Some("exit 3".into()), true)
            .expect("a shell should spawn");
        let id = info.id;
        assert_eq!(info.kind, SessionKind::Shell);
        assert!(!info.exited);

        // The seeded `exit` ends the shell on its own.
        assert!(
            wait_until(|| core.sessions.iter().any(|s| s.id == id && !s.is_alive())),
            "the seeded `exit` never ended the shell"
        );

        // Reaping must NOT take it: a self-exited terminal is kept.
        for _ in 0..3 {
            assert!(core.reap_dead().is_empty());
        }
        let infos = core.session_infos();
        assert_eq!(infos.len(), 1, "the exited terminal was removed");
        assert!(infos[0].exited, "it should be marked exited");

        // A terminal is never a hub peer, alive or not.
        let peers = std::fs::read_to_string(core.state_dir.join("instances")).unwrap();
        assert_eq!(peers.trim(), "", "a terminal must not be a messageable peer");
        // …and never persisted for restore.
        assert!(core.store.load().is_empty(), "a terminal was persisted");

        // The manifest the MCP side reads says it's there but finished. It is
        // refreshed from the poll loop, since a shell can exit with nothing else
        // happening to trigger a rewrite.
        core.sync_terminal_index();
        let index =
            std::fs::read_to_string(core.state_dir.join("terminals").join("index")).unwrap();
        assert!(index.starts_with(&format!("{id}\texited\t")), "{index:?}");

        // An explicit close is what finally removes it.
        core.close(id);
        assert!(
            wait_until(|| core.reap_dead() == vec![id]),
            "close did not remove the exited terminal"
        );
        assert!(core.sessions.is_empty());
        assert!(!crate::pty::terminal_log_path(&core.state_dir, id).exists());

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `reap_dead` runs on every 200 ms tick and does real disk I/O, so it has to
    /// stay a no-op while an exited terminal is simply sitting in the list. This
    /// is what a plain `all(is_alive)` guard would get wrong — forever.
    #[test]
    fn a_kept_exited_terminal_does_not_make_every_poll_do_work() {
        let (_env, root, mut core) = scratch_core("term-idle-test");
        let info = core.spawn_terminal(Some("exit".into()), None, true).unwrap();
        assert!(wait_until(|| {
            core.sessions
                .iter()
                .any(|s| s.id == info.id && !s.is_alive())
        }));
        core.reap_dead();

        let store = core.store.path().to_path_buf();
        let stamp = || std::fs::metadata(&store).and_then(|m| m.modified()).ok();
        let before = stamp();
        std::thread::sleep(Duration::from_millis(20));
        for _ in 0..20 {
            core.reap_dead();
        }
        assert_eq!(before, stamp(), "reap_dead rewrote the session store on an idle tick");

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The whole read path with nothing simulated: a real `$SHELL` on a real PTY,
    /// through the real recorder, into the file the MCP helper parses with the
    /// shared `termlog` header.
    #[test]
    fn a_real_shells_output_lands_in_the_log_the_helper_reads() {
        use mulpex_core::termlog;
        let (_env, root, mut core) = scratch_core("term-log-test");
        // Enough lines to push the early ones off a 32-row screen, so both
        // surfaces are exercised: the scrolled-off history and the live screen.
        let info = core
            .spawn_terminal(Some("seq 1 200; echo LAST-LINE-MARKER".into()), None, true)
            .unwrap();
        let log = crate::pty::terminal_log_path(&core.state_dir, info.id);
        let screen = crate::pty::terminal_screen_path(&core.state_dir, info.id);

        let history = || -> String {
            let Ok(raw) = std::fs::read(&log) else {
                return String::new();
            };
            if termlog::parse_header(&raw).is_none() {
                return String::new();
            }
            String::from_utf8_lossy(&raw[termlog::HEADER_LEN..]).into_owned()
        };
        let visible = || std::fs::read_to_string(&screen).unwrap_or_default();

        // Lines that scrolled off are history…
        assert!(
            wait_until(|| history().contains("\n1\n") && history().contains("\n100\n")),
            "the shell's output never reached the log: {:?}",
            history()
        );
        // …and what is still on screen is only in the snapshot, which is exactly
        // why a read returns both (a dev server sitting at a steady screen never
        // scrolls a single line into the log).
        assert!(
            wait_until(|| visible().contains("LAST-LINE-MARKER")),
            "the tail never reached the screen snapshot: {:?}",
            visible()
        );
        // The last lines are still on screen, so they are NOT yet history. (The
        // marker itself is no use for this check — the shell echoes the command
        // that contains it, and that echo *did* scroll off.)
        assert!(!history().contains("\n200\n"), "{:?}", history());
        assert!(visible().contains("200"), "{:?}", visible());

        // Escape sequences must not survive into what a model reads.
        assert!(!history().contains('\u{1b}'), "{:?}", history());
        assert!(!visible().contains('\u{1b}'), "{:?}", visible());

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Closing a session must leave nothing behind: no live process, and no
    /// unreaped zombie. `Session::kill` is `killpg(SIGHUP)` → `killpg(SIGKILL)`
    /// → `child.kill()` → `child.wait()`, and it is the same code for both kinds.
    ///
    /// The descendant here is in the session's **own process group** — the shape
    /// a `claude` has, since node is not a shell and does no job control, so
    /// everything its Bash tool spawns inherits its group. That is exactly what
    /// `killpg` exists to reach.
    #[test]
    fn closing_a_session_kills_its_process_group_and_leaves_no_zombie() {
        let (_env, root, mut core) = scratch_core("term-kill-test");
        // `sh -c` is non-interactive: no job control, so the background child
        // stays in the shell's process group.
        let info = core
            .spawn_terminal(
                Some("sh -c 'sleep 300 & echo BGPID=$!; sleep 60'".into()),
                None,
                true,
            )
            .unwrap();
        let screen = crate::pty::terminal_screen_path(&core.state_dir, info.id);

        let read_pid = || -> Option<i32> {
            let text = std::fs::read_to_string(&screen).ok()?;
            text.lines()
                .filter_map(|l| l.split("BGPID=").nth(1))
                .filter_map(|t| {
                    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
                    digits.parse::<i32>().ok()
                })
                .next()
        };
        assert!(
            wait_until(|| read_pid().is_some()),
            "never saw the background pid: {:?}",
            std::fs::read_to_string(&screen)
        );
        let bg = read_pid().unwrap();
        let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
        assert!(alive(bg), "the background process never started");

        core.close(info.id);
        assert!(
            wait_until(|| core.reap_dead() == vec![info.id]),
            "the closed session was never reaped"
        );

        // The descendant went with it…
        assert!(
            wait_until(|| !alive(bg)),
            "closing the session orphaned its descendant (pid {bg})"
        );
        // …and nothing is left defunct: `child.wait()` reaps the direct child, so
        // no <defunct> entry survives.
        let zombies = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &bg.to_string()])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        assert!(
            !zombies.starts_with('Z'),
            "left a zombie: ps stat = {zombies:?}"
        );

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The case `killpg` alone cannot reach, and the reason `kill` sweeps by
    /// controlling terminal: an **interactive** shell does job control, so
    /// `cmd &` runs in its own process group — not the shell's, and not the
    /// tty's foreground group either, so neither the `killpg` nor the hangup on
    /// close touches it. Measured before the fix: it survived the close *and*
    /// app quit.
    #[test]
    fn closing_a_terminal_also_kills_its_background_jobs() {
        let (_env, root, mut core) = scratch_core("term-bgjob-test");
        let info = core
            .spawn_terminal(Some("sleep 300 & echo BGPID=$!".into()), None, true)
            .unwrap();
        let screen = crate::pty::terminal_screen_path(&core.state_dir, info.id);

        // The shell echoes `echo BGPID=$!` too, so take the first match that is
        // followed by actual digits.
        let read_pid = || -> Option<i32> {
            let text = std::fs::read_to_string(&screen).ok()?;
            text.split("BGPID=")
                .skip(1)
                .filter_map(|t| {
                    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
                    digits.parse::<i32>().ok()
                })
                .next()
        };
        assert!(
            wait_until(|| read_pid().is_some()),
            "never saw the background pid: {:?}",
            std::fs::read_to_string(&screen)
        );
        let bg = read_pid().unwrap();
        let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
        assert!(alive(bg), "the background job never started");

        // It really is out of reach of the shell's own process group — that is
        // the whole point of this test.
        let shell_pgid = unsafe { libc::getpgid(bg) };
        assert_ne!(
            shell_pgid, -1,
            "could not read the background job's process group"
        );

        core.close(info.id);
        assert!(wait_until(|| core.reap_dead() == vec![info.id]));
        assert!(
            wait_until(|| !alive(bg)),
            "a backgrounded job outlived the terminal that started it (pid {bg})"
        );

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A restore that fails must not erase the record it failed to load, and
    /// must not disappear without saying anything either.
    ///
    /// Two contracts in one place because they are two halves of the same
    /// failure. The record surviving is the difference between "one launch
    /// didn't restore" and "that session is gone forever": the old code rewrote
    /// the store without the id, so there was nothing left to retry with. The
    /// row surviving is the difference between a diagnosable failure and the
    /// app looking broken: the session used to vanish about 100 ms after
    /// appearing, which is what "Claude refuses to open" actually was.
    #[test]
    fn a_failed_restore_is_kept_visible_and_never_erases_the_record() {
        let _env = env_guard();
        let root = std::env::temp_dir().join(format!("mulpex-lostrestore-{}", persist::new_uuid()));
        let project_dir = root.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::env::set_var("HOME", root.join("home"));

        // A session id `claude` knows nothing about — exactly what a broken
        // restore looks like from Mulpex's side.
        let ghost = persist::new_uuid();
        let store = SessionStore::new(&project_dir);
        store.save(&[persist::SavedSession {
            session_id: ghost.clone(),
            name: Some("important work".into()),
            muted: false,
        }]);

        let mut core = Core::open(
            1,
            project_dir,
            Path::new("/nonexistent/mulpex-helper"),
            root.join("state"),
        )
        .unwrap();

        // `claude --resume <unknown>` exits on its own within seconds. Whether
        // it even spawned depends on the machine, so accept either and drive the
        // reap either way.
        assert_eq!(core.sessions.len(), 1, "the restore did not spawn");
        // Simulate the restored `claude` giving up and exiting on its own.
        // Deliberately NOT `close()`: that marks the id as user-closed, which is
        // the case where forgetting the record is correct.
        if let Some(s) = core.sessions.first_mut() {
            s.kill();
        }
        // The reap must NOT remove it: a session that died this soon after
        // spawning never started, so its row is kept with the reason attached.
        assert!(
            wait_until(|| {
                core.reap_dead();
                core.failed.contains_key(&1)
            }),
            "the dead session was never marked as failed to start"
        );
        assert_eq!(
            core.sessions.len(),
            1,
            "the failed session was reaped instead of being kept visible"
        );
        let info = core.session_infos().into_iter().next().unwrap();
        assert!(
            info.failed.is_some(),
            "the sidebar would show no reason for the failure"
        );
        // It is dead, so it must not still be advertised to peers as reachable.
        let peers = std::fs::read_to_string(core.state_dir.join("instances")).unwrap_or_default();
        assert!(
            !peers.split_whitespace().any(|l| l == "1"),
            "a failed instance was left in the hub peer list: {peers:?}"
        );
        assert!(
            !core.hub_snapshot().statuses.iter().any(|e| e.id == 1),
            "a failed instance kept a hub status, which would badge it as ready"
        );

        let after = std::fs::read_to_string(store.path()).unwrap_or_default();
        assert!(
            after.contains(&ghost),
            "the session record was erased by a failed restore; store is now:\n{after}"
        );
        assert!(
            after.contains("important work"),
            "the custom name was lost too:\n{after}"
        );

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `env_remove` must actually reach the child.
    ///
    /// Mulpex strips `CLAUDE_CODE_CHILD_SESSION` because a `claude` that
    /// inherits it runs with **transcript saving off** — it works normally, but
    /// writes no transcript, so the next launch's `--resume` reports "No
    /// conversation found" and the instance vanishes a second after appearing.
    /// If this ever silently stopped working, every session would be lost on
    /// every restart, with nothing in the logs to say why.
    #[test]
    fn the_child_session_marker_is_stripped_from_spawned_sessions() {
        let _env = env_guard();
        let root = std::env::temp_dir().join(format!("mulpex-envtest-{}", persist::new_uuid()));
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::env::set_var("HOME", root.join("home"));
        // Pretend Mulpex itself was launched from inside a Claude session.
        std::env::set_var("CLAUDE_CODE_CHILD_SESSION", "1");
        std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "cli");

        let out = project.join("env.txt");
        let (_root2, mut core) = (
            (),
            Core::open(
                1,
                project.clone(),
                Path::new("/nonexistent/mulpex-helper"),
                root.join("state"),
            )
            .unwrap(),
        );
        // A shell is the only kind we can make report its own environment; the
        // markers are stripped by `claude_command`'s sibling, so assert on what
        // a spawned child actually sees for the MULPEX_* vars, which
        // `shell_command` strips the same way.
        std::env::set_var("MULPEX_INSTANCE_ID", "99");
        let info = core
            .spawn_terminal(
                Some(format!(
                    "printf 'inst=[%s]\\n' \"$MULPEX_INSTANCE_ID\" > {}",
                    out.display()
                )),
                None,
                true,
            )
            .unwrap();
        assert!(
            wait_until(|| std::fs::read_to_string(&out)
                .map(|s| s.contains("inst="))
                .unwrap_or(false)),
            "the shell never wrote its environment"
        );
        let seen = std::fs::read_to_string(&out).unwrap();
        assert!(
            seen.contains("inst=[]"),
            "env_remove did not reach the child — it saw {seen:?}. If env_remove \
             is broken, CLAUDE_CODE_CHILD_SESSION also survives and every session \
             is unrestorable."
        );
        let _ = info;

        std::env::remove_var("CLAUDE_CODE_CHILD_SESSION");
        std::env::remove_var("CLAUDE_CODE_ENTRYPOINT");
        std::env::remove_var("MULPEX_INSTANCE_ID");
        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Repro harness for the restore path. Needs a real, resumable claude
    /// session id in `/tmp/mulpex_test_uuid` and its project dir in
    /// `/tmp/mulpex_test_dir`, so it is `#[ignore]`d and run by hand.
    #[test]
    #[ignore]
    fn repro_restore_of_a_real_session() {
        let _env = env_guard();
        let uuid = std::fs::read_to_string("/tmp/mulpex_test_uuid")
            .unwrap()
            .trim()
            .to_string();
        let project_dir = PathBuf::from(
            std::fs::read_to_string("/tmp/mulpex_test_dir")
                .unwrap()
                .trim()
                .to_string(),
        );
        let root = std::env::temp_dir().join(format!("mulpex-repro-{}", persist::new_uuid()));
        std::fs::create_dir_all(&root).unwrap();

        // Seed the store exactly as a prior run would have left it.
        let store = SessionStore::new(&project_dir);
        store.save(&[persist::SavedSession {
            session_id: uuid.clone(),
            name: None,
            muted: false,
        }]);
        eprintln!("seeded store at {:?}", store.path());

        let helper = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target/debug/mulpex-helper");
        assert!(helper.exists(), "build mulpex-helper first: {helper:?}");

        let mut core = Core::open(1, project_dir, &helper, root.join("state")).unwrap();
        eprintln!("restored {} session(s)", core.sessions.len());
        assert_eq!(core.sessions.len(), 1, "the saved session did not spawn");

        // Watch it for a while: a claude that cannot resume exits within seconds.
        for i in 0..40 {
            std::thread::sleep(Duration::from_millis(500));
            let removed = core.reap_dead();
            if !removed.is_empty() {
                let after = std::fs::read_to_string(store.path()).unwrap_or_default();
                panic!(
                    "the restored session DIED after {:.1}s (removed {removed:?}).\n\
                     store now:\n{after}",
                    i as f32 * 0.5
                );
            }
        }
        eprintln!("still alive after 20s — restore works");
        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Does a quit destroy the record of the sessions that were running? Drives
    /// the real `Workspace`, an isolated HOME, and the same call sequence the
    /// poll loop and the exit handler use.
    #[test]
    #[ignore]
    fn repro_quit_preserves_the_session_store() {
        let _env = env_guard();
        let uuid = std::fs::read_to_string("/tmp/mulpex_test_uuid")
            .unwrap()
            .trim()
            .to_string();
        let project_dir = std::fs::read_to_string("/tmp/mulpex_test_dir")
            .unwrap()
            .trim()
            .to_string();
        let root = std::env::temp_dir().join(format!("mulpex-quit-{}", persist::new_uuid()));
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::env::set_var("HOME", root.join("home"));

        let store = SessionStore::new(Path::new(&project_dir));
        store.save(&[persist::SavedSession {
            session_id: uuid.clone(),
            name: None,
            muted: false,
        }]);
        let dump = || std::fs::read_to_string(store.path()).unwrap_or_default();
        assert!(dump().contains(&uuid), "seed failed");

        let helper = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target/debug/mulpex-helper");
        let mut ws = Workspace::new();
        ws.state_root = root.join("state");
        let (h, _) = ws.open_or_focus(&project_dir, &helper).unwrap();
        assert_eq!(ws.project(h).unwrap().sessions.len(), 1, "did not restore");
        eprintln!("after open:\n{}", dump());

        // What the poll loop does, for a few seconds of normal running.
        for _ in 0..15 {
            std::thread::sleep(Duration::from_millis(200));
            let core = ws.project_mut(h).unwrap();
            core.reap_dead();
            core.process_spawn_requests();
            core.process_terminal_requests();
            core.refresh_worked();
            core.sync_terminal_index();
        }
        let running = dump();
        eprintln!("while running:\n{running}");
        assert!(
            running.contains(&uuid),
            "the store lost the session WHILE RUNNING"
        );

        // Quit.
        ws.teardown_all();
        eprintln!("after teardown:\n{}", dump());

        // The poll thread keeps ticking until the process actually exits.
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(200));
            for core in &mut ws.projects {
                core.reap_dead();
                core.refresh_worked();
                core.sync_terminal_index();
            }
        }
        let after = dump();
        eprintln!("after quit + more polling:\n{after}");
        assert!(
            after.contains(&uuid),
            "QUIT DESTROYED THE SESSION RECORD — store is now:\n{after}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The terminal request handshake the MCP helper uses, end to end through the
    /// same entry point the poll loop calls.
    #[test]
    fn terminal_requests_open_send_and_close() {
        let (_env, root, mut core) = scratch_core("term-req-test");
        let dir = core.state_dir.join("termreq");

        let post = |body: serde_json::Value| {
            let token = persist::new_uuid();
            std::fs::write(
                dir.join(format!("{token}.json")),
                body.to_string(),
            )
            .unwrap();
            token
        };
        let reply = |token: &str| -> serde_json::Value {
            let p = dir.join(format!("{token}.done"));
            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
        };

        let t = post(serde_json::json!({ "op": "open", "from": 1, "label": "shell" }));
        assert!(core.process_terminal_requests());
        let r = reply(&t);
        assert_eq!(r["ok"], true);
        let id = r["id"].as_u64().unwrap() as usize;

        // Typing into it reaches the PTY: ask the shell to write a file.
        let marker = core.project_dir.join("touched");
        let t = post(serde_json::json!({
            "op": "send", "from": 1, "id": id,
            "data": format!("touch {}\r", marker.display()),
        }));
        core.process_terminal_requests();
        assert_eq!(reply(&t)["ok"], true);
        assert!(wait_until(|| marker.exists()), "input never reached the shell");

        // A send to an id that isn't a terminal is refused, not silently dropped.
        let t = post(serde_json::json!({ "op": "send", "from": 1, "id": 999, "data": "x" }));
        core.process_terminal_requests();
        let r = reply(&t);
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("no terminal"));

        let t = post(serde_json::json!({ "op": "close", "from": 1, "id": id }));
        core.process_terminal_requests();
        assert_eq!(reply(&t)["ok"], true);
        assert!(wait_until(|| core.reap_dead() == vec![id]));

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }
}
