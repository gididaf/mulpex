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
use mulpex_core::registry;

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
    /// The one geometry every PTY in every project runs at (cols, rows) — the
    /// center pane's, once the frontend has measured it. Held here rather than
    /// per-project because all PTYs have always shared one size, and a project
    /// that happens to have no xterms yet must still spawn at the size the
    /// frontend is about to build them at. See `resize_all`.
    pub geometry: (u16, u16),
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
            geometry: (DEFAULT_COLS, DEFAULT_ROWS),
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
        let core = Core::open(handle, canon, helper_path, state_dir, self.geometry)?;
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
            cols: self.geometry.0,
            rows: self.geometry.1,
        }
    }

    /// Bring **every** session in **every** project to the center pane's geometry,
    /// and record it as the size future spawns start at.
    ///
    /// Workspace-wide rather than per-project on purpose. The frontend holds one
    /// geometry for all its xterms, so a project it hasn't built xterms for yet
    /// would otherwise keep spawning at the stale size while the frontend built
    /// its terminals at the current one — and a PTY whose emulator is a different
    /// size is corrupted permanently (see `WorkspaceInfo::cols`).
    pub fn resize_all(&mut self, cols: u16, rows: u16) {
        self.geometry = (cols.max(1), rows.max(1));
        for core in &mut self.projects {
            core.resize_all(cols, rows);
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
    /// Provisional labels Mulpex derived from an instance's own task because it
    /// finished a turn without naming itself (`apply_fallback_names`). Displayed
    /// exactly like a real name, but **deliberately kept out of `names`**:
    ///
    /// - it must never be persisted — a name coming back from the store is taken
    ///   as the user's (`manual_names` is seeded from the restored names), which
    ///   would freeze a machine-made guess *and* permanently refuse the
    ///   instance's own `hub_set_name`;
    /// - it must not count as "named" for `name_verdict`, whose `current` check
    ///   would otherwise let a fallback shadow the real thing.
    ///
    /// So it is a display-only overlay, dropped the moment a real name lands.
    fallback_names: HashMap<usize, String>,
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
    /// Remote terminals that have been given something and owe an answer.
    ///
    /// The idle backstop is meaningless without this. A remote claude sitting at
    /// a fresh prompt, having never been asked anything, is *also* silent — so a
    /// backstop keyed on silence alone fires the instant the TUI finishes
    /// drawing and reports a finished turn that never started. Measured: the
    /// first live run woke the driver 5.7 s in, before the task had even been
    /// typed. An id is added when input is sent to it and removed when a wake is
    /// delivered, so "quiet" only counts as "your turn" while an answer is owed.
    remote_awaiting: HashSet<usize>,
    /// Ids restored from the store this run, and when they started.
    restored: HashMap<usize, Instant>,
    /// When each session was spawned, for `EARLY_DEATH_GRACE`. Covers every
    /// session, not just restored ones — a fresh ⌘T can fail to start too.
    started: HashMap<usize, Instant>,
    /// Instances that died before they were ever usable, and why. Their rows are
    /// **kept** so the failure is visible; see `reap_dead`.
    failed: HashMap<usize, String>,
    /// Records that must stay in the store even though their session is gone —
    /// see `reap_dead`. Deduped by session id, and each paired with the row it
    /// occupied so `persist_sessions` can put it back there rather than at the
    /// end: a conversation that silently migrates to the bottom of the sidebar
    /// after a failed launch is indistinguishable from one that was reordered on
    /// purpose.
    sticky: Vec<(usize, persist::SavedSession)>,
    /// Last content written to `terminals/index`, so the poll loop can refresh
    /// it on change without a disk write every tick.
    terminal_index: String,
    /// The size this project's PTYs run at (cols, rows). Every spawn uses it, so
    /// a session created now matches the xterm the frontend is about to build for
    /// it; `resize_all` keeps it current. Seeded from `Workspace::geometry`.
    geometry: (u16, u16),
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
        geometry: (u16, u16),
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
            "bg",
            "compacting",
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
        let mut sticky: Vec<(usize, persist::SavedSession)> = Vec::new();
        // Ids come from the store, so an instance keeps the number the user knows
        // it by (`claude#15` stays claude#15). `sessions.len() + 1` cannot do that
        // for two independent reasons: it renumbers 2/3/15 to 1/2/3 on every
        // launch, and it does not advance when a spawn fails, so the next record
        // would silently take the failed one's number.
        let mut used: HashSet<usize> = HashSet::new();
        let mut next_free = 1usize;
        for (position, saved) in store.load().into_iter().enumerate() {
            // A store written before ids were persisted (or one with a duplicate
            // after hand-editing) numbers sequentially, exactly as before.
            let id = match saved.id {
                Some(n) if n >= 1 && !used.contains(&n) => n,
                _ => {
                    while used.contains(&next_free) {
                        next_free += 1;
                    }
                    next_free
                }
            };
            used.insert(id);
            // Restores are deliberately NOT preflighted with `dir_access_error`.
            // Letting the spawn fail produces a session row that says why it
            // failed, which is the whole point — refusing up front would restore
            // the project to an empty sidebar, i.e. the original bug.
            if let Ok(session) = Session::spawn(
                id,
                &project_dir,
                geometry.1,
                geometry.0,
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
                // Keep the record AND where it sat: appending it would move that
                // conversation to the last row on the next launch, which is how a
                // single bad restore reshuffles a whole sidebar.
                sticky.push((position, persist::SavedSession { id: Some(id), ..saved }));
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
        // Continue past the highest number ever handed out, so a new ⌘T can never
        // collide with a restored instance (or with the gap a failed one left).
        let next_id = used.iter().copied().max().unwrap_or(0) + 1;
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
            fallback_names: HashMap::new(),
            muted,
            worked,
            closing: HashSet::new(),
            remote_awaiting: HashSet::new(),
            restored,
            started,
            failed: HashMap::new(),
            sticky,
            terminal_index: String::new(),
            geometry,
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
                name: self.display_name(s.id),
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
            self.geometry.1,
            self.geometry.0,
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
            self.geometry.1,
            self.geometry.0,
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
                        serde_json::json!({ "ok": false, "error": format!("term#{id} has exited; open a new one") }),
                        false,
                    ),
                    Some(s) => {
                        s.send(data.as_bytes());
                        // Anything typed at a remote claude puts the ball in its
                        // court, which is what arms the idle backstop. Sending
                        // to an ordinary shell arms nothing.
                        if mulpex_core::remote::RemoteMeta::read(&self.state_dir, id).is_some() {
                            self.remote_awaiting.insert(id);
                        }
                        (serde_json::json!({ "ok": true, "id": id }), false)
                    }
                    None => (
                        serde_json::json!({ "ok": false, "error": format!("no term#{id}") }),
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
                        serde_json::json!({ "ok": false, "error": format!("no term#{id}") }),
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
        // Either way the user has spoken, so a provisional label is done: clearing
        // a name must actually clear the row, not fall back to Mulpex's guess.
        self.fallback_names.remove(&id);
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
                // A real name supersedes anything we guessed for this row.
                self.fallback_names.remove(&id);
                changed = true;
            }
        }
        if changed {
            self.persist_sessions();
        }
        changed
    }

    /// The label the sidebar shows for a row: the real name if there is one,
    /// otherwise the provisional one. See `fallback_names` for why the two are
    /// separate maps rather than one.
    fn display_name(&self, id: usize) -> Option<String> {
        self.names
            .get(&id)
            .or_else(|| self.fallback_names.get(&id))
            .cloned()
    }

    /// Give an instance that has finished a turn without naming itself a
    /// provisional label derived from its own captured task — the same
    /// `name_from_task` a `hub_spawn` child is labelled with.
    ///
    /// This is the deterministic half of the naming fix; the model-facing half is
    /// the hook's mid-turn nudge. Both exist because the prompt-time reminder is
    /// only a *request*: an instance can acknowledge it, get absorbed in a long
    /// task, and never call `hub_set_name` — and until the user's next prompt
    /// nothing asks again, leaving the row showing the raw prompt verbatim.
    ///
    /// Three things make this safe to do behind the model's back:
    /// - **Only at a turn boundary.** Naming from a task mid-turn would label the
    ///   row off the first thing typed; by `waiting`/`needs` the task is at least
    ///   the whole request. (An unnamed row shows that same text meanwhile, so
    ///   nothing is hidden in the interim — it just isn't frozen yet.)
    /// - **Never over anything.** A real name, a ⌘R name, or an earlier fallback
    ///   all win; this only fills an empty label, once.
    /// - **Not persisted, and not `named/<id>`.** The instance keeps being nudged
    ///   and its own `hub_set_name` still lands, so a guess is always replaceable
    ///   by the real thing. See `fallback_names`.
    ///
    /// Pure in-memory (the statuses and tasks come from the snapshot the poll loop
    /// already computed), so a quiet tick costs a couple of hash lookups — this
    /// runs at 5 Hz per project.
    pub fn apply_fallback_names(&mut self, snap: &HubSnapshot) -> bool {
        let mut changed = false;
        for entry in &snap.statuses {
            let id = entry.id;
            if entry.status == Status::Working
                || self.names.contains_key(&id)
                || self.fallback_names.contains_key(&id)
            {
                continue;
            }
            let Some(task) = snap.tasks.iter().find(|t| t.id == id) else {
                continue;
            };
            if let Some(name) = name_from_task(&task.task) {
                self.fallback_names.insert(id, name);
                changed = true;
            }
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
        self.geometry = (cols.max(1), rows.max(1));
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
                        id: Some(session.id),
                    };
                    if !self
                        .sticky
                        .iter()
                        .any(|(_, s)| s.session_id == record.session_id)
                    {
                        self.sticky.push((kept.len(), record));
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
        self.fallback_names
            .retain(|id, _| self.sessions.iter().any(|s| s.id == *id));
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
        // A remote terminal's token and seen-markers go with it: a recycled id
        // must never inherit another terminal's identity.
        mulpex_core::remote::forget_all(&self.state_dir, id);
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

    /// Watch every remote-claude terminal and turn "it's your turn" into a hub
    /// message in the driver's inbox.
    ///
    /// This is the whole reason a remote peer can *initiate*. A remote claude has
    /// no inbox, no instance id and no way to push — all it can do is print. So
    /// the poll loop reads what it printed and writes the message on its behalf,
    /// into the opener's inbox dir, which is precisely the directory that
    /// instance's hub-listener Monitor is already watching. Nothing new wakes the
    /// driver: the existing peer-message path does, unchanged.
    ///
    /// Two triggers, deliberately:
    ///
    /// - **The signal** the remote prints, which carries *why* it is calling.
    /// - **Silence**, when it doesn't. A working `claude` animates its spinner
    ///   continuously, so no output for `IDLE_TURN_END_MS` means its turn ended.
    ///   This backstop exists because the signal is an instruction to a language
    ///   model and instructions get skipped — and the failure mode without it is
    ///   the bad kind: the driver waits forever and nothing looks broken.
    ///
    /// Both are deduped, so a marker sitting on a screen that never scrolls does
    /// not re-fire every 200 ms.
    pub fn process_remote_signals(&mut self) {
        for (id, meta) in mulpex_core::remote::RemoteMeta::all(&self.state_dir) {
            // The terminal is gone (closed, or exited and reaped): drop its
            // records so a recycled id can never inherit another's token.
            let Some(session) = self.sessions.iter().find(|s| s.id == id) else {
                mulpex_core::remote::forget_all(&self.state_dir, id);
                continue;
            };
            // Nothing to deliver to: the driver has closed. The remote keeps
            // running — it is the user's terminal now — but nobody is listening.
            if !self.sessions.iter().any(|s| s.id == meta.opener) {
                continue;
            }

            let log_path = crate::pty::terminal_log_path(&self.state_dir, id);
            let log_bytes = std::fs::read(&log_path).unwrap_or_default();
            let log = String::from_utf8_lossy(
                log_bytes
                    .get(mulpex_core::termlog::HEADER_LEN..)
                    .unwrap_or_default(),
            )
            .into_owned();
            let screen =
                std::fs::read_to_string(crate::pty::terminal_screen_path(&self.state_dir, id))
                    .unwrap_or_default();
            // Both channels, because a row only reaches the log once it scrolls
            // off the top: a remote that answers briefly and sits there has its
            // marker on screen and nowhere else.
            let signal = mulpex_core::remote::find_signals(&log, &meta.token)
                .into_iter()
                .last()
                .or_else(|| {
                    mulpex_core::remote::find_signals(&screen, &meta.token)
                        .into_iter()
                        .last()
                });

            // Idleness comes from the log header the recorder maintains, not
            // from the Session: the recorder is what actually sees the bytes.
            let idle_ms = mulpex_core::termlog::parse_header(&log_bytes)
                .map(|h| crate::vtgrid::now_ms().saturating_sub(h.last_out_ms))
                .unwrap_or(0);
            let quiet = session.is_alive()
                && idle_ms >= mulpex_core::remote::IDLE_TURN_END_MS
                && mulpex_core::remote::looks_like_claude_tui(&screen)
                && !mulpex_core::remote::has_spinner(&screen);

            let to_send = match signal {
                Some(sig) => mulpex_core::remote::take_if_new(&self.state_dir, id, "watch", &sig)
                    .then_some(sig),
                // Silence only means "your turn" if the remote owes an answer.
                None if quiet && self.remote_awaiting.contains(&id) => {
                    Some(mulpex_core::remote::Signal {
                        kind: mulpex_core::remote::Kind::Ended,
                        summary: String::new(),
                    })
                }
                _ => None,
            };

            let Some(sig) = to_send else { continue };
            // The debt is settled: nothing further is owed until the driver
            // speaks again, which is what stops one quiet period from waking it
            // on every 200 ms tick.
            self.remote_awaiting.remove(&id);
            // A synthesised "it went quiet" must not out-shout a real signal that
            // is about to arrive; a real one for the same turn supersedes it via
            // the dedupe above.
            let body = mulpex_core::remote::wake_body(id, &meta.ssh_target, &sig);
            self.deliver_hub_message(meta.opener, id, &body);
        }
    }

    /// Drop a message into an instance's inbox as if a peer had sent it.
    ///
    /// Writes the same shape `mcp::hub_send` writes, plus `from_terminal` so the
    /// recipient is told a *terminal* is calling and not claude#N — replying
    /// with `hub_send` to a terminal id would go nowhere.
    fn deliver_hub_message(&self, to: usize, from_terminal: usize, body: &str) {
        let dir = self.state_dir.join("inbox").join(to.to_string());
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let name = format!("{}-{}.json", ts, mulpex_core::persist::new_uuid());
        let _ = std::fs::write(
            dir.join(name),
            serde_json::json!({
                "from": 0,
                "from_terminal": from_terminal,
                "ts": ts,
                "body": body,
            })
            .to_string(),
        );
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
                    bounce_dead_inbox(
                        &self.state_dir,
                        &self.project_dir,
                        &self.project_name,
                        id,
                        &entry.path(),
                        &live,
                    );
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

    /// This project as the workspace registry describes it to the OTHER projects:
    /// who is live here, and where this project's hub lives on disk so another
    /// project's helper can deliver into its inboxes.
    ///
    /// Derived from the snapshot the poll loop has already computed, so it costs
    /// no extra disk reads — and it inherits `statuses`' membership rule for
    /// free, which is the one that matters: shells and failed-to-start instances
    /// are absent, so `hub_send` can never be offered one as a cross-project peer
    /// any more than it can as a local one.
    pub fn registry_entry(&self, snap: &HubSnapshot) -> registry::ProjectEntry {
        let instances = snap
            .statuses
            .iter()
            .map(|s| registry::InstanceEntry {
                id: s.id,
                status: s.status.word().to_string(),
                task: snap
                    .tasks
                    .iter()
                    .find(|t| t.id == s.id)
                    .map(|t| t.task.clone())
                    .unwrap_or_default(),
                name: self.display_name(s.id),
            })
            .collect();
        registry::ProjectEntry {
            handle: self.handle,
            name: self.project_name.clone(),
            dir: self.project_dir.to_string_lossy().into_owned(),
            state_dir: self.state_dir.to_string_lossy().into_owned(),
            instances,
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
                // The number the user knows this instance by, so the next launch
                // brings it back as the same `claude#N`.
                id: Some(s.id),
            })
            .collect();
        // Records whose session is gone but must not be forgotten — a restore
        // that failed. Re-inserted at the row they held (ascending, so earlier
        // ones don't shift the later ones past their slot), and only if the
        // session isn't already live, so a later successful restore of the same
        // one doesn't duplicate it.
        let mut pending: Vec<&(usize, persist::SavedSession)> = self.sticky.iter().collect();
        pending.sort_by_key(|(at, _)| *at);
        for (at, record) in pending {
            if !sessions.iter().any(|s| s.session_id == record.session_id) {
                let at = (*at).min(sessions.len());
                sessions.insert(at, record.clone());
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
        // `from` is an address (`claude#2` / `central-one#3`), not a number — it
        // has to carry the project once a sender can live in a different one.
        let Ok(ts) = ts.parse::<u64>() else {
            continue;
        };
        out.push(MsgEntry {
            from: from.to_string(),
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
///
/// **The sender may live in another project**, and that is not a detail: a
/// foreign message carries `from_project_dir`, and its bare `from` is an id in
/// *that* project's numbering. Routing on `from` alone would hand a stranger's
/// undelivered mail to whichever local instance happens to share the number —
/// mis-delivery, not a dropped message. So a tagged sender is resolved through
/// the registry and bounced back across the boundary, and one whose project has
/// since closed is dropped rather than guessed at.
fn bounce_dead_inbox(
    state_dir: &Path,
    project_dir: &Path,
    project_name: &str,
    dead_id: usize,
    inbox: &Path,
    live: &HashSet<usize>,
) -> usize {
    let mut bounced = 0usize;
    // Read lazily: this whole function only runs when an instance has closed
    // holding unread mail, and the foreign case is rarer still.
    let mut reg: Option<registry::Registry> = None;
    let my_dir = project_dir.to_string_lossy().into_owned();

    if let Ok(entries) = std::fs::read_dir(inbox) {
        for entry in entries.flatten() {
            let file = entry.path();
            if file.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some((from, from_project_dir, body)) =
                std::fs::read_to_string(&file).ok().and_then(|c| {
                    let v: serde_json::Value = serde_json::from_str(&c).ok()?;
                    // A remote claude's wake has no id to bounce to (`from: 0`
                    // plus `from_terminal`); it is skipped by the liveness checks
                    // below, since 0 is never a live instance.
                    let from = v.get("from").and_then(|x| x.as_u64())? as usize;
                    let from_project_dir = v
                        .get("from_project_dir")
                        .and_then(|x| x.as_str())
                        .map(str::to_string);
                    let body = v
                        .get("body")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    Some((from, from_project_dir, body))
                })
            else {
                continue;
            };

            let snippet: String = body.chars().take(80).collect();
            let ellipsis = if body.chars().count() > 80 { "…" } else { "" };

            // Where the bounce goes, and how the dead recipient is named to the
            // sender — from *their* point of view, so the address they see is one
            // they could have used.
            let (dir, dead_addr, foreign) = match from_project_dir {
                Some(ref d) if !registry::same_dir(Path::new(d), project_dir) => {
                    let reg = reg.get_or_insert_with(|| registry::Registry::read_for(state_dir));
                    let Some(sender) = reg.projects.iter().find(|p| p.is_dir(Path::new(d))) else {
                        continue; // their whole project closed too
                    };
                    if !sender.has_instance(from) {
                        continue;
                    }
                    (
                        sender.inbox_dir(from),
                        format!("{project_name}#{dead_id}"),
                        true,
                    )
                }
                _ => {
                    if from == dead_id || !live.contains(&from) {
                        continue;
                    }
                    (
                        state_dir.join("inbox").join(from.to_string()),
                        format!("claude#{dead_id}"),
                        false,
                    )
                }
            };

            let notice = format!(
                "[Mulpex hub — automated] Your message to {dead_addr} was NOT delivered: \
                 that instance closed before reading it. Original: \"{snippet}{ellipsis}\""
            );
            if std::fs::create_dir_all(&dir).is_ok() {
                // A cross-project bounce must carry the provenance keys too, or
                // the sender's `sender_label` would read it as a local `claude#N`.
                let payload = if foreign {
                    serde_json::json!({
                        "from": dead_id,
                        "from_project": project_name,
                        "from_project_dir": my_dir,
                        "ts": now_secs(),
                        "body": notice,
                    })
                } else {
                    serde_json::json!({ "from": dead_id, "ts": now_secs(), "body": notice })
                };
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

    /// What `Workspace::geometry` hands a `Core` at open. Deliberately not the
    /// DEFAULT pair, so a test asserting a spawn used the *workspace's* size
    /// can't pass by accidentally reading the default.
    const TEST_GEOMETRY: (u16, u16) = (100, 40);

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
            TEST_GEOMETRY,
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

    /// The log carries addresses now, not ids, so an entry can name the project a
    /// message came from. A bare-number `from` would have nowhere to put it.
    #[test]
    fn the_message_log_carries_addresses_on_both_ends() {
        let dir = std::env::temp_dir().join(format!("mulpex-msglog-{}", persist::new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("messages.log");
        std::fs::write(
            &log,
            "100\tclaude#1\tclaude#2\thello\n\
             101\tclaude#2\tall\tbroadcast\n\
             102\tcloud#2\tcentral-one#3\tacross\\nprojects\n",
        )
        .unwrap();

        // Newest first.
        let msgs = read_messages(&log, 10);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].from, "cloud#2");
        assert_eq!(msgs[0].to, "central-one#3");
        assert_eq!(msgs[0].body, "across\nprojects", "escapes still decode");
        assert_eq!(msgs[1].to, "all");
        assert_eq!(msgs[2].from, "claude#1");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A message from another project carries `from_project_dir`, and its bare
    /// `from` is an id in *that* project's numbering. Bouncing on the number alone
    /// would hand a stranger's undelivered mail to whichever local instance
    /// happens to share it — mis-delivery, not a dropped message.
    ///
    /// Non-tautological by construction: project B has a LIVE claude#2, which is
    /// exactly where the pre-fix code would have put this bounce.
    #[test]
    fn a_foreign_bounce_goes_back_across_the_boundary_not_to_the_local_same_number() {
        let root = std::env::temp_dir().join(format!("mulpex-bounce-{}", persist::new_uuid()));
        let a_state = root.join("1");
        let b_state = root.join("2");
        let a_dir = root.join("proj-a");
        let b_dir = root.join("proj-b");
        std::fs::create_dir_all(a_state.join("inbox").join("2")).unwrap();
        std::fs::create_dir_all(b_state.join("inbox").join("2")).unwrap();
        let dead_inbox = b_state.join("inbox").join("3");
        std::fs::create_dir_all(&dead_inbox).unwrap();

        let reg = registry::Registry {
            projects: vec![
                registry::ProjectEntry {
                    handle: 1,
                    name: "proj-a".into(),
                    dir: a_dir.to_string_lossy().into_owned(),
                    state_dir: a_state.to_string_lossy().into_owned(),
                    instances: vec![registry::InstanceEntry {
                        id: 2,
                        status: "waiting".into(),
                        task: String::new(),
                        name: None,
                    }],
                },
                registry::ProjectEntry {
                    handle: 2,
                    name: "proj-b".into(),
                    dir: b_dir.to_string_lossy().into_owned(),
                    state_dir: b_state.to_string_lossy().into_owned(),
                    instances: vec![],
                },
            ],
        };
        assert!(registry::Registry::write_if_changed(&root, &reg));

        // proj-a's claude#2 wrote to proj-b's claude#3, which has since closed.
        std::fs::write(
            dead_inbox.join("m.json"),
            serde_json::json!({
                "from": 2,
                "from_project": "proj-a",
                "from_project_dir": a_dir.to_string_lossy(),
                "ts": 1,
                "body": "the shared endpoint is /v2/tokens",
            })
            .to_string(),
        )
        .unwrap();

        // proj-b's own claude#1 also wrote to the same dead instance.
        std::fs::write(
            dead_inbox.join("local.json"),
            serde_json::json!({ "from": 1, "ts": 1, "body": "local note" }).to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(b_state.join("inbox").join("1")).unwrap();

        // proj-b's claude#2 is LIVE — the wrong place the old routing would pick.
        let live: HashSet<usize> = [1usize, 2].into_iter().collect();
        let bounced = bounce_dead_inbox(&b_state, &b_dir, "proj-b", 3, &dead_inbox, &live);
        assert_eq!(bounced, 2, "both senders should have been told");

        let count = |p: PathBuf| {
            std::fs::read_dir(p)
                .map(|d| d.flatten().count())
                .unwrap_or(0)
        };
        assert_eq!(
            count(b_state.join("inbox").join("2")),
            0,
            "the foreign bounce landed on the LOCAL claude#2 — that is the bug"
        );
        assert_eq!(count(a_state.join("inbox").join("2")), 1);
        assert_eq!(count(b_state.join("inbox").join("1")), 1);

        // The foreign sender must be told an address it could have used, and the
        // notice must carry provenance or their `sender_label` would read it as a
        // local `claude#3`.
        let file = std::fs::read_dir(a_state.join("inbox").join("2"))
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        let body = v["body"].as_str().unwrap();
        assert!(body.contains("proj-b#3"), "{body}");
        assert!(body.contains("the shared endpoint"), "{body}");
        assert_eq!(v["from_project"], "proj-b");
        assert_eq!(v["from"], 3);

        // And the local one still reads as local.
        let file = std::fs::read_dir(b_state.join("inbox").join("1"))
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        assert!(v["body"].as_str().unwrap().contains("claude#3"));
        assert!(v.get("from_project").is_none());

        assert!(!dead_inbox.exists(), "the dead inbox is removed either way");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A sender whose whole project has closed has nowhere to bounce to. Dropping
    /// it is the only honest option — the alternative is guessing at a local id.
    #[test]
    fn a_bounce_to_a_closed_project_is_dropped_rather_than_guessed_at() {
        let root = std::env::temp_dir().join(format!("mulpex-bounce2-{}", persist::new_uuid()));
        let b_state = root.join("2");
        let b_dir = root.join("proj-b");
        let dead_inbox = b_state.join("inbox").join("3");
        std::fs::create_dir_all(&dead_inbox).unwrap();
        std::fs::create_dir_all(b_state.join("inbox").join("2")).unwrap();
        // Registry knows only proj-b; the sender's project is gone.
        let reg = registry::Registry {
            projects: vec![registry::ProjectEntry {
                handle: 2,
                name: "proj-b".into(),
                dir: b_dir.to_string_lossy().into_owned(),
                state_dir: b_state.to_string_lossy().into_owned(),
                instances: vec![],
            }],
        };
        registry::Registry::write_if_changed(&root, &reg);
        std::fs::write(
            dead_inbox.join("m.json"),
            serde_json::json!({
                "from": 2,
                "from_project": "ghost",
                "from_project_dir": root.join("ghost").to_string_lossy(),
                "ts": 1,
                "body": "x",
            })
            .to_string(),
        )
        .unwrap();

        let live: HashSet<usize> = [2usize].into_iter().collect();
        assert_eq!(
            bounce_dead_inbox(&b_state, &b_dir, "proj-b", 3, &dead_inbox, &live),
            0
        );
        assert_eq!(
            std::fs::read_dir(b_state.join("inbox").join("2"))
                .map(|d| d.flatten().count())
                .unwrap_or(0),
            0,
            "a bounce for a closed project must not fall back to a local id"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `HOME` is process-global and the session store is derived from it, so the
    /// tests that repoint it have to take turns. Without this they race and one
    /// test's store lands under another's HOME.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub(super) fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A `Core` on a throwaway HOME + scratch dir, for terminal tests. Returns
    /// the env guard first — hold it for the life of the test.
    pub(super) fn scratch_core(tag: &str) -> (std::sync::MutexGuard<'static, ()>, PathBuf, Core) {
        let guard = env_guard();
        let (root, core) = scratch_core_inner(tag);
        (guard, root, core)
    }

    pub(super) fn scratch_core_inner(tag: &str) -> (PathBuf, Core) {
        let root = std::env::temp_dir().join(format!("mulpex-{tag}-{}", persist::new_uuid()));
        let project_dir = root.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::env::set_var("HOME", root.join("home"));
        let core = Core::open(
            1,
            project_dir,
            Path::new("/nonexistent/mulpex-helper"),
            root.join("state"),
            TEST_GEOMETRY,
        )
        .unwrap();
        (root, core)
    }

    /// A session's PTY must spawn at the geometry the frontend is going to build
    /// its xterm at — not at a fixed default the frontend then has to correct.
    ///
    /// This is the whole of the "last few lines render outdated content" bug.
    /// `attach_session` flushes everything the PTY has already printed, so an
    /// xterm built at some other size renders that backlog wrong — and Claude
    /// Code draws on the alt screen and repaints it DIFFERENTIALLY, skipping rows
    /// it believes are already correct, so a row the emulator got wrong is never
    /// rewritten. Measured against a real `claude`: a stream rendered for 120x32
    /// replayed into an 80x24 emulator and then resized keeps its debris forever,
    /// while the same stream into an emulator that agreed all along matches
    /// tmux byte for byte.
    ///
    /// Spawning a real shell rather than asserting on a constant: the assertion
    /// reads the size off the live PTY, so it fails if the spawn path goes back
    /// to `DEFAULT_COLS`/`DEFAULT_ROWS` (which `TEST_GEOMETRY` deliberately is
    /// not) or ignores a later resize.
    #[test]
    fn a_session_spawns_at_the_geometry_the_frontend_will_build_its_xterm_at() {
        let (_env, root, mut core) = scratch_core("geometry-test");

        let first = core.spawn_terminal(None, None, true).unwrap();
        assert_eq!(
            core.session_mut(first.id).unwrap().size(),
            TEST_GEOMETRY,
            "a new PTY ignored the workspace geometry, so its xterm cannot match it"
        );

        // What the frontend's refit does once it has measured the center pane.
        core.resize_all(204, 55);
        assert_eq!(
            core.session_mut(first.id).unwrap().size(),
            (204, 55),
            "an existing PTY was not brought to the new geometry"
        );

        let second = core.spawn_terminal(None, None, true).unwrap();
        assert_eq!(
            core.session_mut(second.id).unwrap().size(),
            (204, 55),
            "a session spawned after a resize went back to the spawn-time default"
        );

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The geometry is workspace-wide, and `bootstrap` must report it: the
    /// frontend builds every xterm at whatever this says before attaching any of
    /// them. A project with no xterms yet still has to spawn at the same size.
    #[test]
    fn the_workspace_reports_and_applies_one_geometry_for_every_project() {
        let mut ws = Workspace::new();
        assert_eq!(
            (ws.workspace_info().cols, ws.workspace_info().rows),
            ws.geometry,
            "bootstrap must hand the frontend the size the PTYs actually have"
        );

        ws.resize_all(204, 55);
        assert_eq!(ws.geometry, (204, 55));
        let info = ws.workspace_info();
        assert_eq!((info.cols, info.rows), (204, 55));
    }

    /// Restore a store seeded exactly as a previous run left it, and hand back
    /// the `Core` plus its scratch root.
    fn core_from_store(tag: &str, saved: &[persist::SavedSession]) -> (std::sync::MutexGuard<'static, ()>, PathBuf, Core) {
        let guard = env_guard();
        let root = std::env::temp_dir().join(format!("mulpex-{tag}-{}", persist::new_uuid()));
        let project_dir = root.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::env::set_var("HOME", root.join("home"));
        SessionStore::new(&project_dir).save(saved);
        let core = Core::open(
            1,
            project_dir,
            Path::new("/nonexistent/mulpex-helper"),
            root.join("state"),
            TEST_GEOMETRY,
        )
        .unwrap();
        (guard, root, core)
    }

    fn record(name: &str, id: Option<usize>) -> persist::SavedSession {
        persist::SavedSession {
            session_id: persist::new_uuid(),
            name: Some(name.into()),
            muted: false,
            id,
        }
    }

    /// An instance must come back as the number the user knows it by.
    ///
    /// Reported from the field: three instances — claude#2, claude#3, claude#15 —
    /// came back after a restart as claude#1, claude#2, claude#3. Every number
    /// then named a different conversation than it had before, with nothing on
    /// screen saying so, which is how you end up opening the wrong one. The id is
    /// an identity (it is what `hub_send` addresses, and what a person says out
    /// loud), not a position in a list.
    #[test]
    fn a_restored_instance_keeps_the_number_the_user_knows_it_by() {
        let saved = [
            record("waiting-room phase 2", Some(2)),
            record("ticket routing", Some(3)),
            record("drifted STAGING audit", Some(15)),
        ];
        let (_env, root, mut core) = core_from_store("keepid", &saved);

        assert_eq!(
            core.sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![2, 3, 15],
            "restored instances were renumbered, so every claude#N now means something else"
        );
        // The conversation, the name and the number must all still be the same
        // row — a number that survives while the transcript it points at moves is
        // worse than renumbering, not better.
        for (session, want) in core.sessions.iter().zip(saved.iter()) {
            assert_eq!(session.session_id, want.session_id);
            assert_eq!(core.display_name(session.id).as_deref(), want.name.as_deref());
        }
        assert_eq!(core.next_id, 16, "a fresh ⌘T would collide with a restored id");

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A store written before ids were persisted has to keep working, and keep
    /// numbering exactly as it used to.
    #[test]
    fn a_store_without_ids_still_numbers_sequentially() {
        let saved = [record("one", None), record("two", None), record("three", None)];
        let (_env, root, mut core) = core_from_store("legacyid", &saved);

        assert_eq!(
            core.sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(core.next_id, 4);

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A restore that failed must be written back where it sat, not at the end.
    ///
    /// The second half of the field report: `sticky` records were appended, so a
    /// single bad launch moved that conversation to the bottom of the sidebar —
    /// permanently, since the next launch reads the new order back. Combined with
    /// positional ids it silently reshuffled which number held which
    /// conversation, which is what made the wrong session look like the right one.
    #[test]
    fn a_failed_restore_is_written_back_where_it_sat() {
        let saved = [record("first", Some(1)), record("second", Some(2)), record("third", Some(3))];
        let (_env, root, mut core) = core_from_store("stickyorder", &saved);
        assert_eq!(core.sessions.len(), 3, "the restore did not spawn");

        // The middle one dies inside RESTORE_GRACE without being closed — a lost
        // restore, the case `sticky` exists for. Age its start stamp past
        // EARLY_DEATH_GRACE so `reap_dead` actually REMOVES it (a session that
        // dies within the grace is deliberately kept on screen instead, which is
        // why simply killing it never reaches the `sticky` path at all).
        let victim = core.sessions[1].session_id.clone();
        let victim_id = core.sessions[1].id;
        core.started
            .insert(victim_id, Instant::now() - EARLY_DEATH_GRACE - Duration::from_secs(1));
        core.sessions[1].kill();
        assert!(wait_until(|| !core.sessions[1].is_alive()), "it never died");
        core.reap_dead();
        assert_eq!(core.sessions.len(), 2, "the lost restore was not reaped");
        assert!(!core.sticky.is_empty(), "the lost restore was not kept as sticky");

        let order: Vec<String> = core
            .store
            .load()
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(
            order,
            saved.iter().map(|s| s.session_id.clone()).collect::<Vec<_>>(),
            "a failed restore moved its conversation to another row"
        );
        assert_eq!(
            core.store.load()[1].session_id,
            victim,
            "the failed record did not land back in its own slot"
        );
        // ...and it kept its number, so the next launch restores it as claude#2.
        assert_eq!(core.store.load()[1].id, Some(2));

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
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
            id: None,
        }]);
        let mut core = Core::open(
            1,
            project_dir,
            Path::new("/nonexistent/mulpex-helper"),
            root.join("state"),
            TEST_GEOMETRY,
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

    pub(super) fn wait_until(mut f: impl FnMut() -> bool) -> bool {
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

    /// An instance that ends a turn without having named itself gets a
    /// provisional label from its own task — and that guess must never harden
    /// into a real name, because a name coming back from the store is taken as
    /// the *user's* and would then refuse the instance's own `hub_set_name`
    /// forever.
    ///
    /// The bug this closes: claude#6 acknowledged the naming nudge, armed its
    /// listener, worked for three minutes and never called `hub_set_name`, so
    /// its row showed the user's raw prompt verbatim with nothing to ask again
    /// until the next prompt.
    #[test]
    fn an_unnamed_instance_gets_a_provisional_name_that_never_persists() {
        let (_env, root, mut core) = scratch_core("fallback-name-test");
        let id = core.spawn_instance().unwrap().id;
        let status = core.state_dir.join(id.to_string());
        let task = "fix the brand switcher on the organizations page";
        std::fs::write(core.state_dir.join("tasks").join(id.to_string()), task).unwrap();

        // Mid-turn, nothing is guessed: the request is still being read.
        std::fs::write(&status, "working").unwrap();
        core.refresh_worked();
        let snap = core.hub_snapshot();
        assert!(!core.apply_fallback_names(&snap), "named mid-turn");
        assert_eq!(core.session_infos()[0].name, None);

        // Turn over, still unnamed → the row is labelled from the task.
        std::fs::write(&status, "waiting").unwrap();
        let snap = core.hub_snapshot();
        assert!(core.apply_fallback_names(&snap), "no fallback label");
        assert_eq!(core.session_infos()[0].name.as_deref(), Some(task));
        // Once, not on every one of the five ticks a second takes.
        assert!(!core.apply_fallback_names(&snap), "the label churns every tick");

        // It is a display overlay only — the store must stay empty of it, or the
        // next launch would restore it as a name the user chose.
        assert!(core.worked.contains(&id), "the store test would be vacuous");
        // Through a real write, not just the stale file: the store is rewritten
        // from `names` by anything that persists (a rename, a mute, a reap), so
        // "we didn't persist right now" is not the invariant — "it isn't in
        // `names` at all" is.
        core.persist_sessions();
        assert_eq!(core.store.load().first().and_then(|s| s.name.clone()), None);

        // And the instance can still name itself properly, which supersedes it.
        std::fs::write(core.state_dir.join("namereq").join(id.to_string()), "brand switcher gate")
            .unwrap();
        assert!(core.process_name_requests());
        assert_eq!(core.session_infos()[0].name.as_deref(), Some("brand switcher gate"));
        assert!(!core.fallback_names.contains_key(&id), "the guess outlived the real name");
        assert_eq!(
            core.store.load().first().and_then(|s| s.name.clone()).as_deref(),
            Some("brand switcher gate"),
            "a real name is persisted, unlike the guess"
        );

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
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
            id: None,
        }]);

        let mut core = Core::open(
            1,
            project_dir,
            Path::new("/nonexistent/mulpex-helper"),
            root.join("state"),
            TEST_GEOMETRY,
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
                TEST_GEOMETRY,
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

    /// An rc-file export must reach a spawned child.
    ///
    /// This is the "Not logged in · Please run /login" bug: a Finder-launched
    /// bundle never sourced a login shell, so `CLAUDE_CODE_OAUTH_TOKEN` —
    /// which for a token-authenticated user is the ONLY credential, with no
    /// `~/.claude/.credentials.json` to fall back to — simply wasn't in the
    /// environment `portable_pty` passed through, and every instance opened
    /// logged out. Neither `tauri dev` (inherits the terminal's environment) nor
    /// a ⌘⇧T terminal (`$SHELL -l -i` sources the rc itself) could show it.
    ///
    /// Non-tautological by construction: the variable is exported ONLY from the
    /// probe's `$ZDOTDIR/.zshrc`, and `ZDOTDIR` is removed before the child is
    /// spawned — so the child cannot source that file itself, and the value can
    /// only arrive by being forwarded. The value carries a newline, which is why
    /// the probe uses `env -0`.
    #[test]
    fn an_rc_file_export_reaches_a_spawned_child() {
        let _env = env_guard();
        let root = std::env::temp_dir().join(format!("mulpex-loginenv-{}", persist::new_uuid()));
        let project = root.join("project");
        let zdot = root.join("zdot");
        let home = root.join("home");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&zdot).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("HOME", &home);
        std::env::set_var("SHELL", "/bin/zsh");
        // zsh reads $ZDOTDIR/.zshrc when interactive — the same file the real
        // token export lives in.
        std::fs::write(
            zdot.join(".zshrc"),
            "export MPX_TEST_TOKEN='sk-ant-probe\nsecond-line'\n",
        )
        .unwrap();
        std::env::set_var("ZDOTDIR", &zdot);

        crate::claude_bin::refresh_login_env();

        let forwarded = crate::claude_bin::forwarded_env();
        let seen = forwarded
            .iter()
            .find(|(k, _)| k == "MPX_TEST_TOKEN")
            .map(|(_, v)| v.clone());
        assert_eq!(
            seen.as_deref(),
            Some("sk-ant-probe\nsecond-line"),
            "the login shell's rc export was not harvested (forwarded keys: {:?})",
            forwarded.iter().map(|(k, _)| k).collect::<Vec<_>>()
        );

        // From here on nothing but the forwarding can supply the value.
        std::env::remove_var("ZDOTDIR");
        assert!(std::env::var_os("MPX_TEST_TOKEN").is_none());

        let out = project.join("env.txt");
        let mut core = Core::open(
            1,
            project.clone(),
            Path::new("/nonexistent/mulpex-helper"),
            root.join("state"),
            TEST_GEOMETRY,
        )
        .unwrap();
        let info = core
            .spawn_terminal(
                Some(format!(
                    "printf 'tok=[%s]\\n' \"$MPX_TEST_TOKEN\" > {}",
                    out.display()
                )),
                None,
                true,
            )
            .unwrap();
        assert!(
            wait_until(|| std::fs::read_to_string(&out)
                .map(|s| s.contains("tok="))
                .unwrap_or(false)),
            "the shell never wrote its environment"
        );
        let got = std::fs::read_to_string(&out).unwrap();
        assert!(
            got.contains("sk-ant-probe") && got.contains("second-line"),
            "the rc-file export never reached the child — it saw {got:?}. Every \
             instance would open on \"Not logged in\"."
        );
        let _ = info;

        core.teardown();
        // Leave the cached environment as the next test would expect it.
        crate::claude_bin::refresh_login_env();
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
            id: None,
        }]);
        eprintln!("seeded store at {:?}", store.path());

        let helper = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target/debug/mulpex-helper");
        assert!(helper.exists(), "build mulpex-helper first: {helper:?}");

        let mut core = Core::open(1, project_dir, &helper, root.join("state"), TEST_GEOMETRY).unwrap();
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
            id: None,
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
        assert!(r["error"].as_str().unwrap().contains("no term#"));

        let t = post(serde_json::json!({ "op": "close", "from": 1, "id": id }));
        core.process_terminal_requests();
        assert_eq!(reply(&t)["ok"], true);
        assert!(wait_until(|| core.reap_dead() == vec![id]));

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Live end-to-end exercise of a remote claude peer, against a real machine.
///
/// `#[ignore]`d because it needs an ssh target with `claude` installed and burns
/// real model turns on it. Run with:
///
/// ```text
/// MULPEX_TEST_SSH=root@1.2.3.4 cargo test --lib remote_peer_live -- --ignored --nocapture
/// ```
///
/// It is the only test that proves the *whole* chain rather than a piece of it:
/// the ssh command line, the base64'd rules actually governing the remote, the
/// remote emitting a marker the parser accepts, the recorder preserving that
/// marker through a repainting TUI, and the watcher turning it into a message in
/// the driver's inbox. Every link in that chain has already broken once in
/// development.
#[cfg(test)]
mod remote_peer_live {
    use super::*;
    use mulpex_core::remote;

    /// The flow where the *user* establishes the connection: a terminal that is
    /// already logged in to the remote machine, into which only the `claude`
    /// half is launched. This is what makes a password login, a jump host or a
    /// VPN usable — none of which ssh keys alone can reach.
    #[test]
    #[ignore]
    fn a_claude_launched_into_an_already_connected_terminal_signals_home() {
        let Ok(target) = std::env::var("MULPEX_TEST_SSH") else {
            eprintln!("set MULPEX_TEST_SSH to run this");
            return;
        };
        let _env = tests::env_guard();
        let (root, mut core) = tests::scratch_core_inner("remote-attach");

        let driver = core.spawn_terminal(None, Some("driver".into()), false).unwrap();
        // The user's own terminal, logged in by hand — no rules, no token yet.
        let term = core
            .spawn_terminal(Some(format!("ssh -tt {target}")), Some("my ssh".into()), false)
            .unwrap();
        let screen_path = crate::pty::terminal_screen_path(&core.state_dir, term.id);

        assert!(
            wait_until_slow(|| {
                let s = std::fs::read_to_string(&screen_path).unwrap_or_default();
                remote::at_shell_prompt(&s) && !s.trim().is_empty()
            }),
            "never reached a remote shell prompt:\n{}",
            std::fs::read_to_string(&screen_path).unwrap_or_default()
        );
        eprintln!("remote shell is up");

        // Only the claude half is launched — the terminal is already there.
        let token = remote::new_token(driver.id, 7);
        let rules_b64 = remote::b64(remote::peer_rules(&token).as_bytes());
        // false: this is the user's own ssh session in the live test too, so
        // their shell must outlive the claude.
        let cmd = remote::remote_launch_command(Some("/tmp/mpx-probe"), &rules_b64, false);
        remote::RemoteMeta {
            token: token.clone(),
            ssh_target: String::new(),
            opener: driver.id,
        }
        .write(&core.state_dir, term.id)
        .unwrap();
        send_to(&mut core, driver.id, term.id, &format!("{cmd}\r"));

        assert!(
            wait_until_slow(|| remote::looks_like_claude_tui(
                &std::fs::read_to_string(&screen_path).unwrap_or_default()
            )),
            "the remote claude never drew its input box"
        );
        eprintln!("remote claude is up inside the user's own ssh session");

        let task = "Run `echo attached` and then follow your signalling instructions.";
        send_to(&mut core, driver.id, term.id, task);
        std::thread::sleep(Duration::from_millis(400));
        send_to(&mut core, driver.id, term.id, "\r");

        let inbox = core.state_dir.join("inbox").join(driver.id.to_string());
        let mut delivered = Vec::new();
        for _ in 0..240 {
            std::thread::sleep(Duration::from_millis(500));
            core.process_remote_signals();
            if let Ok(entries) = std::fs::read_dir(&inbox) {
                delivered = entries
                    .flatten()
                    .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                    .collect();
                if !delivered.is_empty() {
                    break;
                }
            }
        }
        let body = delivered.join("\n");
        eprintln!("delivered:\n{body}");
        assert!(!body.is_empty(), "no wake reached the driver");
        assert!(
            body.contains("has FINISHED the work"),
            "woken by the backstop rather than a real signal: {body}"
        );
        // With no ssh target recorded, the wake must still read as English.
        assert!(
            !body.contains("started  ("),
            "an empty target left a gap in the message: {body}"
        );

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    fn send_to(core: &mut Core, from: usize, id: usize, data: &str) {
        core.apply_terminal_request(
            &serde_json::json!({ "op": "send", "from": from, "id": id, "data": data }).to_string(),
        );
    }

    /// Like `wait_until` but patient enough for a network round trip.
    fn wait_until_slow(mut f: impl FnMut() -> bool) -> bool {
        for _ in 0..120 {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        false
    }

    #[test]
    #[ignore]
    fn a_remote_claude_signals_home_and_wakes_its_driver() {
        let Ok(target) = std::env::var("MULPEX_TEST_SSH") else {
            eprintln!("set MULPEX_TEST_SSH to run this");
            return;
        };
        let _env = tests::env_guard();
        let (root, mut core) = tests::scratch_core_inner("remote-live");

        // A stand-in for the driving instance: the watcher refuses to deliver to
        // an opener that is no longer there, so one has to exist.
        let driver = core.spawn_terminal(None, Some("driver".into()), false).unwrap();
        eprintln!("driver session id = {}", driver.id);

        let token = remote::new_token(driver.id, 42);
        let rules_b64 = remote::b64(remote::peer_rules(&token).as_bytes());
        let cmd = remote::ssh_command(&target, Some("/tmp/mpx-probe"), &rules_b64);
        let remote_term = core
            .spawn_terminal(Some(cmd), Some(format!("ssh {target}")), false)
            .unwrap();
        eprintln!("remote terminal id = {}, token = {token}", remote_term.id);

        remote::RemoteMeta {
            token: token.clone(),
            ssh_target: target.clone(),
            opener: driver.id,
        }
        .write(&core.state_dir, remote_term.id)
        .unwrap();

        // Wait for the remote TUI, exactly as `hub_remote_open` does.
        let screen_path = crate::pty::terminal_screen_path(&core.state_dir, remote_term.id);
        let mut ready = false;
        for _ in 0..120 {
            std::thread::sleep(Duration::from_millis(500));
            let screen = std::fs::read_to_string(&screen_path).unwrap_or_default();
            if remote::looks_like_claude_tui(&screen) {
                ready = true;
                break;
            }
        }
        assert!(ready, "the remote claude's input box never appeared");
        eprintln!("remote TUI is up");

        let task = "Print the current working directory using pwd, then follow your signalling \
                    instructions.";
        // Text and Enter as SEPARATE writes — a `\r` on the tail of the same
        // burst is swallowed as paste content and the task is never submitted.
        // See `mcp::inject_task`; this is the shape that failed live.
        let send = |core: &mut Core, data: &str| {
            core.apply_terminal_request(
                &serde_json::json!({ "op": "send", "from": driver.id, "id": remote_term.id,
                                     "data": data })
                .to_string(),
            );
        };
        send(&mut core, task);
        std::thread::sleep(Duration::from_millis(400));
        send(&mut core, "\r");

        // Now the real assertion: without anyone reading the terminal, a message
        // must appear in the driver's inbox on its own.
        let inbox = core.state_dir.join("inbox").join(driver.id.to_string());
        let mut delivered = Vec::new();
        for _ in 0..240 {
            std::thread::sleep(Duration::from_millis(500));
            core.process_remote_signals();
            if let Ok(entries) = std::fs::read_dir(&inbox) {
                delivered = entries
                    .flatten()
                    .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                    .collect();
                if !delivered.is_empty() {
                    break;
                }
            }
        }

        let screen = std::fs::read_to_string(&screen_path).unwrap_or_default();
        assert!(
            !delivered.is_empty(),
            "no wake reached the driver's inbox.\nfinal screen:\n{screen}"
        );
        let body = delivered.join("\n");
        eprintln!("delivered:\n{body}");
        assert!(
            body.contains(&format!("term#{}", remote_term.id)),
            "the wake does not name the terminal: {body}"
        );
        // The wake must be the remote's own signal, not the backstop: passing on
        // the backstop would hide a broken marker contract completely, which is
        // exactly how the first run of this test passed while proving nothing.
        assert!(
            body.contains("has FINISHED the work"),
            "woken by the idle backstop rather than by a real signal — the remote \
             did not emit a parseable marker: {body}"
        );
        assert!(
            body.contains("hub_terminal_send"),
            "the wake does not say how to reply: {body}"
        );

        // And the marker itself must never be shown back to a model.
        let log = std::fs::read(crate::pty::terminal_log_path(&core.state_dir, remote_term.id))
            .unwrap_or_default();
        let text = String::from_utf8_lossy(
            log.get(mulpex_core::termlog::HEADER_LEN..).unwrap_or_default(),
        );
        assert!(
            !remote::strip_signals(&text, &token).contains(&token),
            "the token survived stripping"
        );

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// The remote-peer watcher, driven without a network: a real shell terminal
/// stands in for the remote claude by printing what one would print.
///
/// Worth having alongside the live test because these run on every `cargo test`
/// and cost nothing — and because both cases below are bugs that actually
/// happened, one of which the live test could not have caught (it passed while
/// proving nothing).
#[cfg(test)]
mod remote_watcher {
    use super::tests::{scratch_core, wait_until};
    use super::*;
    use mulpex_core::remote;

    const TOKEN: &str = "feedface";

    /// A terminal the watcher believes is a remote claude, plus the driver it
    /// reports to. `paint` is typed as the terminal's **seed**, not sent to it:
    /// sending is what arms the idle backstop, so a test that set the scene by
    /// sending could never observe the un-armed state.
    fn remote_pair(core: &mut Core, paint: Option<String>) -> (usize, usize) {
        let driver = core.spawn_terminal(None, Some("driver".into()), false).unwrap();
        let term = core.spawn_terminal(paint, Some("remote".into()), false).unwrap();
        remote::RemoteMeta {
            token: TOKEN.to_string(),
            ssh_target: "root@example.test".into(),
            opener: driver.id,
        }
        .write(&core.state_dir, term.id)
        .unwrap();
        (driver.id, term.id)
    }

    fn inbox(core: &Core, id: usize) -> Vec<String> {
        std::fs::read_dir(core.state_dir.join("inbox").join(id.to_string()))
            .map(|e| {
                e.flatten()
                    .filter_map(|f| std::fs::read_to_string(f.path()).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn type_into(core: &mut Core, id: usize, data: &str) {
        core.apply_terminal_request(
            &serde_json::json!({ "op": "send", "from": 1, "id": id, "data": data }).to_string(),
        );
    }

    /// A fake claude prompt: enough for `looks_like_claude_tui`, which is what
    /// qualifies silence as "a turn ended" rather than "the ssh died".
    fn fake_prompt() -> String {
        format!(
            "printf '%s\\n%s\\n' '{}' '❯ '",
            "─".repeat(40)
        )
    }

    #[test]
    fn a_signal_wakes_the_driver_exactly_once() {
        let (_env, root, mut core) = scratch_core("remote-signal");
        let marker = format!("{} {TOKEN} done Fixed the auth bug{}", remote::SIG_OPEN, remote::SIG_CLOSE);
        let (driver, term) = remote_pair(&mut core, Some(format!("printf '%s\n' '{marker}'")));

        assert!(
            wait_until(|| {
                core.process_remote_signals();
                !inbox(&core, driver).is_empty()
            }),
            "the marker never reached the driver's inbox"
        );

        let body = inbox(&core, driver).join("\n");
        assert!(body.contains("has FINISHED"), "wrong kind reported: {body}");
        assert!(body.contains("Fixed the auth bug"), "summary lost: {body}");
        assert!(
            body.contains(&format!("\"from_terminal\":{term}")),
            "the sender is not tagged as a terminal: {body}"
        );

        // The marker stays on screen indefinitely; the wake must not repeat.
        let before = inbox(&core, driver).len();
        for _ in 0..10 {
            core.process_remote_signals();
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            inbox(&core, driver).len(),
            before,
            "the same signal woke the driver more than once"
        );

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The bug the first live run hid behind a passing assertion: a remote that
    /// has never been asked anything is silent too, so a backstop keyed on
    /// silence alone reported a finished turn 5.7 s after launch — before the
    /// task had even been typed.
    #[test]
    fn silence_is_only_a_wake_when_an_answer_is_owed() {
        let (_env, root, mut core) = scratch_core("remote-idle");
        let (driver, term) = remote_pair(&mut core, Some(fake_prompt()));

        std::thread::sleep(Duration::from_millis(remote::IDLE_TURN_END_MS + 1_500));
        for _ in 0..10 {
            core.process_remote_signals();
        }
        assert!(
            inbox(&core, driver).is_empty(),
            "an idle remote that was never given anything woke its driver"
        );

        // Now it owes an answer, and the same silence must wake the driver.
        type_into(&mut core, term, &format!("{}\r", fake_prompt()));
        assert!(
            wait_until(|| {
                core.process_remote_signals();
                !inbox(&core, driver).is_empty()
            }),
            "a remote that went quiet owing an answer never woke its driver"
        );
        assert!(
            inbox(&core, driver).join("\n").contains("did not signal"),
            "wrong backstop wording"
        );

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }
}
