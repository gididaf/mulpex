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

use mulpex_core::agent::Agent;
use mulpex_core::persist::{self, NameSource, SessionStore};
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

/// Set by `teardown_all`: the scratch root is being removed, and the poll
/// loop's registry write (outside the lock) must not put it back.
pub static TORN_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
    /// The remote desktop driving `geometry` right now, as (device id, name):
    /// while one is, this window's own resizes are ignored (`resize_terminals`)
    /// and its xterms follow the lease instead. `None`: this window drives it.
    /// See `remote/desktop.rs`.
    pub geometry_owner: Option<(String, String)>,
    /// ⌘⇧Q Checkpoint & Quit: who was asked to stop, and wakes still queued
    /// after a relaunch. See `checkpoint.rs`.
    pub checkpoint: crate::checkpoint::Checkpoint,
    /// ⌘⇧B Ask All Claudes: the one round in flight. See `broadcast.rs`.
    pub broadcast: crate::broadcast::Broadcast,
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
            geometry_owner: None,
            checkpoint: Default::default(),
            broadcast: Default::default(),
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
        // Named for the project, not the handle: the dir has to outlive this
        // app, since a claude that survives a restart (`ptyd.rs`) still has
        // this path as its `MULPEX_STATE_DIR`.
        let state_dir = self.state_root.join(crate::ptyd::project_key(&canon));
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
            geometry_owner: self.geometry_owner.as_ref().map(|(_, name)| name.clone()),
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

    /// A restart that keeps every session running (`ptyd::restart_detached`):
    /// save what the next app needs to adopt them — the store, and each
    /// project's row manifest — and let go without killing anything. The
    /// scratch root stays; it is the daemon's, and every claude still uses it.
    pub fn detach_all(&mut self) {
        for core in &mut self.projects {
            core.persist_sessions();
            core.adopt_manifest.clear();
            core.sync_adopt_manifest();
        }
        // `Session`'s Drop does not kill while `ptyd::detaching()`.
        self.projects.clear();
        crate::ptyd::detach();
    }

    /// The size the daemon's surviving sessions run at, adopted before any
    /// project opens: every PTY shares one geometry, and the frontend must
    /// build its xterms at exactly the size those panes were drawn for.
    pub fn adopt_geometry(&mut self) {
        let live = crate::ptyd::list();
        if let Some(s) = live.iter().find(|s| s["alive"] == true) {
            let (c, r) = (s["cols"].as_u64().unwrap_or(0) as u16, s["rows"].as_u64().unwrap_or(0) as u16);
            if c >= 1 && r >= 1 {
                self.geometry = (c, r);
            }
        }
    }

    /// After every project has opened: end whatever the daemon still holds that
    /// no project took — its project is no longer open, so (as before the
    /// daemon) nothing of it runs.
    pub fn kill_unadopted(&self) {
        let ours: HashSet<u32> = self.projects.iter().flat_map(|c| c.sessions.iter().map(|s| s.sid())).collect();
        for s in crate::ptyd::list() {
            let sid = s["sid"].as_u64().unwrap_or(0) as u32;
            if !ours.contains(&sid) {
                eprintln!("[ptyd] ending {} — no open project claims it", s["key"]);
                crate::ptyd::kill_sid(sid);
            }
        }
    }

    /// Kill every project's process groups and remove the whole scratch root. The
    /// "no orphaned claude" guarantee, now across all projects.
    pub fn teardown_all(&mut self) {
        TORN_DOWN.store(true, std::sync::atomic::Ordering::SeqCst);
        for core in &mut self.projects {
            core.teardown();
        }
        self.projects.clear();
        // The guarantee said "no orphaned claude" and stopped there, and hub
        // listeners quietly fell through the gap: `killpg` cannot reach one (own
        // process group) and neither can the tty sweep (no controlling terminal),
        // so every quit left one per instance behind, spinning until reboot. A
        // current listener exits by itself once this root disappears, but an
        // instance that was already running when this shipped is still on the old
        // shell loop, which never learned to. The claudes above are dead by now,
        // so theirs have just been reparented to launchd and are reapable.
        crate::pty::reap_orphaned_listeners();
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
    /// Absolute path to the `mulpex-helper` sidecar, kept so `ensure_state_dir`
    /// can rewrite the config files before every spawn (see `write_state_dir`).
    helper_path: PathBuf,
    pub store: SessionStore,
    /// Custom per-instance names (id → name), persisted alongside session ids.
    pub names: HashMap<usize, String>,
    /// Ids whose name the **user** owns — set by ⌘R, and by a restore bringing
    /// back a name saved as `User` (or with no saved source: protecting it is the
    /// safe direction). An instance's own `hub_set_name` is
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
    /// Who set each entry in `names` — reported by `hub_instances` (via the
    /// registry) as `name_set_by`, and persisted so it survives a restart. A
    /// name without an entry here is the user's (`name_source`).
    name_by: HashMap<usize, NameSource>,
    /// Muted instance ids (⌘M), persisted alongside session ids. Presentation
    /// only — a muted instance runs and coordinates exactly like any other; the
    /// frontend just dims it, sorts it last, and leaves it out of the badges.
    pub muted: HashSet<usize>,
    /// child id → the instance that `hub_spawn`ed it. Only `hub_spawn` creates a
    /// link (a ⌘T claude is always top-level), and a terminal is never a child.
    /// The sidebar draws its tree from this; when a parent goes away its
    /// children move up one level (`reap_dead`), so a value always names a row
    /// that still exists.
    pub parents: HashMap<usize, usize>,
    /// Rows whose `hub_spawn` family is folded away in the sidebar. Persisted.
    pub collapsed: HashSet<usize>,
    /// Auto-compact rules: row id → context % at which the row itself and every
    /// claude in its sidebar subtree is compacted (`lib/compact.ts` enforces it). Set only by
    /// the user, from the row's context menu. Persisted.
    pub auto_compact: HashMap<usize, u8>,
    /// Instance ids that have been "worked on" (restored, or fired a hook this
    /// run). Only these are persisted for restore.
    pub worked: HashSet<usize>,
    /// Sessions asked to go away (⌘W, `hub_terminal_close`). A dead *instance* is
    /// removed the moment it dies, but a dead *terminal* is kept so its output
    /// stays readable — so "dead" alone can't mean "remove", and this is what
    /// distinguishes the two. Removal still happens uniformly in `reap_dead`.
    closing: HashSet<usize>,
    /// Claudes stopped by ⌘S (`begin_save`). Saving means the user is done with
    /// that claude, so its process is killed at once — nothing can move on past
    /// the conversation the save forks — but its row is **kept**, locked, until
    /// the user closes it: the save's progress, its result and its Retry live
    /// there. Out of the hub like a failed row. Released by `close`.
    saving: HashSet<usize>,
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
    /// Last-written `terminals/meta`, so the 200 ms poll rewrites it only when a
    /// terminal actually starts or finishes a command, or changes directory.
    terminal_meta: String,
    /// Last-written row manifest (`ADOPT_MANIFEST`), for the same reason.
    adopt_manifest: String,
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
    /// The project's Board (`board.rs`), cached; the app is its only writer.
    pub board: crate::board::BoardStore,
    /// Each codex row's wake ring (`wake.rs`).
    wakes: crate::wake::Wakes,
    /// When `settle_codex_turns` last looked.
    codex_settled_at: Option<Instant>,
}

/// Minimum gap between launching two `hub_spawn` children. Each `claude` cold
/// start is heavy (node boot, MCP handshake, hook registration); firing a whole
/// batch at once made them contend badly enough that none was ready to be typed
/// into in time. Spacing them keeps each start fast enough to be usable.
const SPAWN_STAGGER: Duration = Duration::from_millis(500);

/// How long a codex's TUI takes to come up far enough to take a typed line or
/// a queued one, when its `SessionStart` has not said so yet (a fresh codex
/// fires that only at its first prompt).
const CODEX_UP: Duration = Duration::from_secs(4);

/// How often `settle_codex_turns` reads the rollouts of `working` codexes.
const CODEX_SETTLE_EVERY: Duration = Duration::from_secs(3);
/// How long a rollout must have been quiet after a turn's end before the app
/// takes over the `Stop` hook's job of writing `waiting`.
const CODEX_SETTLE_QUIET: Duration = Duration::from_secs(3);
/// How much of a rollout's end `settle_codex_turns` reads.
const CODEX_TAIL_BYTES: u64 = 64 * 1024;

/// How long a due Board message waits for its claude's listener before it is
/// left in the inbox regardless — `checkpoint.rs`'s `WAKE_WAIT`, for the same reason.
const TODOS_WAIT: Duration = Duration::from_secs(180);

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
    /// The agent to run, or `None` for the spawner's own.
    agent: Option<Agent>,
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
        let settings_path = state_dir.join("settings.json");
        mulpex_core::state_dir::write_state_dir(&state_dir, helper_path)?;

        let store = SessionStore::new(&project_dir);
        let mut sessions: Vec<Session> = Vec::new();
        let mut worked: HashSet<usize> = HashSet::new();
        let mut names: HashMap<usize, String> = HashMap::new();
        let mut name_by: HashMap<usize, NameSource> = HashMap::new();
        let mut muted: HashSet<usize> = HashSet::new();
        let mut parents: HashMap<usize, usize> = HashMap::new();
        let mut collapsed: HashSet<usize> = HashSet::new();
        let mut auto_compact: HashMap<usize, u8> = HashMap::new();
        let mut todos_on: HashSet<usize> = HashSet::new();
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

        // Sessions the daemon kept running through an app restart come back
        // first, as they were — same process, same pane, every row they had
        // (unprompted claudes and terminals too, which the store never held).
        // The row manifest the last app left in the scratch dir says how each
        // was shown; one spawned too late to be in it gets its defaults.
        let mut adopted_uuids: HashSet<String> = HashSet::new();
        let mut adopted_active: Option<usize> = None;
        {
            let prefix = pty::session_key(&state_dir, 0).trim_end_matches('0').to_string();
            let manifest = read_adopt_manifest(&state_dir);
            let mut live: Vec<serde_json::Value> = crate::ptyd::list()
                .into_iter()
                .filter(|s| s["alive"] == true && s["key"].as_str().is_some_and(|k| k.starts_with(&prefix)))
                .collect();
            let id_of = |s: &serde_json::Value| s["meta"]["id"].as_u64().unwrap_or(0) as usize;
            live.sort_by_key(|s| (manifest.iter().position(|r| r.id == id_of(s)).unwrap_or(usize::MAX), id_of(s)));
            for info in &live {
                let id = id_of(info);
                if id == 0 || used.contains(&id) {
                    continue;
                }
                let shell = info["meta"]["kind"] == "shell";
                let row = manifest.iter().find(|r| r.id == id);
                let session_id = row
                    .map(|r| r.session_id.clone())
                    .filter(|u| !u.is_empty())
                    .unwrap_or_else(|| info["meta"]["session_id"].as_str().unwrap_or("").to_string());
                let kind = pty::SessionKind::from_word(info["meta"]["kind"].as_str().unwrap_or("claude"));
                match Session::adopt(id, kind, &session_id, info, crate::ptyd::take_snapshot(&state_dir, id)) {
                    Ok(session) => {
                        used.insert(id);
                        if !shell {
                            adopted_uuids.insert(session_id);
                        }
                        if let Some(r) = row {
                            if r.active {
                                adopted_active = Some(id);
                            }
                            if r.worked {
                                worked.insert(id);
                            }
                            if let Some(name) = &r.name {
                                names.insert(id, name.clone());
                                if let Some(by) = r.name_by.as_deref().and_then(NameSource::parse) {
                                    name_by.insert(id, by);
                                }
                            }
                            if r.muted {
                                muted.insert(id);
                            }
                            if let Some(p) = r.parent {
                                parents.insert(id, p);
                            }
                            if r.collapsed {
                                collapsed.insert(id);
                            }
                            if let Some(pct) = r.auto_compact {
                                auto_compact.insert(id, pct);
                            }
                            if r.todos {
                                todos_on.insert(id);
                            }
                        }
                        sessions.push(session);
                    }
                    Err(e) => eprintln!("[ptyd] could not adopt {}: {e}", info["key"]),
                }
            }
        }

        for (position, saved) in store.load().into_iter().enumerate() {
            // Already running: adopted above, not resumed a second time.
            if adopted_uuids.contains(&saved.session_id) {
                continue;
            }
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
                SpawnSpec::Agent {
                    agent: saved.agent,
                    settings_path: &settings_path,
                    state_dir: &state_dir,
                    helper_path,
                    session_id: &saved.session_id,
                    resume: true,
                    initial_task: None,
                    parent_agent: Agent::Claude,
                    plain_prompt: None,
                },
            ) {
                worked.insert(id);
                restored.insert(id, Instant::now());
                started.insert(id, Instant::now());
                if let Some(name) = saved.name {
                    names.insert(id, name);
                    if let Some(by) = saved.name_by {
                        name_by.insert(id, by);
                    }
                }
                if saved.muted {
                    muted.insert(id);
                }
                if let Some(p) = saved.parent {
                    parents.insert(id, p);
                }
                if saved.collapsed {
                    collapsed.insert(id);
                }
                if let Some(pct) = saved.auto_compact {
                    auto_compact.insert(id, pct);
                }
                if saved.todos {
                    todos_on.insert(id);
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
        // A parent that did not come back (failed restore, hand-edited store)
        // leaves its children top-level rather than linked to nothing.
        parents.retain(|_, p| sessions.iter().any(|s| s.id == *p));
        // Continue past the highest number ever handed out, so a new ⌘T can never
        // collide with a restored instance (or with the gap a failed one left).
        let next_id = used.iter().copied().max().unwrap_or(0) + 1;
        let project_name = project_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| project_dir.display().to_string());

        // Rows that had the old per-claude To Do list on: only read, so the
        // first open after the Board replaced it can carry their lists over.
        let board = crate::board::BoardStore::beside(store.path(), &project_dir, &todos_on);
        let mut core = Self {
            handle,
            project_dir,
            project_name,
            sessions,
            active: 0,
            next_id,
            state_dir,
            settings_path,
            helper_path: helper_path.to_path_buf(),
            board,
            store,
            // A restored name the USER set (⌘R, a save's title — or one from a
            // store too old to say who set it) stays the user's, so the instance
            // is neither nudged to rename itself nor allowed to. A name the
            // instance gave itself, a spawn label or a guide title stays
            // renameable: locking those made `hub_set_name` silently fail for
            // every instance after any relaunch.
            manual_names: names
                .keys()
                .filter(|id| name_by.get(id).map_or(true, |by| *by == NameSource::User))
                .copied()
                .collect(),
            names,
            name_by,
            fallback_names: HashMap::new(),
            muted,
            parents,
            collapsed,
            auto_compact,
            worked,
            closing: HashSet::new(),
            saving: HashSet::new(),
            remote_awaiting: HashSet::new(),
            restored,
            started,
            failed: HashMap::new(),
            sticky,
            terminal_index: String::new(),
            terminal_meta: String::new(),
            adopt_manifest: String::new(),
            geometry,
            pending_spawns: VecDeque::new(),
            last_spawn_at: None,
            wakes: Default::default(),
            codex_settled_at: None,
        };
        // The row that had focus before the restart has it again.
        if let Some(id) = adopted_active {
            core.set_active(id);
        }
        core.persist_sessions();
        core.sync_board_flag();
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
            .map(|s| {
                let board = self.board.with(|b| b.counts(s.id));
                SessionInfo {
                    id: s.id,
                    name: self.display_name(s.id),
                    muted: self.muted.contains(&s.id),
                    parent: self.parents.get(&s.id).copied(),
                    collapsed: self.collapsed.contains(&s.id),
                    auto_compact: self.auto_compact.get(&s.id).copied(),
                    kind: kind_of(s),
                    exited: s.is_shell() && !s.is_alive(),
                    failed: self.failed.get(&s.id).cloned(),
                    board_open: board.open,
                    board_unread: board.unread,
                    board_urgent: board.urgent,
                    outdated: s.outdated(),
                    worked: self.worked.contains(&s.id),
                }
            })
            .collect()
    }

    /// Spawn a fresh Claude in the project dir and focus it (⌘T / the frontend
    /// `create_session` command). Ports `App::spawn_instance`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn spawn_instance(&mut self) -> anyhow::Result<SessionInfo> {
        self.spawn_agent(Agent::Claude)
    }

    /// Spawn a fresh agent of either kind and focus it — ⌘T (claude) or ⌘⌥T
    /// (codex).
    pub fn spawn_agent(&mut self, agent: Agent) -> anyhow::Result<SessionInfo> {
        self.spawn_with(agent, None, None, None, true)
    }

    /// The agent `id` runs, or `None` for a terminal or an unknown id.
    pub fn agent_of(&self, id: usize) -> Option<Agent> {
        self.sessions.iter().find(|s| s.id == id)?.kind.agent()
    }

    /// The conversation id `id` resumes (empty for a codex before its first
    /// prompt), or `None` for an unknown id.
    pub fn session_id_of(&self, id: usize) -> Option<String> {
        self.sessions.iter().find(|s| s.id == id).map(|s| s.session_id.clone())
    }

    /// Spawn a fresh Claude that starts on `prompt` exactly as written, as if the
    /// user had typed it (⌘L Load), and focus it. `name` labels the row. With
    /// `user_owned` it is kept like a ⌘R name — a save's title is what identifies
    /// that work, so the instance's own `hub_set_name` must not replace it; a
    /// guide's title is only a starting label, and the instance may rename the
    /// row after the specific incident.
    pub fn spawn_instance_with_prompt(
        &mut self,
        prompt: String,
        name: Option<String>,
        user_owned: bool,
    ) -> anyhow::Result<SessionInfo> {
        let info = self.spawn_with(Agent::Claude, None, Some(prompt), None, true)?;
        Ok(self.name_row(info, name, user_owned))
    }

    /// Spawn a new row that resumes conversation `uuid` (⌘L ▸ Continue
    /// conversation), focused and named like a load. The caller must have checked
    /// that no open row already holds `uuid`: two claudes on one transcript
    /// corrupt it. Marked `worked` + `restored` exactly like a startup restore, so
    /// a resume that dies at once is kept as a failed row with its reason rather
    /// than vanishing.
    pub fn spawn_instance_resuming(
        &mut self,
        agent: Agent,
        uuid: String,
        name: Option<String>,
    ) -> anyhow::Result<SessionInfo> {
        let info = self.spawn_with(agent, None, None, Some(uuid), true)?;
        self.worked.insert(info.id);
        self.restored.insert(info.id, Instant::now());
        Ok(self.name_row(info, name, true))
    }

    /// Name a just-spawned row (see `spawn_instance_with_prompt` for `user_owned`).
    fn name_row(&mut self, info: SessionInfo, name: Option<String>, user_owned: bool) -> SessionInfo {
        match name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()) {
            Some(name) => {
                self.names.insert(info.id, name.clone());
                // A save's title is the user's; a guide's is only Mulpex's
                // starting label (see `spawn_instance_with_prompt`).
                let by = if user_owned { NameSource::User } else { NameSource::Auto };
                self.name_by.insert(info.id, by);
                if user_owned {
                    self.manual_names.insert(info.id);
                }
                SessionInfo { name: Some(name), ..info }
            }
            None => info,
        }
    }

    /// Spawn a fresh agent that starts immediately on `task`, assigned by
    /// `parent_id` (an instance's `hub_spawn` call, dispatched by the poll loop).
    /// The child is auto-named from the task so the sidebar labels it, is told its
    /// parent (so it can report back via `hub_send`), and is NOT focused — the user
    /// stays on their current pane while children appear in the sidebar.
    ///
    /// `agent` is `hub_spawn`'s `agent`. `None` is the spawner's own kind — a codex
    /// fans out codexes unless it asks otherwise — and a claude when the spawner
    /// is gone.
    pub fn spawn_instance_with_task_as(
        &mut self,
        parent_id: usize,
        task: String,
        agent: Option<Agent>,
    ) -> anyhow::Result<SessionInfo> {
        let parent_agent = self.agent_of(parent_id).unwrap_or_default();
        let agent = agent.unwrap_or(parent_agent);
        let name = name_from_task(&task);
        // Publish the assignment BEFORE the child exists, both because injection
        // is asynchronous and because a spawn child's task is never captured by
        // the `UserPromptSubmit` hook (the injected prompt carries the
        // `[mulpex:hub]` sentinel, which that hook skips). Without this, every
        // spawned instance reports `task: ""` to `hub_instances` for its whole
        // life — the exact reading that made a merely-slow spawn look like a
        // dropped one.
        let id = self.next_id;
        let tasks_dir = self.state_dir.join("tasks");
        let _ = std::fs::create_dir_all(&tasks_dir);
        let _ = std::fs::write(tasks_dir.join(id.to_string()), task.trim());
        pty::mark_delivery_pending(&self.state_dir, id);
        let info = match self.spawn_with_parent(agent, Some(SpawnTask { parent_id, task }), parent_agent, None, None, false) {
            Ok(info) => info,
            Err(e) => {
                // Nothing to deliver to; don't leave the markers behind for a
                // number that will be handed to somebody else.
                let _ = std::fs::remove_file(tasks_dir.join(id.to_string()));
                pty::clear_delivery(&self.state_dir, id);
                return Err(e);
            }
        };
        // The row hangs under its spawner in the sidebar — only while that
        // spawner is a live claude here, so a stale or odd `from` makes a
        // top-level row rather than a link to nothing.
        let parent = self
            .sessions
            .iter()
            .any(|s| s.id == parent_id && !s.is_shell())
            .then_some(parent_id);
        if let Some(p) = parent {
            self.parents.insert(info.id, p);
        }
        // Born into a muted family, it starts muted like the rest of it.
        let muted = parent.is_some_and(|p| self.muted.contains(&p));
        if muted {
            self.muted.insert(info.id);
        }
        let info = SessionInfo { parent, muted, ..info };
        if let Some(name) = name {
            self.names.insert(info.id, name.clone());
            self.name_by.insert(info.id, NameSource::Spawn);
            return Ok(SessionInfo {
                name: Some(name),
                ..info
            });
        }
        Ok(info)
    }

    /// Re-lay the scratch dir before a spawn, repairing whatever macOS purged
    /// out from under a long-running Mulpex. See `write_state_dir` for why this
    /// runs on every spawn and not just at open.
    pub fn ensure_state_dir(&self) -> anyhow::Result<()> {
        mulpex_core::state_dir::write_state_dir(&self.state_dir, &self.helper_path).map_err(|e| {
            anyhow::anyhow!(
                "could not write Mulpex's scratch dir at {}: {e}",
                self.state_dir.display()
            )
        })
    }

    /// Shared spawn path: allocate an id, spawn the session (optionally with an
    /// initial task), append it, optionally focus it, and republish the peer list.
    fn spawn_with(
        &mut self,
        agent: Agent,
        initial_task: Option<SpawnTask>,
        plain_prompt: Option<String>,
        resume_uuid: Option<String>,
        focus: bool,
    ) -> anyhow::Result<SessionInfo> {
        self.spawn_with_parent(agent, initial_task, Agent::Claude, plain_prompt, resume_uuid, focus)
    }

    fn spawn_with_parent(
        &mut self,
        agent: Agent,
        initial_task: Option<SpawnTask>,
        parent_agent: Agent,
        plain_prompt: Option<String>,
        resume_uuid: Option<String>,
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
        self.ensure_state_dir()?;
        let id = self.next_id;
        let resume = resume_uuid.is_some();
        let session_id = resume_uuid.unwrap_or_else(|| fresh_session_id(agent));
        let session = Session::spawn(
            id,
            &self.project_dir,
            self.geometry.1,
            self.geometry.0,
            SpawnSpec::Agent {
                agent,
                settings_path: &self.settings_path,
                state_dir: &self.state_dir,
                helper_path: &self.helper_path,
                session_id: &session_id,
                resume,
                initial_task,
                parent_agent,
                plain_prompt,
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
            parent: None,
            collapsed: false,
            auto_compact: None,
            kind: SessionKind::from(agent),
            exited: false,
            failed: None,
            board_open: 0,
            board_unread: 0,
            board_urgent: 0,
            outdated: false,
            worked: false,
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
        self.ensure_state_dir()?;
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
            parent: None,
            collapsed: false,
            auto_compact: None,
            kind: SessionKind::Shell,
            exited: false,
            failed: None,
            board_open: 0,
            board_unread: 0,
            board_urgent: 0,
            outdated: false,
            worked: false,
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
            let parsed: Option<(usize, Vec<String>, Option<Agent>)> = serde_json::from_str::<serde_json::Value>(
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
                // Optional; missing (every spawn before Codex) means the
                // spawner's own agent.
                let agent = v.get("agent").and_then(|a| a.as_str()).and_then(Agent::parse);
                Some((from, tasks, agent))
            });
            let _ = std::fs::remove_file(&req);
            let Some((from, tasks, agent)) = parsed else { continue };

            // Queue the batch rather than launching it here: firing every child in
            // one tick makes N `claude` cold starts fight for CPU, and a child that
            // takes too long to paint its input box misses its task injection.
            let Some(stem) = req.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            self.pending_spawns.push_back(PendingSpawn {
                token: stem.to_string(),
                from,
                agent,
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
                    if let Ok(info) = self.spawn_instance_with_task_as(batch.from, task, batch.agent) {
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

    /// Fulfil the row requests instances left on disk (`hub_terminal_open` /
    /// `_send` / `_close`, and `hub_close`): apply each one, then write a
    /// `<token>.done` reply the waiting MCP tool reads. Returns whether a row was
    /// created or removed.
    ///
    /// The dir is `termreq` because terminals were the first thing to need this
    /// channel; it now carries instance ops too, and the name is a wire format the
    /// helper agrees on rather than a description of the contents.
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
            // Label a terminal this instance did NOT open — in practice one the
            // user opened with ⌘⇧T, which has no name at all and shows up as
            // `name: null`. Only an UNNAMED terminal can be labelled: a name is
            // usually the user's, and a row they deliberately titled must not be
            // silently relabelled by an instance.
            "name" => {
                let Some(id) = id else {
                    return (
                        serde_json::json!({ "ok": false, "error": "missing 'id'" }),
                        false,
                    );
                };
                let Some(label) = str_of("label") else {
                    return (
                        serde_json::json!({ "ok": false, "error": "missing 'label'" }),
                        false,
                    );
                };
                let is_shell = self.sessions.iter().any(|s| s.id == id && s.is_shell());
                if !is_shell {
                    let known = self.sessions.iter().any(|s| s.id == id);
                    let err = if known {
                        format!("#{id} is a Claude instance, not a terminal")
                    } else {
                        format!("no term#{id}")
                    };
                    return (serde_json::json!({ "ok": false, "error": err }), false);
                }
                if let Some(existing) = self.names.get(&id) {
                    return (
                        serde_json::json!({
                            "ok": false,
                            "error": format!(
                                "term#{id} is already named {existing:?} — leave it alone"
                            ),
                            "name": existing,
                        }),
                        false,
                    );
                }
                let label = label.replace(['\t', '\n'], " ");
                self.names.insert(id, label.clone());
                // Deliberately NOT persisted: terminals are not restored across a
                // restart (`persist_sessions` filters on kind), so there is
                // nothing for a stored label to come back to.
                (serde_json::json!({ "ok": true, "id": id, "name": label }), true)
            }
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
            // `hub_board` — a claude on the project's Board (`board.rs`).
            // Routed through here so the app is the board's only writer.
            "board" => (self.apply_board_request(&v), false),
            // A helper from before the Board (a claude that outlived an update).
            "todos" => (serde_json::json!({ "ok": false, "error":
                "The To Do list was replaced by the project's Board — use mcp__mulpex__hub_board." }), false),
            // `hub_close` — close claude rows, the inverse of `hub_spawn`. Ids are
            // decided one at a time and reported individually: a batch cleaning up
            // six workers must not lose the five it can close because the sixth is
            // still mid-turn.
            "close_instance" => {
                let force = v.get("force").and_then(|x| x.as_bool()).unwrap_or(false);
                let ids: Vec<usize> = v
                    .get("ids")
                    .and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|n| n.as_u64()).map(|n| n as usize).collect())
                    .unwrap_or_default();
                if ids.is_empty() {
                    return (
                        serde_json::json!({ "ok": false, "error": "missing 'ids'" }),
                        false,
                    );
                }
                let mut closed: Vec<usize> = Vec::new();
                let mut refused: Vec<serde_json::Value> = Vec::new();
                for id in ids {
                    // Resolved to a plain bool before any mutation, so the
                    // immutable borrow is gone by the time `close` needs `&mut`.
                    let is_shell = self.sessions.iter().find(|s| s.id == id).map(|s| s.is_shell());
                    match is_shell {
                        None => refused.push(serde_json::json!({
                            "id": id,
                            "reason": format!(
                                "no claude#{id} is open — it may already have been closed."
                            ),
                        })),
                        Some(true) => refused.push(serde_json::json!({
                            "id": id,
                            "reason": format!(
                                "term#{id} is a terminal, not a claude — close it with \
                                 hub_terminal_close."
                            ),
                        })),
                        // Only the user closes a row ⌘S holds: it carries the
                        // save's result, and its Close is what they wait for.
                        Some(false) if self.saving.contains(&id) => refused.push(serde_json::json!({
                            "id": id,
                            "reason": format!(
                                "claude#{id} is being saved by the user — they close it themselves."
                            ),
                        })),
                        Some(false) => match mulpex_core::close_busy_reason(&self.state_dir, id)
                            .filter(|_| !force)
                        {
                            Some(reason) => {
                                refused.push(serde_json::json!({ "id": id, "reason": reason }))
                            }
                            None => {
                                self.close(id);
                                closed.push(id);
                            }
                        },
                    }
                }
                let changed = !closed.is_empty();
                (
                    serde_json::json!({ "ok": true, "closed": closed, "refused": refused }),
                    changed,
                )
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

    /// Set (or, with `None`, drop) a claude's ⌘⇧K compact-request phase. The
    /// frontend drives the steps; this file is what the hook and the phone see.
    /// → `mulpex_core::COMPACTREQ_DIR`.
    pub fn set_compact_phase(&self, id: usize, phase: Option<&str>) {
        let path = mulpex_core::compactreq_path(&self.state_dir, id);
        match phase {
            Some(p) if matches!(p, "requested" | "compacting" | "resume") => {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&path, p);
            }
            _ => {
                let _ = std::fs::remove_file(&path);
            }
        }
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
            self.name_by.remove(&id);
            self.manual_names.remove(&id);
            self.clear_named(id);
        } else {
            self.names.insert(id, name.to_string());
            self.name_by.insert(id, NameSource::User);
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
                self.name_by.insert(id, NameSource::Instance);
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

    /// Who set the label `display_name` returns. A name with no recorded source
    /// is the user's: that is a store written before the source was saved, and
    /// treating a restored name as the user's is what Mulpex has always done.
    fn name_source(&self, id: usize) -> Option<NameSource> {
        if self.names.contains_key(&id) {
            Some(self.name_by.get(&id).copied().unwrap_or(NameSource::User))
        } else if self.fallback_names.contains_key(&id) {
            Some(NameSource::Auto)
        } else {
            None
        }
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
        // A parent's mute carries its whole `hub_spawn` family with it, both
        // ways. A child can still be (un)muted on its own afterwards.
        for id in self.family_of(id) {
            if muted {
                self.muted.insert(id);
            } else {
                self.muted.remove(&id);
            }
        }
        self.persist_sessions();
    }

    /// Fold or unfold `id`'s `hub_spawn` family in the sidebar. Presentation
    /// only, like mute; persisted so the sidebar comes back the way it was.
    pub fn set_collapsed(&mut self, id: usize, collapsed: bool) {
        let changed = if collapsed {
            self.collapsed.insert(id)
        } else {
            self.collapsed.remove(&id)
        };
        if changed {
            self.persist_sessions();
        }
    }

    /// Set (`Some(pct)`) or clear the auto-compact rule on claude#id and its
    /// sidebar subtree — only ever from the user's context menu. Persists, and
    /// tells the orchestrator once, so a pause to compact isn't mistaken for stuck.
    /// Returns false (and does nothing) for a terminal, an unknown row, a
    /// percent outside the menu's choices, or no change.
    pub fn set_auto_compact(&mut self, id: usize, pct: Option<u8>) -> bool {
        if !self.instances().any(|s| s.id == id) {
            return false;
        }
        if pct.is_some_and(|p| !AUTO_COMPACT_CHOICES.contains(&p)) {
            return false;
        }
        if self.auto_compact.get(&id).copied() == pct {
            return false;
        }
        match pct {
            Some(p) => self.auto_compact.insert(id, p),
            None => self.auto_compact.remove(&id),
        };
        self.persist_sessions();
        self.send_app_message(id, &auto_compact_message(pct));
        true
    }

    /// Turn the project's Board on or off (project tab menu, the Board tab).
    /// Every claude is told at once. False if nothing changed.
    pub fn set_board_on(&mut self, on: bool) -> bool {
        let changed = self
            .board
            .mutate(|b| {
                let was = b.on;
                b.on = on;
                Ok(was != on)
            })
            .unwrap_or(false);
        if !changed {
            return false;
        }
        self.sync_board_flag();
        let msg = crate::board::toggle_message(on, &self.board.with(|b| b.columns_text()));
        for id in self.live_claude_ids() {
            self.send_app_message(id, &msg);
        }
        true
    }

    /// `board_on` exists while the Board is on, holding its columns — the hook
    /// carries it with the rule on every turn (`hook::userpromptsubmit`), so a
    /// renamed column reaches every claude on its next turn.
    pub fn sync_board_flag(&self) {
        let path = self.state_dir.join(mulpex_core::BOARD_ON_FILE);
        let (on, cols) = self.board.with(|b| (b.on, b.columns_text()));
        if on {
            if std::fs::read_to_string(&path).ok().as_deref() != Some(cols.as_str()) {
                let tmp = path.with_extension("tmp");
                if std::fs::write(&tmp, &cols).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Move `id` (with its whole family) under `parent`, or to the top level
    /// with `None` — the sidebar's drag-to-nest. Presentation only, like
    /// folding: the hub never reads `parents`. `ids` is the sidebar order to
    /// commit with it, so the row lands where the drop put it in one step.
    ///
    /// Refused (nothing changes) for a terminal on either end, an unknown id, or
    /// a `parent` inside `id`'s own family — that would be a cycle, which the
    /// frontend's tree walks assume can never exist. Returns whether it applied.
    pub fn reparent(&mut self, id: usize, parent: Option<usize>, ids: &[usize]) -> bool {
        let is_claude = |id: usize| self.sessions.iter().any(|s| s.id == id && !s.is_shell());
        if !is_claude(id) {
            return false;
        }
        match parent {
            Some(p) => {
                if !is_claude(p) || self.family_of(id).contains(&p) {
                    return false;
                }
                self.parents.insert(id, p);
                // Unfold the new parent so the moved row stays in sight.
                self.collapsed.remove(&p);
            }
            None => {
                self.parents.remove(&id);
            }
        }
        self.reorder_sessions(ids); // persists
        true
    }

    /// `id` and every row under it in the sidebar tree.
    fn family_of(&self, id: usize) -> Vec<usize> {
        let mut family = vec![id];
        let mut i = 0;
        while i < family.len() {
            let parent = family[i];
            family.extend(self.parents.iter().filter(|(_, p)| **p == parent).map(|(c, _)| *c));
            i += 1;
        }
        family
    }

    /// Unmute any instance the user has just spoken to: one `userprompt/<id>`
    /// mark per real user prompt, written by the `UserPromptSubmit` hook.
    /// Returns whether any row changed (the poll loop republishes on the diff).
    ///
    /// Mute means "stop putting this in front of me". Sending it a prompt is the
    /// user putting it in front of themselves, so leaving the row dimmed and sunk
    /// below the others would hide the one instance they are actively working
    /// with. Consume-and-delete like `process_name_requests`, and unmuting an
    /// already-unmuted row is a no-op — the common case, since the hook writes
    /// the mark for every instance rather than only the muted ones (it cannot see
    /// the mute state, which lives here).
    ///
    /// **Only a genuine user prompt gets here.** A `<task-notification>` — a hub
    /// wake, a finished background job — fires `UserPromptSubmit` exactly like a
    /// prompt, and unmuting on one would undo a ⌘M the moment a peer sent mail.
    /// The hook is the only side that can tell them apart, so it writes the mark
    /// only on the user's turns and this reader trusts it.
    pub fn process_user_prompts(&mut self) -> bool {
        let dir = self.state_dir.join(mulpex_core::USERPROMPT_DIR);
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
            let _ = std::fs::remove_file(&path);
            let Some(id) = id else { continue };
            if self.muted.remove(&id) {
                changed = true;
            }
        }
        if changed {
            self.persist_sessions();
        }
        changed
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
        // Gone for good: a later claude with this number starts on the
        // default Live Token, not on this one's pin.
        crate::live_token::forget(&crate::live_token::route(&self.project_dir, id));
        self.closing.insert(id);
        self.saving.remove(&id);
        if let Some(session) = self.session_mut(id) {
            session.kill();
        }
    }

    /// ⌘S: stop claude#`id` for good and hold its row for the save. Returns the
    /// project dir and the conversation uuid the save forks.
    ///
    /// Refused mid-turn and over an open question rather than warned about: the
    /// save is the last word on this conversation, so a step it would miss is
    /// lost for good. A retry on a row already held skips the checks and the
    /// kill — there is no process left to be busy.
    pub fn begin_save(&mut self, id: usize) -> Result<(PathBuf, String), String> {
        if let Some(why) = self.save_refusal(id) {
            return Err(why);
        }
        let uuid = self.sessions.iter().find(|s| s.id == id).map(|s| s.session_id.clone()).unwrap_or_default();
        if !self.saving.contains(&id) {
            self.saving.insert(id);
            if let Some(session) = self.session_mut(id) {
                session.kill();
            }
            self.write_live_instances();
        }
        Ok((self.project_dir.clone(), uuid))
    }

    /// ⌘⇧E: the project dir and conversation uuid an explain forks. Unlike
    /// `begin_save` it touches nothing — the fork only reads the transcript, so
    /// mid-turn and over an open question are both fine.
    ///
    /// Gated on the transcript FILE, not on `worked`: a conversation opened with
    /// an in-TUI `/resume` has its whole history on disk before anyone prompts
    /// it, and `worked` only turns on at the first prompt. (`session_id` follows
    /// the file through `reconcile_session_ids`.)
    pub fn explain_target(&self, id: usize) -> Result<(PathBuf, String), String> {
        let s = self
            .sessions
            .iter()
            .find(|s| s.id == id)
            .ok_or(format!("claude#{id} is not open"))?;
        let Some(agent) = s.kind.agent() else {
            return Err("Explain works on claudes and codexes only".into());
        };
        if s.session_id.is_empty()
            || !crate::agent_transcript::path(agent, &self.project_dir, &s.session_id, &self.state_dir, id)
                .is_some_and(|p| p.is_file())
        {
            return Err(format!("Nothing to explain yet — this {} has no conversation", agent.word()));
        }
        Ok((self.project_dir.clone(), s.session_id.clone()))
    }

    /// Why ⌘S must not save claude#`id` now, or `None`. Asked before the confirm
    /// (`save_check`) and again by `begin_save`. A `working` claude is allowed
    /// when its turn is over and only background work is left
    /// (`only_background_work`) — saving stops that work with it.
    pub fn save_refusal(&self, id: usize) -> Option<String> {
        let Some(s) = self.sessions.iter().find(|s| s.id == id) else {
            return Some(format!("claude#{id} is not open"));
        };
        if s.is_shell() {
            return Some(format!("term#{id} is a terminal — only a claude can be saved"));
        }
        if s.session_id.is_empty() || !self.worked.contains(&id) {
            return Some(format!("claude#{id} has nothing to save yet — send it a prompt first"));
        }
        if self.saving.contains(&id) {
            return None;
        }
        let status = std::fs::read_to_string(self.state_dir.join(id.to_string())).unwrap_or_default();
        match status.trim() {
            "working" if !mulpex_core::only_background_work(&self.state_dir, id) => {
                Some("Wait for it to finish first".into())
            }
            "needs" => Some("Answer or close the open question first".into()),
            _ => None,
        }
    }

    /// The save of a held row succeeded: drop it from the restore store, so a
    /// quit before the user clicks Close does not bring the claude back next
    /// launch. Saved is done; the conversation stays reachable through ⌘L.
    /// Not done on failure — then the store is the only way back to it.
    pub fn save_succeeded(&mut self, id: usize) {
        if self.saving.contains(&id) {
            self.worked.remove(&id);
            self.persist_sessions();
        }
    }

    /// Relaunch one claude **in place** (⌘⇧R): kill the child, then spawn a new
    /// one on the same row with `--resume`, so the conversation carries on inside
    /// a process that has just re-read the world.
    ///
    /// That re-reading is the whole point. A `claude` resolves its environment
    /// once, at exec — a rotated `CLAUDE_CODE_OAUTH_TOKEN`, a skill installed
    /// five minutes ago, an edited `~/.claude/settings.json` reach a running
    /// instance never. ⌘W then ⌘T already picks them up, but hands back a
    /// *different* instance: new number, no name, empty inbox, and a conversation
    /// to go and find. This keeps everything that identifies the row — its id and
    /// therefore its hub address, its name, its mute, its place in the sidebar,
    /// and its undelivered mail — and replaces only the process.
    ///
    /// An instance that has never had a prompt submitted has no transcript on
    /// disk, so `claude --resume <uuid>` would exit within seconds (measured —
    /// see the ghost-uuid test below). Such a claude is restarted **fresh**
    /// instead: same row, a new conversation uuid, no `--resume` — it had no
    /// conversation to lose. `worked` answers exactly that question: it latches
    /// when the first `UserPromptSubmit` hook writes the status file. This is
    /// what lets the ↻ badge (an outdated spawn, `spawnprint.rs`) be cleared on
    /// any claude.
    pub fn restart_instance(&mut self, id: usize) -> anyhow::Result<()> {
        let pos = self
            .sessions
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| anyhow::anyhow!("claude#{id} is not open"))?;
        let Some(agent) = self.sessions[pos].kind.agent() else {
            anyhow::bail!("term#{id} is a terminal — only a claude can be restarted");
        };
        let me = format!("{}#{id}", agent.word());
        if self.saving.contains(&id) {
            anyhow::bail!("{me} was saved — continue it from ⌘L instead");
        }
        let old_session_id = self.sessions[pos].session_id.clone();
        let resume = !old_session_id.is_empty() && self.worked.contains(&id);
        let session_id = if resume { old_session_id } else { fresh_session_id(agent) };
        self.respawn(pos, agent, session_id, resume, None)
    }

    /// Replace the agent on `id`'s row with a fresh `agent` that starts on
    /// `plain_prompt` (⌘⌥S Continue in Codex/Claude, `handoff.rs`). Everything
    /// that identifies the row — id and hub address, name, mute, parent and
    /// children, undelivered mail — is kept, exactly as ⌘⇧R keeps it; only the
    /// process and its conversation change. The caller emits
    /// `session-restarted`, as `restart_session` does.
    ///
    /// A ⌘S-style hold (`saving`) is released: the handoff that held the row is
    /// what calls this.
    pub fn replace_with_agent(&mut self, id: usize, agent: Agent, plain_prompt: String) -> anyhow::Result<()> {
        let pos = self
            .sessions
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| anyhow::anyhow!("#{id} is not open"))?;
        if self.sessions[pos].is_shell() {
            anyhow::bail!("term#{id} is a terminal — only an agent row can continue in another agent");
        }
        self.saving.remove(&id);
        // A new conversation: nothing of the old one's may be adopted by it.
        let _ = std::fs::remove_file(mulpex_core::session_id_path(&self.state_dir, id));
        let _ = std::fs::remove_file(mulpex_core::transcript_path(&self.state_dir, id));
        let _ = std::fs::remove_file(mulpex_core::resumed_in_place_path(&self.state_dir, id));
        self.worked.remove(&id);
        self.respawn(pos, agent, fresh_session_id(agent), false, Some(plain_prompt))
    }

    /// Kill row `pos` and start `agent` in its place with the same id —
    /// `restart_instance` (same agent, resuming) and `replace_with_agent` (the
    /// other agent, fresh) share it.
    fn respawn(
        &mut self,
        pos: usize,
        agent: Agent,
        session_id: String,
        resume: bool,
        plain_prompt: Option<String>,
    ) -> anyhow::Result<()> {
        let id = self.sessions[pos].id;
        // Same check ⌘T makes, for the same reason: if macOS is withholding the
        // folder, the respawn would fail *after* we had already killed the
        // instance the user still has.
        if let Some(reason) = pty::dir_access_error(&self.project_dir) {
            anyhow::bail!(reason);
        }
        self.ensure_state_dir()?;

        // Stamped BEFORE the kill, not after a successful spawn. If the respawn
        // fails, these are what make `reap_dead` read the corpse as a failed
        // *restore*: the row is kept with the reason printed into its own pane,
        // and `persist_sessions` still writes its `session_id`. Without them the
        // dead session is an ordinary exit — removed from the list, and the
        // conversation's uuid gone from the store along with it.
        //
        // A fresh restart has no conversation to keep, so it is an ordinary
        // start: an early death fails the row, and the store keeps nothing.
        self.started.insert(id, Instant::now());
        if resume {
            self.restored.insert(id, Instant::now());
        } else {
            self.restored.remove(&id);
        }

        // Kill first, and finish killing before spawning: two claudes appending
        // to one transcript is not a state `--resume` can be asked to make sense
        // of afterwards.
        self.sessions[pos].kill();

        // The status file is an assertion left by a process that no longer
        // exists. An instance killed while it was waiting on the user would go on
        // telling the sidebar, the dock badge and every peer that it `needs`
        // something. Removing it reads as `waiting` (`mcp::status_of`'s default),
        // which is the truth about a claude that is booting. Deliberately NOT
        // cleared: the inbox, the task line and the `named` marker — those
        // describe the instance, which is the thing being kept.
        let _ = std::fs::remove_file(self.state_dir.join(id.to_string()));
        // `armed/<id>` tracks a *live* hub-listener Monitor, and that process is
        // gone with the old child. Left behind, the `UserPromptSubmit` hook sees
        // the flag, skips the arm nudge for good, and the resumed instance is
        // never woken by hub mail again — with nothing anywhere to say so.
        let _ = std::fs::remove_file(self.state_dir.join("armed").join(id.to_string()));
        // The context % belongs to the old process; the new one reports its own
        // on its first statusline draw.
        let _ = std::fs::remove_file(mulpex_core::ctx_path(&self.state_dir, id));
        // A compact request was for the old process's conversation turn.
        let _ = std::fs::remove_file(mulpex_core::compactreq_path(&self.state_dir, id));
        // A codex's "has a thread, hooks work" mark and its wake ledger describe
        // the old process too; the new one earns its own (`wake.rs`).
        let _ = std::fs::remove_file(mulpex_core::agentready_path(&self.state_dir, id));
        let _ = std::fs::remove_file(mulpex_core::wake_ack_path(&self.state_dir, id));
        self.wakes.forget(id);
        // ...and because it was cleared, the resumed child has to re-arm — which,
        // with no task on its command line, only happens if it takes a turn. The
        // one thing that makes it take one is the "orphaned background task" wake
        // the dead Monitor is about to produce, and the `UserPromptSubmit` hook
        // swallows that wake by default (it is what made every instance open
        // itself after an app update). This flag is the exception: it tells the
        // hook that *this* wake was asked for, so the instance comes back armed
        // with its inbox drained instead of silently deaf to hub mail.
        //
        // A fresh claude needs none of that: nothing is resumed, and its listener
        // is armed at session start like any new instance's. Its old process's
        // uuid report is dropped too, or `reconcile_session_ids` would hand the
        // new process the old uuid back.
        //
        // A codex has no listener to re-arm (the app rings it, `wake.rs`), so
        // the flag is a claude's only.
        if resume && agent == Agent::Claude {
            let _ = std::fs::create_dir_all(self.state_dir.join(mulpex_core::RESUMED_DIR));
            let _ = std::fs::write(
                mulpex_core::resumed_in_place_path(&self.state_dir, id),
                "",
            );
        } else if !resume {
            let _ = std::fs::remove_file(mulpex_core::session_id_path(&self.state_dir, id));
        }

        let settings_path = self.settings_path.clone();
        let state_dir = self.state_dir.clone();
        let helper_path = self.helper_path.clone();
        let session = Session::spawn(
            id,
            &self.project_dir,
            self.geometry.1,
            self.geometry.0,
            SpawnSpec::Agent {
                agent,
                settings_path: &settings_path,
                state_dir: &state_dir,
                helper_path: &helper_path,
                session_id: &session_id,
                resume,
                initial_task: None,
                parent_agent: Agent::Claude,
                plain_prompt,
            },
        )?;
        // Dropping the old `Session` here `kill`s it a second time, which is
        // idempotent by contract.
        self.sessions[pos] = session;
        // A row that had failed to start is a live peer again — `live_instances`
        // (hence the peer list on disk and the hub snapshot) filters on this.
        self.failed.remove(&id);
        self.write_live_instances();
        Ok(())
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
                    let reason = self.start_failure_reason(&session);
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
                        .is_some_and(|t| t.elapsed() < RESTORE_GRACE)
                    // …and never came up. A resume that worked fires
                    // `SessionStart` (the hook writes `sessionid/<id>`) about a
                    // second in; one `claude` cannot open fires nothing and
                    // exits ("No conversation found") — both measured on
                    // v2.1.291. Without this, a restored claude the user quit
                    // with Ctrl+C inside the grace came back at the next launch.
                    && !mulpex_core::session_id_path(&self.state_dir, session.id).exists()
                    // Or it never exited at all: the PTY daemon went away and
                    // took it along. The conversation is intact on disk, so the
                    // record stays for the next launch's `--resume`.
                    || (session.lost_with_daemon()
                        && !self.closing.contains(&session.id)
                        && self.worked.contains(&session.id));
                if lost_restore && !session.session_id.is_empty() {
                    // Said out loud: a row the user believes closed coming back
                    // at the next launch was reported once and never reproduced.
                    eprintln!(
                        "[restore] {}#{} died unasked {:?} after its restore — kept for the next launch",
                        session.kind.word(),
                        session.id,
                        self.restored.get(&session.id).map(|t| t.elapsed()),
                    );
                    let record = persist::SavedSession {
                        session_id: session.session_id.clone(),
                        name: self.names.get(&session.id).cloned(),
                        muted: self.muted.contains(&session.id),
                        id: Some(session.id),
                        parent: self.parents.get(&session.id).copied(),
                        collapsed: self.collapsed.contains(&session.id),
                        auto_compact: self.auto_compact.get(&session.id).copied(),
                        name_by: self.name_by.get(&session.id).copied(),
                        todos: false,
                        agent: session.kind.agent().unwrap_or_default(),
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
        // A removed parent's children move up one level, to its own parent or
        // to the top. Order-independent: a removed child of a removed parent
        // hands its children up first or second, and both end at the nearest
        // live ancestor.
        for gone in &removed {
            let up = self.parents.remove(gone);
            let kids: Vec<usize> = self
                .parents
                .iter()
                .filter(|(_, p)| **p == *gone)
                .map(|(c, _)| *c)
                .collect();
            for kid in kids {
                match up {
                    Some(up) => self.parents.insert(kid, up),
                    None => self.parents.remove(&kid),
                };
            }
        }
        self.parents.retain(|id, _| self.sessions.iter().any(|s| s.id == *id));
        self.collapsed.retain(|id| self.sessions.iter().any(|s| s.id == *id));
        self.auto_compact.retain(|id, _| self.sessions.iter().any(|s| s.id == *id));
        self.closing.retain(|id| self.sessions.iter().any(|s| s.id == *id));
        self.saving.retain(|id| self.sessions.iter().any(|s| s.id == *id));
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
        !s.is_shell() && !self.saving.contains(&s.id) && !self.failed_to_start(s)
    }

    /// A dead instance that failed to start and has not been marked as such yet
    /// — i.e. this reap pass still owes it a reason and a notice in its pane.
    fn needs_failure_mark(&self, s: &Session) -> bool {
        !s.is_alive()
            && !s.is_shell()
            && !self.closing.contains(&s.id)
            && !self.saving.contains(&s.id)
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
    fn start_failure_reason(&self, s: &Session) -> String {
        let agent = s.kind.agent().unwrap_or_default();
        match pty::dir_access_error(&self.project_dir) {
            Some(reason) => format!("{} could not start here. {reason}", agent.title()),
            None => match agent {
                Agent::Claude => "Claude exited immediately, before this session was usable. \
                                  Its output above is all it managed to say. Press ⌘W to \
                                  dismiss this, or ⌘T to try again."
                    .to_string(),
                // `codex resume <id>` on a thread Codex no longer has prints
                // "No saved session found" and exits 1 (measured, docs/codex.md).
                Agent::Codex if self.restored.contains_key(&s.id) && !s.session_id.is_empty() => format!(
                    "Codex could not reopen this conversation ({}). Its output above says why. \
                     Press ⌘W to dismiss this, or ⌘⌥T to start a new Codex session.",
                    s.session_id
                ),
                Agent::Codex => "Codex exited immediately, before this session was usable. \
                                 Its output above is all it managed to say. Press ⌘W to \
                                 dismiss this, or ⌘⌥T to try again."
                    .to_string(),
            },
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
        // The transcript this id was writing to. Left behind, a recycled number
        // would adopt the dead instance's conversation on its first tick.
        let _ = std::fs::remove_file(mulpex_core::session_id_path(&self.state_dir, id));
        let _ = std::fs::remove_file(mulpex_core::ctx_path(&self.state_dir, id));
        let _ = std::fs::remove_file(mulpex_core::compactreq_path(&self.state_dir, id));
        let _ = std::fs::remove_file(mulpex_core::agentready_path(&self.state_dir, id));
        let _ = std::fs::remove_file(mulpex_core::transcript_path(&self.state_dir, id));
        let _ = std::fs::remove_file(mulpex_core::wake_ack_path(&self.state_dir, id));
        crate::pty::clear_delivery(&self.state_dir, id);
        let _ = std::fs::remove_file(crate::pty::terminal_log_path(&self.state_dir, id));
        let _ = std::fs::remove_file(crate::pty::terminal_screen_path(&self.state_dir, id));
        let _ = std::fs::remove_file(crate::pty::terminal_frames_path(&self.state_dir, id));
        let _ = std::fs::remove_file(terminal_mark_path(&self.state_dir, id));
        // A remote terminal's token and seen-markers go with it: a recycled id
        // must never inherit another terminal's identity.
        mulpex_core::remote::forget_all(&self.state_dir, id);
        // Its one-off ⌘K secrets: they were handed to this instance only.
        let _ = std::fs::remove_dir_all(crate::secrets::instance_dir(&self.state_dir, id));
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

            // Which TUI to read the screen as: a remote claude or a codex.
            let remote_agent = mulpex_core::remote::agent_of(&self.state_dir, id);
            // Idleness comes from the log header the recorder maintains, not
            // from the Session: the recorder is what actually sees the bytes.
            let idle_ms = mulpex_core::termlog::parse_header(&log_bytes)
                .map(|h| crate::vtgrid::now_ms().saturating_sub(h.last_out_ms))
                .unwrap_or(0);
            let quiet = session.is_alive()
                && idle_ms >= mulpex_core::remote::IDLE_TURN_END_MS
                && mulpex_core::remote::looks_like_tui(remote_agent, &screen)
                && !mulpex_core::remote::has_spinner_for(remote_agent, &screen);

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
        self.write_inbox(to, |ts| {
            serde_json::json!({
                "from": 0,
                "from_terminal": from_terminal,
                "ts": ts,
                "body": body,
            })
        });
    }

    /// One `hub_board` call. `from` is the caller's env identity.
    fn apply_board_request(&mut self, v: &serde_json::Value) -> serde_json::Value {
        let Some(from) = v.get("from").and_then(|x| x.as_u64()).map(|n| n as usize) else {
            return serde_json::json!({ "ok": false, "error": "missing 'from'" });
        };
        if !self.board.with(|b| b.on) {
            return serde_json::json!({
                "ok": false,
                "error": "The Board is OFF for this project, so it can't be used. Don't retry. If \
                          the user asked you to use it, tell them to turn it on: right-click the \
                          project's tab in Mulpex → \"Board\", or open the side panel's Board tab.",
            });
        }
        let action = v.get("action").and_then(|x| x.as_str()).unwrap_or("");
        let all = v.get("scope").and_then(|x| x.as_str()) == Some("all");
        let full = v.get("id").and_then(|x| x.as_u64()).map(|n| n as u32);
        let img_dir = self.board.img_dir().to_path_buf();
        if action == "list" {
            let view = self.board.with(|b| b.claude_view(from, all, full, &img_dir));
            return serde_json::json!({ "ok": true, "board": view });
        }
        let now = crate::board::now();
        let store = &self.board;
        let r = store.mutate(|b| {
            let mut import = |p: &str| store.import_image(p);
            b.apply_claude(from, v, now, &mut import)
        });
        match r {
            Ok((note, events)) => {
                if !events.is_empty() {
                    self.board.outbox(|o, _| o.push(events, false, Instant::now()));
                }
                serde_json::json!({
                    "ok": true,
                    "note": note,
                    "board": self.board.with(|b| b.claude_view(from, false, None, &img_dir)),
                })
            }
            Err(e) => serde_json::json!({ "ok": false, "error": e }),
        }
    }

    /// The board as the panel draws it, plus the claudes a message is waiting for.
    pub fn board_view(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self.board.get()).unwrap_or_default();
        let pending = self.board.pending();
        let (sending, unsent) = self.board.with_outbox(|o| (o.sending_ids(), o.unsent_comments()));
        v["unsent"] = serde_json::json!(unsent);
        v["pending"] = serde_json::json!(pending);
        v["sending"] = serde_json::json!(sending);
        v
    }

    /// The user's edit to the board (desktop panel, remote desktop, phone).
    pub fn board_edit(&mut self, edit: crate::board::BoardEdit) -> Result<serde_json::Value, String> {
        if let crate::board::BoardEdit::Comment { images, .. } | crate::board::BoardEdit::Add { images, .. } = &edit {
            if let Some(bad) = images.iter().find(|i| !self.board.has_image(i)) {
                return Err(format!("image {bad} is gone — attach it again"));
            }
        }
        if let crate::board::BoardEdit::Edit { images: Some(images), .. } = &edit {
            if let Some(bad) = images.iter().find(|i| !self.board.has_image(i)) {
                return Err(format!("image {bad} is gone — attach it again"));
            }
        }
        if let crate::board::BoardEdit::CommentEdit { id, .. } | crate::board::BoardEdit::CommentDelete { id, .. } = &edit {
            if !self.board.with_outbox(|o| o.unsent_comments()).contains(id) {
                return Err("that comment was already sent — add a new one instead".into());
            }
        }
        if let crate::board::BoardEdit::CommentEdit { images, .. } = &edit {
            if let Some(bad) = images.iter().find(|i| !self.board.has_image(i)) {
                return Err(format!("image {bad} is gone — attach it again"));
            }
        }
        let now = crate::board::now();
        if let crate::board::BoardEdit::Approve { proposal } | crate::board::BoardEdit::Reject { proposal, .. } = &edit {
            let (approve, reason) = match &edit {
                crate::board::BoardEdit::Reject { reason, .. } => (false, reason.as_str()),
                _ => (true, ""),
            };
            let (by, msg, events) = self.board.mutate(|b| b.decide(*proposal, approve, reason, now))?;
            // Like the user's own column edit: moved cards wait for UPDATE.
            if !events.is_empty() {
                self.board.outbox(|o, _| o.push(events, true, Instant::now()));
            }
            // The answer goes now. A row that is gone for good has no one to tell.
            if self.sessions.iter().any(|s| s.id == by) {
                self.send_app_message(by, &msg);
            }
            self.sync_board_flag();
            self.board.touch();
            return Ok(self.board_view());
        }
        let (events, gone) = self.board.mutate(|b| b.apply_user(edit, now))?;
        if !events.is_empty() {
            self.board.outbox(|o, _| o.push(events, true, Instant::now()));
        }
        self.board.remove_images(&gone);
        self.sync_board_flag();
        self.board.touch();
        Ok(self.board_view())
    }

    /// UPDATE: the user's waiting Board changes go out — to `ids`, or to every
    /// claude with some. A claude that is not running keeps its changes.
    pub fn board_update(&mut self, ids: Option<Vec<usize>>) -> serde_json::Value {
        let live = self.live_claude_ids();
        let pending: Vec<usize> = self.board.pending().into_keys().collect();
        let targets = ids.unwrap_or_else(|| pending.clone());
        let mut sent = Vec::new();
        let mut refused = Vec::new();
        for id in targets {
            if !pending.contains(&id) {
                continue;
            }
            if !live.contains(&id) {
                refused.push(format!("claude#{id} is not running — its changes are kept"));
                continue;
            }
            if self.board.outbox(|o, b| o.release(id, b, Instant::now())) {
                sent.push(id);
            }
        }
        serde_json::json!({ "sent": sent, "refused": refused })
    }

    /// Deliver released Board changes (UPDATE, or a claude's change for another
    /// claude), with the same gates as every app message: the claude's listener
    /// is up, or it is mid-turn (the Stop hook finds the mail), or it has waited
    /// `TODOS_WAIT` and the mail is left for its next turn anyway —
    /// `checkpoint.rs::drain_wake_queue`'s reasoning.
    pub fn drain_board_outbox(&mut self) {
        if self.board.with_outbox(|o| o.is_empty()) {
            return;
        }
        // A row that is gone for good (⌘W) takes its queue with it; a restored
        // one keeps its row, so its queue waits.
        let rows: HashSet<usize> = self.sessions.iter().map(|s| s.id).collect();
        if self.board.with_outbox(|o| o.waiting_ids().iter().chain(o.sending_ids().iter()).any(|id| !rows.contains(id))) {
            self.board.outbox(|o, _| o.forget(|id| rows.contains(&id)));
        }
        let live = self.live_claude_ids();
        for (id, waited) in self.board.with_outbox(|o| o.due(Instant::now())) {
            if !live.contains(&id) {
                continue;
            }
            let working = std::fs::read_to_string(self.state_dir.join(id.to_string()))
                .is_ok_and(|w| w.trim() == "working");
            if !(self.wake_ready(id) || working || waited >= TODOS_WAIT) {
                continue;
            }
            let img_dir = self.board.img_dir().to_path_buf();
            if let Some(msg) = self.board.outbox(|o, b| o.take(id, b, &img_dir)) {
                self.send_app_message(id, &msg);
            }
        }
    }

    /// Drop a message into `claude#to`'s inbox **as the app itself** — sender
    /// "Mulpex" (`from_app`), which `HUB_RULES` says not to reply to. Logged to
    /// `messages.log` like a peer's `hub_send`, so the hub panel shows it.
    /// Returns the inbox file, whose disappearing is how the caller learns the
    /// message was read (`hub_inbox` deletes what it returns).
    pub fn send_app_message(&self, to: usize, body: &str) -> Option<PathBuf> {
        let file = self.write_inbox(to, |ts| {
            serde_json::json!({ "from": 0, "from_app": true, "ts": ts, "body": body })
        })?;
        mulpex_core::mcp::log_message(
            &self.state_dir.join("messages.log"),
            mulpex_core::mcp::APP_SENDER,
            &format!("claude#{to}"),
            body,
        );
        Some(file)
    }

    fn write_inbox(
        &self,
        to: usize,
        msg: impl FnOnce(u64) -> serde_json::Value,
    ) -> Option<PathBuf> {
        let dir = self.state_dir.join("inbox").join(to.to_string());
        std::fs::create_dir_all(&dir).ok()?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let file = dir.join(format!("{}-{}.json", ts, mulpex_core::persist::new_uuid()));
        std::fs::write(&file, msg(ts).to_string()).ok()?;
        Some(file)
    }

    /// The claudes that can take a message right now: alive, and not a terminal,
    /// a ⌘S-stopped row or a failed start. ⌘⇧Q Checkpoint & Quit asks exactly
    /// these to stop.
    pub fn live_claude_ids(&self) -> Vec<usize> {
        self.sessions
            .iter()
            .filter(|s| {
                s.is_alive()
                    && !s.is_shell()
                    && !self.saving.contains(&s.id)
                    && !self.failed.contains_key(&s.id)
            })
            .map(|s| s.id)
            .collect()
    }

    /// `claude#id`'s transcript `.jsonl`, if it has one on disk yet.
    pub fn transcript_of(&self, id: usize) -> Option<PathBuf> {
        let s = self.sessions.iter().find(|s| s.id == id && !s.is_shell())?;
        if s.session_id.is_empty() {
            return None;
        }
        let p = crate::agent_transcript::path(s.kind.agent()?, &self.project_dir, &s.session_id, &self.state_dir, id)?;
        p.is_file().then_some(p)
    }

    /// The sidebar label for `claude#id`, if it has one.
    pub fn label_of(&self, id: usize) -> Option<String> {
        self.display_name(id)
    }

    /// Whether `claude#id`'s hub listener is running and has been for a moment.
    ///
    /// The listener only announces mail that arrives **after** it starts (it
    /// seeds its count from what is already there), so a message dropped into the
    /// inbox of a claude still booting would sit unannounced until its next turn.
    /// `armed/<id>` is touched every second while it runs; its creation time is
    /// when it started.
    pub fn listener_ready(&self, id: usize) -> bool {
        let Ok(meta) = std::fs::metadata(self.state_dir.join("armed").join(id.to_string())) else {
            return false;
        };
        let age = |t: std::io::Result<std::time::SystemTime>| {
            t.ok().and_then(|t| t.elapsed().ok())
        };
        let fresh = age(meta.modified()).is_some_and(|a| a < Duration::from_secs(30));
        let settled = age(meta.created()).is_none_or(|a| a >= Duration::from_secs(2));
        fresh && settled
    }

    /// Whether a message dropped into `id`'s inbox now will wake it: a claude's
    /// listener is up (`listener_ready`); a codex is woken by the app itself
    /// (`wake.rs`), so it is ready once its TUI is — it has a thread and working
    /// hooks (`agentready/<id>`), or has been up a few seconds (the typed
    /// fallback reaches a codex that has no thread yet).
    pub fn wake_ready(&self, id: usize) -> bool {
        match self.agent_of(id) {
            Some(Agent::Codex) => {
                mulpex_core::agentready_path(&self.state_dir, id).exists()
                    || self.started.get(&id).is_none_or(|t| t.elapsed() >= CODEX_UP)
            }
            _ => self.listener_ready(id),
        }
    }

    /// `listener_alive` per agent: something will wake `id` for mail at all —
    /// a claude's listener heartbeat is fresh; a codex's TUI is up, since the
    /// app rings it (`wake_ready`).
    pub fn wake_alive(&self, id: usize) -> bool {
        match self.agent_of(id) {
            Some(Agent::Codex) => self.wake_ready(id),
            _ => self.listener_alive(id),
        }
    }

    /// Ring the codex rows that have unread mail and sit idle (`wake.rs`).
    /// Returns the rows whose wake line the frontend must type (`codex-wake`).
    pub fn tick_wakes(&mut self) -> Vec<(usize, String)> {
        let codexes: Vec<(usize, String)> = self
            .live_instances()
            .filter(|s| s.kind == pty::SessionKind::Codex && s.is_alive())
            .map(|s| (s.id, s.session_id.clone()))
            .collect();
        self.wakes.retain(|id| codexes.iter().any(|(c, _)| *c == id));
        if codexes.is_empty() {
            return Vec::new();
        }
        let inputs: Vec<crate::wake::Input> = codexes
            .into_iter()
            .map(|(id, sid)| {
                let unread = std::fs::read_dir(self.state_dir.join("inbox").join(id.to_string()))
                    .map(|d| d.flatten().count())
                    .unwrap_or(0);
                let waiting = std::fs::read_to_string(self.state_dir.join(id.to_string()))
                    .map(|w| Status::from_word(w.trim()) == Some(Status::Waiting))
                    // No status file yet: a codex that has never had a prompt,
                    // idle at its first box.
                    .unwrap_or(true);
                let up = mulpex_core::agentready_path(&self.state_dir, id).exists()
                    || self.started.get(&id).is_none_or(|t| t.elapsed() >= CODEX_UP);
                crate::wake::Input { id, unread, waiting, thread: (!sid.is_empty() && up).then_some(sid) }
            })
            .collect();
        let bin = crate::claude_bin::resolve(Agent::Codex).cloned();
        let cwd = self.project_dir.clone();
        let queue = |thread: &str, line: &str| match &bin {
            Some(bin) => crate::wake::spawn_queue(bin.clone(), cwd.clone(), thread, line),
            None => {
                let (tx, rx) = std::sync::mpsc::channel();
                let _ = tx.send(Err("codex is not installed".to_string()));
                rx
            }
        };
        self.wakes.tick(&self.state_dir, &inputs, &queue)
    }

    /// Put back to `waiting` a codex whose turn ended without a `Stop` hook.
    ///
    /// Codex fires none on a turn that FAILED (a 429 that ran out of retries,
    /// measured) or was interrupted, so its status stayed `working` forever: the
    /// row looked busy and `tick_wakes` never rang it. Its rollout still records
    /// the end (`wake::rollout_turn_over`). Every `CODEX_SETTLE_EVERY`, a codex
    /// still `working` whose rollout says the turn is over and has been quiet
    /// for `CODEX_SETTLE_QUIET` — long past when a real `Stop` writes, so this
    /// never races one that writes `needs` or blocks for mail — is set
    /// `waiting`, and its per-turn locks are released as `Stop` would.
    pub fn settle_codex_turns(&mut self) {
        if self.codex_settled_at.is_some_and(|t| t.elapsed() < CODEX_SETTLE_EVERY) {
            return;
        }
        self.codex_settled_at = Some(Instant::now());
        for s in self.live_instances().filter(|s| s.kind == pty::SessionKind::Codex && s.is_alive()) {
            let status = self.state_dir.join(s.id.to_string());
            if !std::fs::read_to_string(&status).is_ok_and(|w| w.trim() == "working") || s.session_id.is_empty() {
                continue;
            }
            let Some(rollout) =
                crate::agent_transcript::path(Agent::Codex, &self.project_dir, &s.session_id, &self.state_dir, s.id)
            else {
                continue;
            };
            let quiet = std::fs::metadata(&rollout)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|a| a >= CODEX_SETTLE_QUIET);
            if !quiet || !read_tail(&rollout, CODEX_TAIL_BYTES).is_some_and(|t| crate::wake::rollout_turn_over(&t)) {
                continue;
            }
            eprintln!("[codex] codex#{}: its turn ended with no Stop hook (failed or interrupted) — now waiting", s.id);
            let _ = std::fs::write(&status, "waiting");
            if let Ok(entries) = std::fs::read_dir(self.state_dir.join("locks")) {
                for e in entries.flatten() {
                    if read_lock(&e.path()).is_some_and(|(holder, _)| holder == s.id) {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
    }

    /// The frontend typed (or refused to type) a `codex-wake` line.
    pub fn wake_typed(&mut self, id: usize, ok: bool, reason: &str) {
        self.wakes.typed(&self.state_dir, id, ok, reason);
    }

    /// Whether `claude#id`'s hub listener is running at all (its heartbeat is
    /// fresh), however recently it started — unlike `listener_ready`, which also
    /// waits out its first seconds.
    pub fn listener_alive(&self, id: usize) -> bool {
        std::fs::metadata(self.state_dir.join("armed").join(id.to_string()))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|a| a < Duration::from_secs(30))
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

    /// Adopt the uuid each instance's `claude` says it is **actually writing
    /// to** (`sessionid/<id>`, written by the hook from `transcript_path`), in
    /// place of the one Mulpex minted and assumed had stuck.
    ///
    /// Those two can diverge, and when they do the conversation is not lost —
    /// only unreachable. warweb#75 ran from 2026-09-14 to 2026-09-19 inside
    /// `c30f48b2-…jsonl` while Mulpex stored `7c1591ba-…`, the id it had spawned
    /// with; the next launch ran `--resume 7c1591ba`, Claude Code said
    /// *"No conversation found with session ID"*, and the row came back "failed
    /// to start" over a 64 MB transcript sitting intact on disk under the other
    /// name. Nothing in the old code could notice: the store only ever held what
    /// we asked for, never what we got.
    ///
    /// A uuid another row already holds is **refused**. Two rows resuming one
    /// transcript is the silent-corruption case `SessionStore::in_home` exists
    /// for, one layer up — and a refusal costs at worst one row that fails to
    /// restore, which is recoverable, against two claudes appending to one
    /// conversation, which is not. The refusal is logged rather than shown,
    /// because a guard that can silently decline to act has to say why.
    /// Clear `needs` from a claude whose question or plan was dismissed with Esc.
    ///
    /// `askq`/`plan` write `needs` when the dialog opens, and `PostToolUse` or
    /// `Stop` normally clears it — but an escaped dialog fires **no hook at all**
    /// (measured, claude 2.1.296, both dialogs). The row then stayed red until
    /// the next prompt typed on the Mac, and from the phone that was a deadlock:
    /// it kept drawing the question, and refused every message with "Answer the
    /// open question first". The transcript does record it — an `is_error`
    /// result for the dialog's tool call — so that is what's read here, only
    /// for claudes showing `needs` with a dialog on file.
    pub fn clear_escaped_dialogs(&self) {
        for s in self.instances() {
            let status = self.state_dir.join(s.id.to_string());
            if !std::fs::read_to_string(&status).is_ok_and(|t| t.trim() == "needs")
                || !mulpex_core::dialog_path(&self.state_dir, s.id).is_file()
            {
                continue;
            }
            let path = crate::saves::transcript_path(&self.project_dir, &s.session_id);
            let Some(tail) = read_tail(&path, DIALOG_TAIL_BYTES) else { continue };
            if let Some(word) = escaped_dialog(&tail) {
                eprintln!("[dialog] claude#{}: its dialog was dismissed (no hook fires for that) — {word}", s.id);
                let _ = std::fs::write(&status, word);
            }
        }
    }

    pub fn reconcile_session_ids(&mut self) {
        let mut changed = false;
        let reports: Vec<(usize, String)> = self
            .instances()
            .filter_map(|s| {
                let uuid = std::fs::read_to_string(mulpex_core::session_id_path(
                    &self.state_dir,
                    s.id,
                ))
                .ok()?;
                let uuid = uuid.trim().to_string();
                (!uuid.is_empty() && uuid != s.session_id).then_some((s.id, uuid))
            })
            .collect();
        for (id, uuid) in reports {
            // Held by another live row, or by a failed restore we are still
            // keeping the record of — either way it is not ours to take.
            let taken = self
                .sessions
                .iter()
                .any(|s| s.id != id && s.session_id == uuid)
                || self.sticky.iter().any(|(_, s)| s.session_id == uuid);
            let Some(session) = self.sessions.iter_mut().find(|s| s.id == id) else {
                continue;
            };
            if taken {
                eprintln!(
                    "[sessionid] {}#{id}: refusing {uuid} — another row already resumes it; \
                     keeping {}",
                    session.kind.word(),
                    session.session_id
                );
                continue;
            }
            eprintln!(
                "[sessionid] {}#{id}: transcript is {uuid}, not {:?} — store follows the file",
                session.kind.word(),
                session.session_id
            );
            session.session_id = uuid;
            // The daemon outlives this app; the next one recognises the
            // session by what it holds.
            session.publish_meta();
            changed = true;
        }
        if changed {
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
                // A watcher leaves the status `waiting` on purpose, so this is the
                // only thing that can tell the updater not to restart the app out
                // from under a live agentalk channel.
                let watching = mulpex_core::watching_path(&self.state_dir, s.id).exists();
                let background_only = status == Status::Working
                    && mulpex_core::only_background_work(&self.state_dir, s.id);
                let ctx_pct = std::fs::read_to_string(mulpex_core::ctx_path(&self.state_dir, s.id))
                    .ok()
                    .and_then(|t| t.trim().parse::<f64>().ok())
                    .map(|p| p.round().clamp(0.0, 100.0) as u8);
                let compact_phase =
                    std::fs::read_to_string(mulpex_core::compactreq_path(&self.state_dir, s.id))
                        .ok()
                        .map(|w| w.trim().to_owned())
                        .filter(|w| !w.is_empty());
                let compacting = mulpex_core::compacting_path(&self.state_dir, s.id).exists();
                StatusEntry {
                    id: s.id,
                    status,
                    watching,
                    background_only,
                    ctx_pct,
                    compact_phase,
                    compacting,
                    wake: self.wakes.mark(s.id),
                }
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
        let mut queued: Vec<(u64, String)> = Vec::new();
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
                let files: Vec<PathBuf> = std::fs::read_dir(entry.path())
                    .map(|d| d.flatten().map(|f| f.path()).collect())
                    .unwrap_or_default();
                let count = files.len();
                for f in &files {
                    if let Some(m) = read_queued(f) {
                        queued.push(m);
                    }
                }
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

        let mut messages = read_messages(&self.state_dir.join("messages.log"), MSG_FEED_MAX);
        mark_unread(&mut messages, queued);

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
                name_by: self.name_source(s.id).map(|b| b.as_str().to_string()),
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
        self.instances()
            .filter(|s| !self.failed.contains_key(&s.id) && !self.saving.contains(&s.id))
    }

    /// Persist the worked-on sessions' ids (+ names + mute), preserving order.
    /// Terminals are deliberately not persisted (they'd restore as a fresh shell
    /// with no scrollback and possibly re-run a destructive seeded command), and
    /// the filter is on `kind` rather than on an empty `session_id` so that
    /// intent is explicit rather than an accident of `persist`'s own guard.
    fn persist_sessions(&self) {
        let mut sessions: Vec<persist::SavedSession> = self
            .instances()
            // A codex has no id until its first prompt (Codex mints it then,
            // docs/codex.md) — nothing to resume yet, so nothing to keep.
            .filter(|s| self.worked.contains(&s.id) && !s.session_id.is_empty())
            .map(|s| persist::SavedSession {
                session_id: s.session_id.clone(),
                name: self.names.get(&s.id).cloned(),
                muted: self.muted.contains(&s.id),
                // The number the user knows this instance by, so the next launch
                // brings it back as the same `claude#N`.
                id: Some(s.id),
                parent: self.parents.get(&s.id).copied(),
                collapsed: self.collapsed.contains(&s.id),
                auto_compact: self.auto_compact.get(&s.id).copied(),
                todos: false,
                name_by: self.name_by.get(&s.id).copied(),
                agent: s.kind.agent().unwrap_or_default(),
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
        // A pin lives as long as its row in the store (`pins.rs`).
        let saved: HashSet<usize> = sessions.iter().filter_map(|s| s.id).collect();
        self.pin_store().retain(&saved);
    }

    /// This project's ⌘⇧P pins, stored beside its session store.
    pub fn pin_store(&self) -> crate::pins::PinStore {
        crate::pins::PinStore::beside(self.store.path(), &self.project_dir)
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
        if out != self.terminal_index {
            let _ = std::fs::write(self.state_dir.join("terminals").join("index"), &out);
            self.terminal_index = out;
        }
        self.sync_terminal_meta();
    }

    /// Publish what each live terminal is *doing* — is a foreground command
    /// running, and the shell's cwd — for the MCP helper, which is a separate
    /// process and can only learn this from disk.
    ///
    /// One JSON file rather than a column on the index line: that line is parsed
    /// `splitn(3, '\t')` with the label last, so a fourth field would be read as
    /// part of the label, and a path is exactly the kind of value that would
    /// smuggle a separator in. A terminal whose state cannot be read is simply
    /// absent — the reader must be able to tell "not known" from "no".
    fn sync_terminal_meta(&mut self) {
        let mut map = serde_json::Map::new();
        for s in self.sessions.iter().filter(|s| s.is_shell()) {
            let Some(state) = s.shell_state() else { continue };
            let mut entry = serde_json::Map::new();
            entry.insert("command_running".into(), serde_json::json!(state.command_running));
            if let Some(cwd) = state.cwd {
                entry.insert("cwd".into(), serde_json::json!(cwd));
            }
            map.insert(s.id.to_string(), serde_json::Value::Object(entry));
        }
        let out = serde_json::Value::Object(map).to_string();
        if out == self.terminal_meta {
            return;
        }
        let _ = std::fs::write(self.state_dir.join("terminals").join("meta"), &out);
        self.terminal_meta = out;
    }

    /// Publish every row as the sidebar shows it to `ADOPT_MANIFEST`, for the
    /// app after a restart to adopt the daemon's sessions with (`Core::open`).
    /// The store cannot do this: it holds only worked claudes, and a restart
    /// must bring back every row — a fresh claude, a terminal — as it was.
    /// Called from the poll loop; writes only on change.
    pub fn sync_adopt_manifest(&mut self) {
        let rows: Vec<serde_json::Value> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| {
                serde_json::json!({
                    "id": s.id,
                    "active": i == self.active,
                    "session_id": s.session_id,
                    "worked": self.worked.contains(&s.id),
                    "name": self.names.get(&s.id),
                    "name_by": self.name_by.get(&s.id).map(|b| b.as_str()),
                    "muted": self.muted.contains(&s.id),
                    "parent": self.parents.get(&s.id),
                    "collapsed": self.collapsed.contains(&s.id),
                    "auto_compact": self.auto_compact.get(&s.id),
                })
            })
            .collect();
        let out = serde_json::Value::Array(rows).to_string();
        if out == self.adopt_manifest {
            return;
        }
        let _ = std::fs::write(self.state_dir.join(ADOPT_MANIFEST), &out);
        self.adopt_manifest = out;
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

/// Every row of a project as the last app showed it, in the scratch dir —
/// which outlives the app, being the daemon's (`Core::sync_adopt_manifest`).
const ADOPT_MANIFEST: &str = "ptyd-rows.json";

/// One row of `ADOPT_MANIFEST`.
#[derive(Debug, Default, serde::Deserialize)]
struct AdoptRow {
    id: usize,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    worked: bool,
    name: Option<String>,
    name_by: Option<String>,
    #[serde(default)]
    muted: bool,
    parent: Option<usize>,
    #[serde(default)]
    collapsed: bool,
    auto_compact: Option<u8>,
    #[serde(default)]
    todos: bool,
    #[serde(default)]
    active: bool,
}

fn read_adopt_manifest(state_dir: &Path) -> Vec<AdoptRow> {
    std::fs::read_to_string(state_dir.join(ADOPT_MANIFEST))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// The thresholds the context menu offers for auto-compact.
pub const AUTO_COMPACT_CHOICES: [u8; 4] = [50, 60, 70, 80];

/// What the orchestrator is told when the user turns auto-compact on or off.
fn auto_compact_message(pct: Option<u8>) -> String {
    match pct {
        Some(p) => format!(
            "The user enabled auto-compact for you and your claudes at {p}% context. When you \
             or one of them reaches it, Mulpex asks that claude to stop at a checkpoint, runs \
             /compact, and tells it to continue. A child may pause briefly while that happens; \
             don't treat it as stuck. You don't need to do anything."
        ),
        None => "The user disabled auto-compact for you and your claudes.".to_string(),
    }
}

/// The conversation id a brand-new row starts with: a claude takes the one
/// Mulpex mints (`--session-id`); a codex has none until Codex mints it at the
/// first prompt, which the hook reports to `sessionid/<id>` and
/// `reconcile_session_ids` adopts (docs/codex.md S10).
fn fresh_session_id(agent: Agent) -> String {
    match agent {
        Agent::Claude => persist::new_uuid(),
        Agent::Codex => String::new(),
    }
}

fn kind_of(s: &Session) -> SessionKind {
    match s.kind {
        pty::SessionKind::Claude => SessionKind::Claude,
        pty::SessionKind::Codex => SessionKind::Codex,
        pty::SessionKind::Shell => SessionKind::Shell,
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
            unread: false,
        });
    }
    out
}

/// One queued inbox file as `(ts, body)`, the two fields it shares with its log
/// line. `None` for a file mid-write or not ours.
fn read_queued(path: &Path) -> Option<(u64, String)> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some((v.get("ts")?.as_u64()?, v.get("body")?.as_str()?.to_string()))
}

/// How far apart a log line's `ts` and its inbox file's may be. `hub_send` stamps
/// each with its own `now()`, inbox first, so they can straddle a second.
const UNREAD_TS_SLACK: u64 = 5;

/// Mark which log records are still waiting in some live inbox, so the user can
/// see *which* message an instance is sitting on and nag it.
///
/// The log line and the inbox file carry no shared id, so this matches on body
/// and a timestamp within `UNREAD_TS_SLACK`. Each inbox file accounts for one
/// directed message; a broadcast (`to: all`) is one log line but a file per
/// recipient, so it takes every matching file and stays unread while any
/// recipient still holds it. A list send (`to: "claude#3, claude#5"`) is the
/// same shape.
fn mark_unread(messages: &mut [MsgEntry], mut queued: Vec<(u64, String)>) {
    if queued.is_empty() {
        return;
    }
    let near = |m: &MsgEntry, q: &(u64, String)| q.1 == m.body && q.0.abs_diff(m.ts) <= UNREAD_TS_SLACK;
    for m in messages.iter_mut() {
        if m.to == "all" || m.to.contains(',') {
            let before = queued.len();
            queued.retain(|q| !near(m, q));
            m.unread = queued.len() < before;
        } else if let Some(i) = queued.iter().position(|q| near(m, q)) {
            queued.swap_remove(i);
            m.unread = true;
        }
    }
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

/// How much of a transcript's end `clear_escaped_dialogs` reads: the dialog
/// call and what followed it are the last few entries.
const DIALOG_TAIL_BYTES: u64 = 256 * 1024;

/// The last `max` bytes of `path`, from its first whole line.
fn read_tail(path: &Path, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let size = f.metadata().ok()?.len();
    let start = size.saturating_sub(max);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let from = if start > 0 { buf.iter().position(|&b| b == b'\n').map_or(buf.len(), |i| i + 1) } else { 0 };
    Some(String::from_utf8_lossy(&buf[from..]).into_owned())
}

/// What a claude's status should be if the LAST question/plan in this
/// transcript tail was dismissed without an answer, or `None` while it's open
/// or was answered (an answer fires `PostToolUse`, which clears it).
///
/// Dismissed = its tool call got an `is_error` result (measured shape, both
/// AskUserQuestion and ExitPlanMode). What followed says where the turn went:
/// - `[Request interrupted by user for tool use]` → Esc ended the turn: `waiting`;
/// - an assistant entry → the plan was rejected with feedback and claude went
///   on working (its tool calls can't clear a `needs`): `working`;
/// - nothing yet → wait for the next line rather than guess.
fn escaped_dialog(tail: &str) -> Option<&'static str> {
    let blocks = |e: &serde_json::Value| -> Vec<serde_json::Value> {
        e.pointer("/message/content").and_then(|c| c.as_array()).cloned().unwrap_or_default()
    };
    // The last dialog call. Lines are tested as text first and parsed only when
    // they could be one: this runs every tick for a claude sitting on a
    // question. The name alone isn't enough — a `prompt_snapshot` entry (the
    // system prompt, tool list included) can come after the call.
    let lines: Vec<&str> = tail.lines().collect();
    let (from, id) = lines.iter().enumerate().rev().find_map(|(i, l)| {
        if !l.contains("\"tool_use\"") || !(l.contains("\"AskUserQuestion\"") || l.contains("\"ExitPlanMode\"")) {
            return None;
        }
        let e: serde_json::Value = serde_json::from_str(l).ok()?;
        (e.get("type")?.as_str()? == "assistant")
            .then(|| blocks(&e))?
            .iter()
            .rev()
            .find(|b| {
                b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                    && matches!(b.get("name").and_then(|n| n.as_str()), Some("AskUserQuestion" | "ExitPlanMode"))
            })
            .and_then(|b| b.get("id")?.as_str().map(|id| (i, id.to_string())))
    })?;
    let mut rejected = false;
    for e in lines[from + 1..].iter().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()) {
        let kind = e.get("type").and_then(|t| t.as_str());
        if kind == Some("user") {
            for b in blocks(&e) {
                if b.get("tool_use_id").and_then(|v| v.as_str()) == Some(id.as_str()) {
                    if b.get("is_error").and_then(|v| v.as_bool()) != Some(true) {
                        return None; // answered
                    }
                    rejected = true;
                }
                if rejected && b.get("text").and_then(|t| t.as_str()).is_some_and(|t| t.starts_with("[Request interrupted"))
                {
                    return Some("waiting");
                }
            }
        } else if rejected && kind == Some("assistant") {
            return Some("working");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The transcript after a dialog, in the shapes measured on claude 2.1.296.
    fn dialog_tail(tool: &str, after: &[serde_json::Value]) -> String {
        let mut lines = vec![
            serde_json::json!({"type":"user","message":{"content":"ask me"}}),
            serde_json::json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":tool,"input":{}}]}}),
            serde_json::json!({"type":"attachment","attachment":{"type":"prompt_snapshot"}}),
        ];
        lines.extend(after.iter().cloned());
        lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n") + "\n"
    }

    #[test]
    fn an_escaped_dialog_is_read_from_the_transcript() {
        use serde_json::json;
        let rejected = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,
            "content":"The user doesn't want to proceed with this tool use. The tool use was rejected"}]}});
        let interrupted = json!({"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}});
        let answered = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"User has answered"}]}});
        let went_on = json!({"type":"assistant","message":{"content":[{"type":"text","text":"Changing the plan."}]}});
        for tool in ["AskUserQuestion", "ExitPlanMode"] {
            assert_eq!(escaped_dialog(&dialog_tail(tool, &[])), None, "{tool}: still open");
            assert_eq!(escaped_dialog(&dialog_tail(tool, &[rejected.clone()])), None, "{tool}: wait for what follows");
            assert_eq!(escaped_dialog(&dialog_tail(tool, &[rejected.clone(), interrupted.clone()])), Some("waiting"), "{tool}: Esc");
            assert_eq!(escaped_dialog(&dialog_tail(tool, &[rejected.clone(), went_on.clone()])), Some("working"), "{tool}: feedback");
            assert_eq!(escaped_dialog(&dialog_tail(tool, &[answered.clone(), went_on.clone()])), None, "{tool}: answered");
        }
        // A newer dialog after an escaped one is the one that counts.
        let mut two = dialog_tail("AskUserQuestion", &[rejected.clone(), interrupted.clone()]);
        two.push_str(&json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"AskUserQuestion","input":{}}]}}).to_string());
        two.push('\n');
        assert_eq!(escaped_dialog(&two), None, "the new question is open");
        assert_eq!(escaped_dialog("not json\n"), None);
    }

    /// Every real transcript on this machine whose LAST dialog was dismissed:
    /// how `escaped_dialog` reads its end. Reports, never fails — run with
    /// `--ignored --nocapture` after Claude Code changes its transcript shape.
    #[test]
    #[ignore]
    fn escaped_dialogs_in_real_transcripts() {
        let root = crate::saves::transcript_path(Path::new("/"), "x");
        let root = root.parent().and_then(Path::parent).unwrap().to_path_buf();
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for d in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                let Some(tail) = read_tail(&f.path(), DIALOG_TAIL_BYTES) else { continue };
                let got = escaped_dialog(&tail).unwrap_or("none");
                *seen.entry(got).or_default() += 1;
                if got != "none" && f.path().to_string_lossy().contains("probe") {
                    eprintln!("{got}: {}", f.path().display());
                }
            }
        }
        eprintln!("escaped-dialog reading of real transcripts: {seen:?}");
    }

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

    /// A claude's `hub_board` `columns` call changes nothing until the user's
    /// Approve; then `board_on` (what every claude reads per turn) follows.
    #[test]
    fn a_column_proposal_goes_through_the_user() {
        let _env = env_guard();
        let root = std::env::temp_dir().join(format!("mulpex-board-prop-{}", persist::new_uuid()));
        let project_dir = root.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::env::set_var("HOME", root.join("home"));
        std::env::set_var("MULPEX_HOME", root.join("home/.mulpex"));
        let mut core = Core::open(1, project_dir, Path::new("/nonexistent/mulpex-helper"), root.join("state"), TEST_GEOMETRY).unwrap();
        core.set_board_on(true);
        let r = core.apply_board_request(&serde_json::json!({
            "op": "board", "from": 3, "action": "columns", "why": "QA",
            "changes": [{"op": "add", "name": "Waiting for QA", "desc": "I test", "before": "Done"}],
        }));
        assert_eq!(r["ok"], true, "{r}");
        assert!(r["note"].as_str().unwrap().contains("P1"));
        let flag_file = core.state_dir.join(mulpex_core::BOARD_ON_FILE);
        let flag = || std::fs::read_to_string(&flag_file).unwrap();
        assert!(!flag().contains("Waiting for QA"));
        assert_eq!(core.board_view()["proposals"].as_array().unwrap().len(), 1);
        let edit: crate::board::BoardEdit = serde_json::from_value(serde_json::json!({"op": "approve", "proposal": 1})).unwrap();
        let v = core.board_edit(edit).unwrap();
        assert!(v["proposals"].as_array().unwrap().is_empty());
        assert!(flag().contains("3. \"Waiting for QA\" — I test"), "{}", flag());
        let edit: crate::board::BoardEdit = serde_json::from_value(serde_json::json!({"op": "reject", "proposal": 1, "reason": "x"})).unwrap();
        assert!(core.board_edit(edit).is_err(), "decided once");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// macOS purges `$TMPDIR` files it has not seen touched in 3 days, and
    /// `settings.json` / `mcp.json` were the only write-once files in the scratch
    /// tree — so a Mulpex left open over a long weekend lost exactly the two files
    /// `claude` is spawned with, and every new ⌘T died with
    /// `Error: Settings file not found`. Every spawn now re-lays the dir first.
    #[test]
    fn a_purged_scratch_dir_is_rebuilt_before_a_spawn() {
        let _env = env_guard();
        let root = std::env::temp_dir().join(format!("mulpex-open-test-{}", persist::new_uuid()));
        let project_dir = root.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::env::set_var("HOME", root.join("home"));

        let helper = Path::new("/nonexistent/mulpex-helper");
        let state_dir = root.join("state");
        let core = Core::open(1, project_dir, helper, state_dir.clone(), TEST_GEOMETRY).unwrap();

        // What dirhelper did: delete the untouched files, and a subdir with them.
        std::fs::remove_file(state_dir.join("settings.json")).unwrap();
        std::fs::remove_file(state_dir.join("mcp.json")).unwrap();
        std::fs::remove_dir_all(state_dir.join("inbox")).unwrap();
        assert!(!core.settings_path.exists());

        core.ensure_state_dir().expect("repair should succeed");

        let settings = std::fs::read_to_string(&core.settings_path)
            .expect("settings.json must be back — `claude --settings` reads it at spawn");
        let mcp = std::fs::read_to_string(state_dir.join("mcp.json")).unwrap();
        assert!(state_dir.join("inbox").is_dir(), "hub subdirs must be back");
        // Rebuilt from the templates, not left with a stale `__MULPEX_BIN__`.
        assert!(settings.contains(&helper.display().to_string()));
        assert!(mcp.contains(&helper.display().to_string()));
        assert!(!settings.contains("__MULPEX_BIN__"));
        assert!(!mcp.contains("__MULPEX_BIN__"));

        // The whole tree gone (not just its files) is the same repair.
        std::fs::remove_dir_all(&state_dir).unwrap();
        core.ensure_state_dir().expect("repair should recreate the root too");
        assert!(core.settings_path.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The log carries addresses now, not ids, so an entry can name the project a
    /// message came from. A bare-number `from` would have nowhere to put it.
    /// The panel marks the messages still sitting in an inbox. Same body twice
    /// with one read must mark one; a broadcast stays unread while any recipient
    /// holds it; a body match far off in time is a different message.
    #[test]
    fn unread_is_matched_by_body_and_time() {
        let msg = |to: &str, body: &str, ts: u64| MsgEntry {
            from: "claude#1".into(),
            to: to.into(),
            body: body.into(),
            ts,
            unread: false,
        };
        // Newest first, as `read_messages` returns them.
        let mut log = vec![
            msg("claude#3, claude#6", "heads up", 400),
            msg("claude#6", "heads up", 400),
            msg("all", "ship it", 300),
            msg("claude#4", "status?", 200),
            msg("claude#4", "status?", 100),
            msg("claude#5", "old", 50),
        ];
        let queued = vec![
            // Both list recipients still hold it: the list line takes both
            // files, so the lone direct line of the same body stays read.
            (400, "heads up".to_string()),
            (400, "heads up".to_string()),
            (299, "ship it".to_string()),
            (300, "ship it".to_string()),
            (199, "status?".to_string()),
            (900, "old".to_string()),
        ];
        mark_unread(&mut log, queued);
        let marks: Vec<bool> = log.iter().map(|m| m.unread).collect();
        assert_eq!(marks, vec![true, false, true, true, false, false]);
    }

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
                        name_by: None,
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

    /// `hub_board` is refused while the board is off; on, a claude's call lands
    /// on the project's board, and a change for a claude that is not running is
    /// dropped from the outbox without touching the board.
    #[test]
    fn board_requests_need_the_board_on() {
        let (_g, root, mut core) = scratch_core("board");
        let req = |core: &mut Core, body: serde_json::Value| core.apply_terminal_request(&body.to_string()).0;

        let r = req(&mut core, serde_json::json!({
            "op": "board", "from": 3, "action": "add", "items": [{"title": "x"}],
        }));
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("right-click"), "{r}");
        assert!(core.board.get().cards.is_empty());
        assert!(!core.state_dir.join(mulpex_core::BOARD_ON_FILE).exists());

        core.set_board_on(true);
        let flag = std::fs::read_to_string(core.state_dir.join(mulpex_core::BOARD_ON_FILE)).unwrap();
        assert!(flag.contains("\"To Do\""), "{flag}");
        let r = req(&mut core, serde_json::json!({
            "op": "board", "from": 3, "action": "add", "items": [{"title": "which port?", "assignee": 4}],
        }));
        assert_eq!(r["ok"], true, "{r}");
        assert!(r["board"]["cards"].as_array().unwrap().is_empty(), "claude#3 sees only its own cards: {r}");
        let r = req(&mut core, serde_json::json!({"op": "board", "from": 4, "action": "list"}));
        assert_eq!(r["board"]["cards"][0]["title"], "which port?", "{r}");
        let r = req(&mut core, serde_json::json!({"op": "board", "from": 3, "action": "bogus"}));
        assert_eq!(r["ok"], false);
        let r = req(&mut core, serde_json::json!({"op": "todos", "from": 3, "action": "list"}));
        assert!(r["error"].as_str().unwrap().contains("hub_board"), "{r}");

        // Renaming a column reaches the hook's file at once.
        core.board_edit(crate::board::BoardEdit::ColEdit { col: 1, name: Some("Backlog".into()), desc: None }).unwrap();
        let flag = std::fs::read_to_string(core.state_dir.join(mulpex_core::BOARD_ON_FILE)).unwrap();
        assert!(flag.contains("Backlog"), "{flag}");

        // The user's comment waits for UPDATE; claude#4 has no row here, so
        // UPDATE refuses it and the drain forgets the queue — the board stays.
        core.board_edit(crate::board::BoardEdit::Comment { n: 1, text: "5173".into(), images: vec![] }).unwrap();
        assert_eq!(core.board_view()["pending"]["4"][0], 1);
        let r = core.board_update(None);
        assert!(r["sent"].as_array().unwrap().is_empty() && r["refused"][0].as_str().unwrap().contains("claude#4"), "{r}");
        core.drain_board_outbox();
        assert!(core.board.with_outbox(|o| o.is_empty()));
        assert!(!core.state_dir.join("inbox/4").exists(), "nothing was sent");
        assert_eq!(core.board.get().cards[0].comments.len(), 1);

        core.set_board_on(false);
        assert!(!core.state_dir.join(mulpex_core::BOARD_ON_FILE).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// END-TO-END: the task a spawner hands `hub_spawn` must reach the child's
    /// process WHOLE, through the real production path — `spawn_instance_with_task_as`
    /// -> `Session::spawn` -> `CommandBuilder` -> `exec`.
    ///
    /// This is the regression that ate two instances' briefs. The task used to be
    /// TYPED into the child's TUI, and `claude` caps a fast burst at one tty read:
    /// measured against v2.1.252 by reading the child's own transcript, sending
    /// 1200 / 1998 / 3000 / 6000 characters produced 1022 received EVERY time,
    /// cutting mid-word — while `hub_spawn` answered `ok: true`, because a turn had
    /// genuinely started, just on the first kilobyte of the brief. A unit test on
    /// the prompt builder cannot catch a return to typing; only reading the child's
    /// real argv can.
    ///
    /// `#[ignore]`d because it has to own the process: `claude_bin`'s `merged_path`
    /// and `resolve_claude` are `OnceLock`s, so the stub must be resolved before
    /// any other test resolves the real binary. Run it alone:
    ///   cargo test --lib -- --ignored --exact \
    ///     state::tests::a_spawned_child_receives_its_whole_task_on_its_command_line
    #[test]
    #[ignore]
    fn a_spawned_child_receives_its_whole_task_on_its_command_line() {
        let _env = env_guard();
        let root = std::env::temp_dir().join(format!("mulpex-argv-{}", persist::new_uuid()));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let argv_out = root.join("argv.txt");

        // A stub `claude` that records its argv NUL-separated and then stays up,
        // so the session looks alive exactly as the real one would.
        let stub = bin.join("claude");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done > {}\nsleep 30\n",
                argv_out.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // The stub must win the PATH search. `merged_path` puts the LOGIN shell's
        // PATH ahead of ours, so point SHELL at something that cannot answer.
        std::env::set_var("SHELL", "/usr/bin/false");
        std::env::set_var(
            "PATH",
            format!("{}:/usr/bin:/bin", bin.display()),
        );

        let (_root, mut core) = scratch_core_inner("argvspawn");

        // Comfortably past the 1022-character cap that used to eat it.
        let task: String = (0..1000).map(|i| format!("{i:05} ")).collect();
        assert_eq!(task.len(), 6000);
        core.spawn_instance_with_task_as(2, task.clone(), None).unwrap();

        assert!(
            wait_until(|| argv_out.exists()),
            "the stub claude never ran — PATH injection failed, so this test proved nothing"
        );
        // Give the redirect a moment to land in full before reading it.
        assert!(wait_until(|| std::fs::read(&argv_out).map(|b| b.len()) .unwrap_or(0) > 6000));
        let raw = std::fs::read_to_string(&argv_out).unwrap();
        let args: Vec<&str> = raw.split('\0').filter(|a| !a.is_empty()).collect();

        let prompt = args
            .last()
            .expect("claude was given no arguments at all");
        let collapsed = task.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            prompt.contains(&collapsed),
            "the task did not reach the child's command line whole: got {} chars, \
             wanted the full {} — head {:?}, tail {:?}",
            prompt.len(),
            collapsed.len(),
            &prompt[..40.min(prompt.len())],
            &prompt[prompt.len().saturating_sub(40)..],
        );
        assert!(
            prompt.starts_with("[mulpex:hub]"),
            "the sentinel the UserPromptSubmit hook keys off must lead the prompt"
        );
        // The old path is gone: nothing may wait for a TUI and type this in.
        assert!(
            args.iter().any(|a| *a == "--session-id"),
            "a spawned child must still get its own session id for --resume"
        );

        let _ = std::fs::remove_dir_all(&root);
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

    /// The sidebar tree survives a restart, and a link to a row that did not
    /// come back is dropped rather than kept pointing at nothing.
    #[test]
    fn parent_links_are_restored_and_dangling_ones_dropped() {
        let saved = [
            persist::SavedSession { collapsed: true, ..record("root", Some(3)) },
            persist::SavedSession { parent: Some(3), ..record("child", Some(7)) },
            persist::SavedSession { parent: Some(7), ..record("grandchild", Some(9)) },
            persist::SavedSession { parent: Some(42), ..record("orphan", Some(11)) },
        ];
        let (_env, root, mut core) = core_from_store("parents", &saved);
        assert_eq!(core.parents.get(&7), Some(&3));
        assert_eq!(core.parents.get(&9), Some(&7));
        assert_eq!(core.parents.get(&11), None, "a parent that is not here was kept");
        assert!(core.collapsed.contains(&3), "the folded family came back unfolded");

        // …and it is written back the same way.
        core.persist_sessions();
        let back = core.store.load();
        let parent_of = |id: usize| back.iter().find(|s| s.id == Some(id)).unwrap().parent;
        assert_eq!(parent_of(7), Some(3));
        assert_eq!(parent_of(9), Some(7));
        assert_eq!(parent_of(11), None);
        assert!(back.iter().find(|s| s.id == Some(3)).unwrap().collapsed);

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    fn record(name: &str, id: Option<usize>) -> persist::SavedSession {
        persist::SavedSession {
            session_id: persist::new_uuid(),
            name: Some(name.into()),
            muted: false,
            id,
            parent: None,
            collapsed: false,
            auto_compact: None,
            name_by: None,
            todos: false,
            agent: Default::default(),
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

    /// The store must follow the transcript, not the uuid we minted.
    ///
    /// This is warweb#75, reproduced from the store outward. Mulpex spawned it
    /// with `7c1591ba-…`; the conversation ended up in
    /// `c30f48b2-…jsonl` and stayed there for five days. Because the store only
    /// ever held what Mulpex *asked for*, the next launch resumed a uuid Claude
    /// Code had never created, and a 64 MB transcript became unreachable while
    /// sitting right there on disk.
    #[test]
    fn the_store_follows_the_transcript_the_hook_reports() {
        let minted = "7c1591ba-fb45-4b07-b044-03a7b2527742";
        let real = "c30f48b2-ac30-4fad-8d29-4cf92cd5a7b9";
        let saved = [persist::SavedSession {
            session_id: minted.into(),
            name: Some("WC2 spells phase 2".into()),
            muted: false,
            id: Some(75),
            parent: None,
            collapsed: false,
            auto_compact: None,
            name_by: None,
            todos: false,
            agent: Default::default(),
        }];
        let (_env, root, mut core) = core_from_store("sidfollow", &saved);

        // Nothing reported yet: the minted id stands.
        core.reconcile_session_ids();
        assert_eq!(core.sessions[0].session_id, minted);

        let sid = mulpex_core::session_id_path(&core.state_dir, 75);
        std::fs::create_dir_all(sid.parent().unwrap()).unwrap();
        std::fs::write(&sid, format!("{real}\n")).unwrap();
        core.reconcile_session_ids();

        assert_eq!(core.sessions[0].session_id, real, "the row kept a uuid with no transcript");
        assert_eq!(
            core.store.load()[0].session_id,
            real,
            "the next launch would still --resume a file that does not exist"
        );
        assert_eq!(
            core.display_name(75).as_deref(),
            Some("WC2 spells phase 2"),
            "adopting the real uuid must not disturb the row's identity"
        );

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two rows may never name one transcript.
    ///
    /// `/resume`-ing the same conversation in a second instance would otherwise
    /// point both store lines at one file, and the launch after that hands two
    /// live claudes the same `--resume` — the silent conversation corruption
    /// `SessionStore::in_home` guards against one layer down. The first row
    /// keeps it; the second keeps its own (recoverable) id.
    #[test]
    fn a_transcript_another_row_already_resumes_is_refused() {
        let shared = "c30f48b2-ac30-4fad-8d29-4cf92cd5a7b9";
        let saved = [
            persist::SavedSession {
                session_id: shared.into(),
                name: Some("holder".into()),
                muted: false,
                id: Some(1),
                parent: None,
                collapsed: false,
                auto_compact: None,
                name_by: None,
                todos: false,
                agent: Default::default(),
            },
            record("copycat", Some(2)),
        ];
        let (_env, root, mut core) = core_from_store("siddup", &saved);
        let mine = core.sessions[1].session_id.clone();

        let sid = mulpex_core::session_id_path(&core.state_dir, 2);
        std::fs::create_dir_all(sid.parent().unwrap()).unwrap();
        std::fs::write(&sid, shared).unwrap();
        core.reconcile_session_ids();

        assert_eq!(core.sessions[1].session_id, mine, "two rows now --resume one transcript");
        assert_eq!(core.sessions[0].session_id, shared, "the holder lost its own conversation");

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ⌘⇧R on a claude nobody has prompted starts it fresh.
    ///
    /// It has no transcript on disk, so `--resume` would exit within seconds and
    /// the row would come back dead. Instead it gets a new uuid and a plain
    /// start, on the same row — and a fresh spawn carries this build's spawn
    /// fingerprint, which is how the ↻ badge clears on it.
    #[test]
    fn restarting_an_unprompted_instance_starts_it_fresh_on_the_same_row() {
        let saved = [record("nothing to resume", Some(1))];
        let (_env, root, mut core) = core_from_store("restartguard", &saved);
        let id = core.sessions[0].id;
        let sid = core.sessions[0].session_id.clone();
        // The restore path seeds `worked`; a claude nobody has prompted yet is
        // exactly the case this covers, so put it back in that state.
        core.worked.remove(&id);
        let stale = mulpex_core::session_id_path(&core.state_dir, id);
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, &sid).unwrap();

        core.restart_instance(id).expect("the fresh restart failed");

        assert_eq!(core.sessions.len(), 1, "the restart changed the row count");
        assert_eq!(core.sessions[0].id, id, "the instance was renumbered");
        assert_ne!(core.sessions[0].session_id, sid, "it reused a uuid with no transcript");
        assert!(core.sessions[0].is_alive(), "nothing was spawned in its place");
        assert!(!core.restored.contains_key(&id), "a fresh start read as a restore");
        assert!(!stale.exists(), "the old process's uuid report would be adopted back");
        assert!(!core.session_infos()[0].outdated, "a fresh spawn is not outdated");

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A restart replaces the *process* and nothing else.
    ///
    /// Same position in the sidebar, same number (so every `claude#N` an
    /// instance or the user wrote down still points here), same conversation,
    /// same name — and the state the dead child left on disk cleared, because a
    /// status file and an `armed` flag are both assertions about a process that
    /// no longer exists.
    #[test]
    fn restarting_an_instance_keeps_its_row_and_clears_the_dead_childs_state() {
        let saved = [record("one", Some(1)), record("two", Some(2)), record("three", Some(3))];
        let (_env, root, mut core) = core_from_store("restartinplace", &saved);
        let id = core.sessions[1].id;
        let sid = core.sessions[1].session_id.clone();
        // What a live instance leaves behind: the status the sidebar, the dock
        // badge and every peer read, and the flag tracking its hub listener.
        std::fs::write(core.state_dir.join(id.to_string()), "needs").unwrap();
        std::fs::write(core.state_dir.join("armed").join(id.to_string()), "").unwrap();
        // A row that had failed to start must come back as a live peer.
        core.failed.insert(id, "an earlier failure".into());

        core.restart_instance(id).expect("the restart failed");

        assert_eq!(core.sessions.len(), 3, "the restart changed the row count");
        assert_eq!(core.sessions[1].id, id, "the instance moved or was renumbered");
        assert_eq!(
            core.sessions[1].session_id, sid,
            "it came back on a different conversation"
        );
        assert!(core.sessions[1].is_alive(), "nothing was spawned in its place");
        assert_eq!(
            core.display_name(id).as_deref(),
            Some("two"),
            "the row lost the name the user gave it"
        );
        assert!(
            !core.state_dir.join(id.to_string()).exists(),
            "a killed child's `needs` still stands, so the sidebar and the dock badge lie"
        );
        assert!(
            !core.state_dir.join("armed").join(id.to_string()).exists(),
            "the resumed instance will never be nudged to re-arm its hub listener"
        );
        assert!(
            !core.failed.contains_key(&id),
            "still marked as failed to start, so it is not offered as a live peer"
        );

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ⌘S stops the claude but keeps its row until the user closes it; mid-turn
    /// and over an open question it refuses without killing anything.
    #[test]
    fn saving_stops_the_claude_and_holds_its_row_until_closed() {
        let saved = [record("one", Some(1)), record("two", Some(2))];
        let (_env, root, mut core) = core_from_store("saveholds", &saved);
        let id = core.sessions[1].id;
        let status = core.state_dir.join(id.to_string());

        for busy in ["working", "needs"] {
            std::fs::write(&status, busy).unwrap();
            assert!(core.begin_save(id).is_err(), "saved a claude that is {busy}");
            assert!(core.sessions[1].is_alive(), "a refused save killed the claude ({busy})");
        }
        std::fs::write(&status, "waiting").unwrap();

        let (_, uuid) = core.begin_save(id).expect("the save was refused");
        assert_eq!(uuid, core.sessions[1].session_id);
        assert!(wait_until(|| !core.sessions[1].is_alive()), "the claude was not stopped");
        assert!(core.reap_dead().is_empty(), "the held row was reaped");
        assert!(core.failed.is_empty(), "the held row was marked as failed to start");
        assert!(!core.live_instances().any(|s| s.id == id), "a held row is still a hub peer");
        assert!(core.restart_instance(id).is_err(), "⌘⇧R brought a saved claude back");
        // A retry on the held row needs no live process.
        assert!(core.begin_save(id).is_ok(), "a retry was refused");

        core.save_succeeded(id);
        assert!(
            !core.store.load().iter().any(|s| s.session_id == uuid),
            "a saved claude would come back at the next launch"
        );

        core.close(id);
        assert_eq!(core.reap_dead(), vec![id], "Close did not remove the held row");

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

    /// A restored claude that came up (its `SessionStart` reported a transcript)
    /// and was then quit from inside — Ctrl+C, `/exit` — is not a failed restore,
    /// however soon after launch it happened. It used to be kept as one, so the
    /// row the user had just quit came back at the next launch.
    #[test]
    fn a_restore_that_came_up_and_was_quit_is_not_kept() {
        let saved = [record("first", Some(1)), record("second", Some(2))];
        let (_env, root, mut core) = core_from_store("quitrestore", &saved);
        assert_eq!(core.sessions.len(), 2, "the restore did not spawn");
        let victim_id = core.sessions[1].id;
        let uuid = core.sessions[1].session_id.clone();
        // What the hook writes on `SessionStart`.
        let file = mulpex_core::session_id_path(&core.state_dir, victim_id);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, &uuid).unwrap();
        core.started
            .insert(victim_id, Instant::now() - EARLY_DEATH_GRACE - Duration::from_secs(1));
        core.sessions[1].kill();
        assert!(wait_until(|| !core.sessions[1].is_alive()), "it never died");
        core.reap_dead();
        assert_eq!(core.sessions.len(), 1, "the quit restore was not reaped");
        assert!(core.sticky.is_empty(), "a restore that came up was kept as failed");
        assert!(core.store.load().iter().all(|s| s.session_id != uuid));

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Closing restored rows one by one, right after launch, must not bring
    /// them back at the next launch.
    #[test]
    fn closing_every_restored_row_leaves_nothing_to_restore() {
        let saved = [record("first", Some(1)), record("second", Some(2)), record("third", Some(3))];
        let (_env, root, mut core) = core_from_store("closeall", &saved);
        assert_eq!(core.sessions.len(), 3, "the restore did not spawn");
        for id in [1, 2, 3] {
            core.close(id);
            assert!(wait_until(|| { core.reap_dead(); !core.sessions.iter().any(|s| s.id == id) }));
        }
        let left: Vec<_> = core.store.load().into_iter().map(|s| (s.id, s.name)).collect();
        assert!(left.is_empty(), "closed rows were written back: {left:?}");

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
            parent: None,
            collapsed: false,
            auto_compact: None,
            name_by: None,
            todos: false,
            agent: Default::default(),
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

    /// A ⌘K one-off secrets file belongs to the instance it was handed to, so it
    /// must not outlive that instance's reap.
    #[test]
    fn reaping_an_instance_deletes_its_secrets() {
        let (_env, root, mut core) = core_with_dead_instance("secrets-reap");
        let rows = [crate::secrets::SecretRow { key: "PASS".into(), value: "x".into() }];
        let file = crate::secrets::create_ephemeral(&core.state_dir, 1, &rows).unwrap();
        core.started
            .insert(1, Instant::now() - EARLY_DEATH_GRACE * 2);
        assert!(
            wait_until(|| !core.reap_dead().is_empty()),
            "the instance was not reaped"
        );
        assert!(!file.exists(), "the secrets file outlived its instance");

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

    /// The point of the PTY daemon: an app goes away without killing anything
    /// (here: its `Core` is leaked, as a crash or a detached restart leaves
    /// it), the next one opens the same project and gets the very same
    /// terminal back — same id, same name, same process, still taking input.
    #[test]
    fn a_reopened_project_adopts_the_sessions_the_daemon_kept() {
        let (_g, root, mut core) = scratch_core("adopt");
        let info = core.spawn_terminal(None, Some("my shell".into()), true).unwrap();
        let screen = crate::pty::terminal_screen_path(&core.state_dir, info.id);
        core.sessions[0].send(b"echo BEFORE-$((6*7))\r");
        assert!(wait_until(|| std::fs::read_to_string(&screen).is_ok_and(|t| t.contains("BEFORE-42"))));
        core.sync_adopt_manifest();
        let (project_dir, state_dir) = (core.project_dir.clone(), core.state_dir.clone());
        let sid = core.sessions[0].sid();
        std::mem::forget(core);

        let mut again = Core::open(2, project_dir, Path::new("/nonexistent/mulpex-helper"), state_dir, TEST_GEOMETRY).unwrap();
        assert_eq!(again.sessions.len(), 1, "the terminal was not adopted");
        let s = &again.sessions[0];
        assert_eq!((s.id, s.sid(), s.is_shell(), s.is_alive()), (info.id, sid, true, true));
        assert_eq!(again.display_name(info.id).as_deref(), Some("my shell"));
        assert!(again.next_id > info.id, "a new row could reuse the adopted id");
        again.sessions[0].send(b"echo AFTER-$((6*7))\r");
        assert!(wait_until(|| std::fs::read_to_string(&screen).is_ok_and(|t| t.contains("AFTER-42"))));
        again.teardown();
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

    /// A turn that ended with background shells still up reads `working`, and
    /// ⌘⇧K waited for `waiting` forever: viva-la-rts's claude#1671 stopped for
    /// its compact with a headless-browser test run in the background and sat at
    /// "compact: waiting for checkpoint…" until a hub message woke it. The
    /// snapshot now says when `working` is only that.
    #[test]
    fn the_snapshot_tells_a_finished_turn_with_background_work_from_a_live_one() {
        let (_env, root, mut core) = scratch_core("background-only-test");
        let id = core.spawn_instance().unwrap().id;
        let status = core.state_dir.join(id.to_string());
        let flag = core.state_dir.join("bg").join(id.to_string());
        std::fs::create_dir_all(core.state_dir.join("bg")).unwrap();
        let pause = || std::thread::sleep(std::time::Duration::from_millis(5));
        let bg_only = |core: &Core| {
            core.hub_snapshot().statuses.iter().find(|e| e.id == id).unwrap().background_only
        };

        std::fs::write(&status, "working").unwrap();
        assert!(!bg_only(&core), "a turn in progress");
        // `Stop`'s order: status, then the flag.
        pause();
        std::fs::write(&flag, "").unwrap();
        assert!(bg_only(&core), "turn over, only background work left");
        // The background job's wake starts a new turn.
        pause();
        std::fs::write(&status, "working").unwrap();
        assert!(!bg_only(&core), "a new turn is a live one");
        // A stale flag never makes an idle instance anything but idle.
        std::fs::write(&status, "waiting").unwrap();
        pause();
        std::fs::write(&flag, "").unwrap();
        assert!(!bg_only(&core), "only `working` can be background-only");

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
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
        assert_eq!(core.name_source(id), Some(NameSource::Auto), "a guess is not reported as a real name");

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
        assert_eq!(core.name_source(id), Some(NameSource::Instance));
        assert_eq!(core.store.load()[0].name_by, Some(NameSource::Instance), "the source must survive a restart");
        // ⌘R takes it over, and says so.
        core.rename(id, "Deployer");
        assert_eq!(core.registry_entry(&snap).instances[0].name_by.as_deref(), Some("user"));
        assert_eq!(core.store.load()[0].name_by, Some(NameSource::User));
        core.rename(id, "");
        assert_eq!(core.name_source(id), None);
        core.rename(id, "brand switcher gate");
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

    /// A prompt from the user undoes ⌘M on the instance it was sent to — and the
    /// mark is consumed whatever happens, so an unmuted row (the common case,
    /// since the hook writes the mark for every instance) or a reaped id can't
    /// leave a file the poll loop re-reads every tick.
    #[test]
    fn a_user_prompt_unmutes_the_instance_it_was_sent_to() {
        let (_env, root, mut core) = scratch_core("unmute-test");
        let dir = core.state_dir.join(mulpex_core::USERPROMPT_DIR);
        assert!(dir.is_dir(), "open() should create the mark dir");

        let muted = core.spawn_instance().unwrap().id;
        let loud = core.spawn_instance().unwrap().id;
        core.set_muted(muted, true);
        assert!(core.muted.contains(&muted));

        // A quiet tick changes nothing and reports nothing.
        assert!(!core.process_user_prompts(), "an empty dir must be a no-op");
        assert!(core.muted.contains(&muted), "nothing was prompted");

        // The user talks to the unmuted one: consumed, but no row changed, so the
        // poll loop must not republish the session list.
        std::fs::write(dir.join(loud.to_string()), "").unwrap();
        assert!(!core.process_user_prompts(), "unmuting an unmuted row is not a change");

        // …and to the muted one.
        std::fs::write(dir.join(muted.to_string()), "").unwrap();
        // An id nobody owns and a junk filename, neither of which may panic.
        std::fs::write(dir.join("999"), "").unwrap();
        std::fs::write(dir.join("notanid"), "").unwrap();
        assert!(core.process_user_prompts(), "a real user prompt must unmute");
        assert!(!core.muted.contains(&muted));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "marks must be consumed, or the poll loop re-reads them every tick"
        );
        // Persisted, or the next launch brings the mute back.
        assert!(
            core.store.load().iter().all(|s| !s.muted),
            "the unmute never reached the store"
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

    /// After a relaunch only a name the USER set stays locked. Seeding the lock
    /// from every restored name made `hub_set_name` silently fail for any
    /// instance that had named itself before the app restarted or updated.
    #[test]
    fn a_restore_locks_only_the_names_the_user_set() {
        let _guard = env_guard();
        let root = std::env::temp_dir().join(format!("mulpex-restore-names-{}", persist::new_uuid()));
        let project_dir = root.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::env::set_var("HOME", root.join("home"));

        let saved = |id: usize, name: &str, name_by: Option<NameSource>| persist::SavedSession {
            session_id: persist::new_uuid(),
            name: Some(name.into()),
            muted: false,
            id: Some(id),
            parent: None,
            collapsed: false,
            auto_compact: None,
            name_by,
            todos: false,
            agent: Default::default(),
        };
        SessionStore::new(&project_dir).save(&[
            saved(1, "Deployer", Some(NameSource::User)),
            saved(2, "auth fix", Some(NameSource::Instance)),
            // A store from before sources were saved: treated as the user's.
            saved(3, "legacy", None),
            saved(4, "spawned task", Some(NameSource::Spawn)),
            saved(5, "guide title", Some(NameSource::Auto)),
        ]);
        let mut core = Core::open(
            1,
            project_dir,
            Path::new("/nonexistent/mulpex-helper"),
            root.join("state"),
            TEST_GEOMETRY,
        )
        .unwrap();

        let mut locked: Vec<usize> = core.manual_names.iter().copied().collect();
        locked.sort();
        assert_eq!(locked, vec![1, 3]);
        for id in [1, 3] {
            assert!(mulpex_core::named_flag_path(&core.state_dir, id).exists(), "claude#{id} would be nudged");
        }

        let req = core.state_dir.join("namereq");
        for id in 1..=5 {
            std::fs::write(req.join(id.to_string()), "renamed mid-conversation").unwrap();
        }
        assert!(core.process_name_requests());
        assert_eq!(core.names[&1], "Deployer", "the user's name must win");
        assert_eq!(core.names[&3], "legacy", "an unknown-source name must stay the user's");
        for id in [2, 4, 5] {
            assert_eq!(core.names[&id], "renamed mid-conversation", "claude#{id} could not rename itself");
            assert_eq!(core.name_source(id), Some(NameSource::Instance));
        }

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

    /// A `hub_spawn` child is linked to its spawner — but only a live claude can
    /// be one, so a terminal or an unknown id makes a top-level row.
    #[test]
    fn a_spawned_child_hangs_under_a_live_claude_only() {
        let (_env, root, mut core) = scratch_core("parent-link");
        let parent = core.spawn_instance().unwrap().id;
        let term = core.spawn_terminal(None, None, false).unwrap().id;

        let child = core.spawn_instance_with_task_as(parent, "do a thing".into(), None).unwrap();
        assert_eq!(child.parent, Some(parent));
        let by_term = core.spawn_instance_with_task_as(term, "do a thing".into(), None).unwrap();
        assert_eq!(by_term.parent, None, "a terminal cannot be a parent");
        let by_ghost = core.spawn_instance_with_task_as(999, "do a thing".into(), None).unwrap();
        assert_eq!(by_ghost.parent, None, "an unknown spawner makes a root");

        let infos = core.session_infos();
        let of = |id: usize| infos.iter().find(|s| s.id == id).unwrap().parent;
        assert_eq!(of(child.id), Some(parent));
        assert_eq!(of(parent), None);

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Muting a parent mutes its whole family and unmuting it unmutes it; a
    /// child can still be flipped alone; a child born to a muted parent starts
    /// muted.
    #[test]
    fn mute_follows_the_family() {
        let (_env, root, mut core) = scratch_core("famute");
        let a = core.spawn_instance().unwrap().id;
        let b = core.spawn_instance_with_task_as(a, "x".into(), None).unwrap().id;
        let c = core.spawn_instance_with_task_as(b, "x".into(), None).unwrap().id;
        let other = core.spawn_instance().unwrap().id;

        core.set_muted(a, true);
        assert!([a, b, c].iter().all(|id| core.muted.contains(id)));
        assert!(!core.muted.contains(&other));

        let d = core.spawn_instance_with_task_as(a, "x".into(), None).unwrap();
        assert!(d.muted && core.muted.contains(&d.id), "a child of a muted parent starts muted");

        core.set_muted(c, false);
        assert!(!core.muted.contains(&c) && core.muted.contains(&b), "a child flips alone");

        core.set_muted(a, false);
        assert!(core.muted.is_empty(), "unmuting the parent unmutes the family: {:?}", core.muted);

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Auto-compact is set only on a claude, only to a menu choice, persists,
    /// and tells the orchestrator exactly once per change.
    #[test]
    fn auto_compact_rule_is_validated_persisted_and_announced() {
        let (_env, root, mut core) = scratch_core("autocompact");
        let a = core.spawn_instance().unwrap().id;
        let term = core.spawn_terminal(None, None, false).unwrap().id;
        core.worked.insert(a);
        let inbox = core.state_dir.join("inbox").join(a.to_string());
        let mail = || std::fs::read_dir(&inbox).map(|d| d.count()).unwrap_or(0);

        assert!(!core.set_auto_compact(term, Some(70)), "a terminal");
        assert!(!core.set_auto_compact(a, Some(73)), "not a menu choice");
        assert_eq!(mail(), 0);

        assert!(core.set_auto_compact(a, Some(70)));
        assert!(!core.set_auto_compact(a, Some(70)), "no change, no second message");
        assert_eq!(mail(), 1);
        let infos = core.session_infos();
        assert_eq!(infos.iter().find(|s| s.id == a).unwrap().auto_compact, Some(70));
        let saved = core.store.load();
        assert_eq!(saved.iter().find(|s| s.id == Some(a)).unwrap().auto_compact, Some(70));

        assert!(core.set_auto_compact(a, None));
        assert_eq!(mail(), 2);
        assert!(core.auto_compact.is_empty());

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Drag-to-nest moves a claude (family and all) under another claude or
    /// back to the top, unfolds the new parent, keeps mute alone, and refuses
    /// a cycle or a terminal on either end without touching anything.
    #[test]
    fn reparent_moves_a_family_and_refuses_cycles() {
        let (_env, root, mut core) = scratch_core("renest");
        let a = core.spawn_instance().unwrap().id;
        let b = core.spawn_instance_with_task_as(a, "x".into(), None).unwrap().id;
        let other = core.spawn_instance().unwrap().id;
        let term = core.spawn_terminal(None, None, false).unwrap().id;
        core.set_muted(other, true);
        core.set_collapsed(other, true);

        // a (with child b) goes under `other`, last in the order given.
        assert!(core.reparent(a, Some(other), &[other, a, b, term]));
        assert_eq!(core.parents.get(&a), Some(&other));
        assert_eq!(core.parents.get(&b), Some(&a), "the family moves along");
        assert!(!core.collapsed.contains(&other), "the new parent unfolds");
        assert!(!core.muted.contains(&a), "moving never changes mute");
        let order: Vec<usize> = core.sessions.iter().map(|s| s.id).collect();
        assert_eq!(order, vec![other, a, b, term]);

        // Cycles and terminals are refused, and change nothing.
        let before = core.parents.clone();
        assert!(!core.reparent(other, Some(b), &[b, other]), "a cycle");
        assert!(!core.reparent(a, Some(a), &[a]), "onto itself");
        assert!(!core.reparent(a, Some(term), &[term, a]), "under a terminal");
        assert!(!core.reparent(term, Some(a), &[a, term]), "a terminal");
        assert_eq!(core.parents, before);
        let order: Vec<usize> = core.sessions.iter().map(|s| s.id).collect();
        assert_eq!(order, vec![other, a, b, term], "a refusal must not reorder");

        // Back to the top level.
        assert!(core.reparent(a, None, &[a, b, other, term]));
        assert!(!core.parents.contains_key(&a));
        assert_eq!(core.parents.get(&b), Some(&a));

        core.teardown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Closing a parent moves its children up one level — to the grandparent,
    /// or to the top — never leaves them pointing at a row that is gone.
    #[test]
    fn closing_a_parent_moves_its_children_up_one_level() {
        let (_env, root, mut core) = scratch_core("reparent");
        // The link is kind-agnostic inside `reap_dead`, and terminals reap
        // deterministically, so they stand in for claudes here.
        let ids: Vec<usize> = (0..4)
            .map(|_| core.spawn_terminal(None, None, false).unwrap().id)
            .collect();
        let (a, b, c, d) = (ids[0], ids[1], ids[2], ids[3]);
        // a ─ b ─ c, and b ─ d
        core.parents.insert(b, a);
        core.parents.insert(c, b);
        core.parents.insert(d, b);

        core.close(b);
        assert!(wait_until(|| core.reap_dead() == vec![b]));
        assert_eq!(core.parents.get(&c), Some(&a));
        assert_eq!(core.parents.get(&d), Some(&a));
        assert!(!core.parents.contains_key(&b));

        core.close(a);
        assert!(wait_until(|| core.reap_dead() == vec![a]));
        assert!(core.parents.is_empty(), "children of a root become roots: {:?}", core.parents);

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

    /// `running: true` on a terminal only ever meant "the shell process is
    /// alive" — true at an idle prompt and true three minutes into a build, so a
    /// reader could not tell the two apart for a terminal the USER was driving
    /// (Mulpex tracks its own submissions by completion marker; nothing typed by
    /// hand is tracked).
    ///
    /// The answer comes from the kernel, not from parsing the screen: an
    /// interactive shell puts each job in its own process group and hands the
    /// tty to it, so the tty's foreground group (`e_tpgid`) differing from the
    /// shell's own (`pbi_pgid`) IS a command running. Driven here against a real
    /// shell on a real PTY, because that disagreement is the entire claim.
    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn a_terminal_reports_whether_a_command_is_running_and_where_it_sits() {
        let (_env, root, mut core) = scratch_core("term-fg-test");
        let info = core.spawn_terminal(None, None, true).unwrap();
        let meta_path = core.state_dir.join("terminals").join("meta");
        let state_now = |core: &mut Core| -> Option<serde_json::Value> {
            core.sync_terminal_index();
            let raw = std::fs::read_to_string(&meta_path).ok()?;
            let all: serde_json::Value = serde_json::from_str(&raw).ok()?;
            all.get(info.id.to_string()).cloned()
        };

        // The shell has to be up and at its prompt before any of this means
        // anything: a shell that has not yet claimed the tty reports nothing.
        assert!(
            wait_until(|| state_now(&mut core).is_some()),
            "the terminal never reported its state"
        );
        assert!(
            wait_until(|| state_now(&mut core)
                .map(|m| m["command_running"] == serde_json::json!(false))
                .unwrap_or(false)),
            "an idle prompt was reported as running a command: {:?}",
            state_now(&mut core)
        );
        // cwd is the project the terminal was opened in. Compare canonically:
        // the scratch root is under /var, which is a symlink to /private/var.
        let want = std::fs::canonicalize(&core.project_dir).unwrap();
        let got = state_now(&mut core).unwrap()["cwd"].as_str().unwrap().to_string();
        assert_eq!(std::fs::canonicalize(&got).unwrap(), want, "wrong cwd: {got}");

        // Now run something slow in the foreground.
        core.sessions
            .iter_mut()
            .find(|s| s.id == info.id)
            .unwrap()
            .send(b"sleep 5\r");
        assert!(
            wait_until(|| state_now(&mut core)
                .map(|m| m["command_running"] == serde_json::json!(true))
                .unwrap_or(false)),
            "a foreground `sleep 5` was reported as an idle prompt"
        );

        // …and it goes back to false on its own when the command ends.
        assert!(
            wait_until(|| state_now(&mut core)
                .map(|m| m["command_running"] == serde_json::json!(false))
                .unwrap_or(false)),
            "the terminal never came back to its prompt"
        );

        // A dead terminal reports nothing rather than a stale yes/no.
        core.close(info.id);
        assert!(wait_until(|| core.reap_dead() == vec![info.id]));
        assert!(
            wait_until(|| state_now(&mut core).is_none()),
            "a closed terminal is still claiming a state"
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
            parent: None,
            collapsed: false,
            auto_compact: None,
            name_by: None,
            todos: false,
            agent: Default::default(),
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

    /// END-TO-END, against a real `claude`: when the transcript moves under a
    /// live process, the store follows it.
    ///
    /// The one thing a unit test cannot prove. Every piece of this was already
    /// green while warweb#75 was breaking — the hook fired, the store was
    /// written, `--resume` was passed a uuid. What nobody checked was whether
    /// that uuid still named a file Claude Code had, and only a real child can
    /// answer that.
    ///
    /// `/clear` is the divergence, chosen because it is drivable: it starts a
    /// new conversation in a **new transcript file** without restarting the
    /// process, which is the same shape as the in-TUI `/resume` that actually
    /// did it. The assertions are deliberately about files on disk, not about
    /// our own bookkeeping: the new uuid must have a `.jsonl` behind it and the
    /// old one is allowed to be anything.
    ///
    /// `#[ignore]`d — it spawns a real `claude`, burns a turn and takes ~a
    /// minute. Run alone:
    ///   cargo test --lib -- --ignored --nocapture --exact \
    ///     state::tests::the_store_follows_a_transcript_that_moves_under_a_live_claude
    #[test]
    #[ignore]
    fn the_store_follows_a_transcript_that_moves_under_a_live_claude() {
        let _env = env_guard();
        let root = std::env::temp_dir().join(format!("mulpex-sidlive-{}", persist::new_uuid()));
        let project_dir = root.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        // An isolated store, not the app's own `~/.mulpex/sessions/`.
        std::env::set_var("MULPEX_HOME", root.join("mulpex-home"));
        // An isolated Claude Code config too, so this test writes no transcript
        // into a real project's history and reads back its own. HOME stays real
        // — only the config dir moves — and `CLAUDE_CODE_OAUTH_TOKEN` from the
        // environment is what keeps it logged in.
        //
        // Three first-run dialogs have to be pre-answered, or `claude` sits on a
        // prompt and the first keystroke picks "No, exit" (which is exactly how
        // this test failed the first time it ran — `alive=false`, and no hook
        // had ever fired). They are artefacts of a fresh config, not of Mulpex:
        // a real user's dir is already trusted and onboarded, and the bypass
        // warning is off in their own `settings.json`.
        let cfg = root.join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        let trusted = std::fs::canonicalize(&project_dir).unwrap();
        std::fs::write(
            cfg.join(".claude.json"),
            serde_json::json!({
                "hasCompletedOnboarding": true,
                "lastOnboardingVersion": "99.0.0",
                "theme": "dark",
                "projects": { trusted.to_string_lossy(): { "hasTrustDialogAccepted": true } },
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            cfg.join("settings.json"),
            r#"{"skipDangerousModePermissionPrompt":true}"#,
        )
        .unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &cfg);

        // Copied, not referenced. `target/debug/mulpex-helper` is a hardlink
        // cargo re-points as it builds other members of the workspace, and this
        // test ran once against a helper eleven hours older than the change it
        // was testing — the hooks fired, wrote everything else, and simply did
        // not know about this one. A copy cannot be swapped underneath it.
        let built = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target/debug/mulpex-helper");
        assert!(built.exists(), "build mulpex-helper first: {built:?}");
        let helper = root.join("mulpex-helper");
        std::fs::copy(&built, &helper).unwrap();

        let mut core =
            Core::open(1, project_dir.clone(), &helper, root.join("state"), TEST_GEOMETRY).unwrap();
        let id = core.spawn_instance().unwrap().id;
        let minted = core.sessions[0].session_id.clone();
        eprintln!("root {root:?}\nspawned claude#{id} with --session-id {minted}");

        // One real turn, so a transcript exists and the hooks have fired.
        let poll = |core: &mut Core, secs: u64, f: &dyn Fn(&Core) -> bool| -> bool {
            for _ in 0..(secs * 5) {
                std::thread::sleep(Duration::from_millis(200));
                // Both halves of what the real poll loop does per tick:
                // `refresh_worked` is what puts the instance in the store at
                // all, and `reconcile_session_ids` is what keeps its uuid true.
                core.refresh_worked();
                core.reconcile_session_ids();
                if f(core) {
                    return true;
                }
            }
            false
        };
        std::thread::sleep(Duration::from_secs(8));
        core.sessions[0].send(b"say only the word ok\r");
        let reported = |core: &Core| -> Option<String> {
            std::fs::read_to_string(mulpex_core::session_id_path(&core.state_dir, id))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        if !poll(&mut core, 90, &|c| reported(c).is_some()) {
            let alive = core.sessions.first().is_some_and(|s| s.is_alive());
            let listing: Vec<String> = std::fs::read_dir(&core.state_dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            panic!(
                "no claude ever reported its transcript — the hook never ran.\n\
                 alive={alive} state_dir={:?}\n{listing:?}",
                core.state_dir
            );
        }
        assert_eq!(
            reported(&core).as_deref(),
            Some(minted.as_str()),
            "a fresh instance's transcript should be the uuid we minted"
        );

        // Now move the transcript under it.
        core.sessions[0].send(b"/clear\r");
        let moved = poll(&mut core, 60, &|c| reported(c).as_deref() != Some(minted.as_str()));
        assert!(moved, "/clear did not move the transcript — pick another divergence");
        let real = reported(&core).unwrap();
        eprintln!("transcript moved: {minted} -> {real}");

        // The claim that matters: the uuid now in the store names a file that
        // exists, and `--resume` would therefore find it.
        assert_eq!(core.sessions[0].session_id, real);
        assert_eq!(core.store.load()[0].session_id, real, "the store kept the stale uuid");
        let slug: String = trusted
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect();
        let transcript = cfg.join("projects").join(&slug).join(format!("{real}.jsonl"));
        assert!(
            transcript.exists(),
            "the store now names a transcript that does not exist either: {transcript:?}"
        );

        core.teardown();
        std::env::remove_var("MULPEX_HOME");
        std::env::remove_var("CLAUDE_CONFIG_DIR");
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
            parent: None,
            collapsed: false,
            auto_compact: None,
            name_by: None,
            todos: false,
            agent: Default::default(),
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
            parent: None,
            collapsed: false,
            auto_compact: None,
            name_by: None,
            todos: false,
            agent: Default::default(),
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

    /// A terminal the USER opened has no label at all — it reaches an instance as
    /// `name: null`, which with four or five terminals open leaves it telling
    /// them apart by their output. An instance can name one, but only if nobody
    /// has: a name that is already there is usually the user's, and the refusal
    /// reports it so the caller learns what the row is instead of just being
    /// told no.
    #[test]
    fn an_instance_can_label_an_unnamed_terminal_but_never_rename_one() {
        let (_env, root, mut core) = scratch_core("term-name-test");
        let dir = core.state_dir.join("termreq");
        let post = |body: serde_json::Value| {
            let token = persist::new_uuid();
            std::fs::write(dir.join(format!("{token}.json")), body.to_string()).unwrap();
            token
        };
        let reply = |token: &str| -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{token}.done"))).unwrap())
                .unwrap()
        };

        // A terminal the user opened: no label.
        let user_term = core.spawn_terminal(None, None, true).unwrap();
        core.sync_terminal_index();
        let index_path = core.state_dir.join("terminals").join("index");
        let index = || std::fs::read_to_string(&index_path).unwrap();
        assert!(
            index()
                .lines()
                .any(|l| l == format!("{}\trunning\t", user_term.id)),
            "expected an unnamed row: {:?}",
            index()
        );

        // An instance labels it, and the sidebar manifest follows.
        let t = post(serde_json::json!({
            "op": "name", "from": 1, "id": user_term.id, "label": "dev server",
        }));
        assert!(
            core.process_terminal_requests(),
            "naming a row must repaint the session list"
        );
        assert_eq!(reply(&t)["ok"], true);
        core.sync_terminal_index();
        assert!(index().contains("dev server"), "{:?}", index());

        // A second attempt is refused, and says what the name already is.
        let t = post(serde_json::json!({
            "op": "name", "from": 2, "id": user_term.id, "label": "something else",
        }));
        core.process_terminal_requests();
        let r = reply(&t);
        assert_eq!(r["ok"], false);
        assert!(
            r["error"].as_str().unwrap().contains("dev server"),
            "the refusal must name the existing label: {r}"
        );
        core.sync_terminal_index();
        assert!(!index().contains("something else"), "{:?}", index());

        // …including when the USER named it by hand.
        let mine = core.spawn_terminal(None, None, true).unwrap();
        core.rename(mine.id, "my build");
        let t = post(serde_json::json!({
            "op": "name", "from": 1, "id": mine.id, "label": "hijacked",
        }));
        core.process_terminal_requests();
        assert_eq!(reply(&t)["ok"], false);
        core.sync_terminal_index();
        assert!(!index().contains("hijacked"), "{:?}", index());

        // A claude is not a terminal, and an unknown id is not one either.
        let t = post(serde_json::json!({ "op": "name", "from": 1, "id": 999, "label": "x" }));
        core.process_terminal_requests();
        assert!(reply(&t)["error"].as_str().unwrap().contains("no term#999"));

        core.teardown();
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

    /// `hub_close`'s end of that same handshake.
    ///
    /// The point of the assertions below is the BATCH: an orchestrator cleaning
    /// up six workers must not lose the five it can close because the sixth is
    /// mid-turn. So every id is decided on its own and reported on its own, and
    /// a request that refuses some of its targets is still `ok` — the refusals
    /// are a report, not a failure. Only `hub_close` itself turns an all-refused
    /// batch into an error, where nothing the caller asked for happened.
    #[test]
    fn close_instance_closes_idle_claudes_and_refuses_the_rest() {
        let (_env, root, mut core) = scratch_core("close-req-test");
        let dir = core.state_dir.join("termreq");
        std::fs::create_dir_all(&dir).unwrap();

        let post = |body: serde_json::Value| {
            let token = persist::new_uuid();
            std::fs::write(dir.join(format!("{token}.json")), body.to_string()).unwrap();
            token
        };
        let reply = |token: &str| -> serde_json::Value {
            let p = dir.join(format!("{token}.done"));
            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
        };
        let reason = |r: &serde_json::Value, id: usize| -> String {
            r["refused"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["id"].as_u64() == Some(id as u64))
                .unwrap_or_else(|| panic!("#{id} was neither closed nor refused: {r}"))["reason"]
                .as_str()
                .unwrap()
                .to_string()
        };

        let idle = core.spawn_instance().unwrap().id;
        let busy = core.spawn_instance().unwrap().id;
        let t = post(serde_json::json!({ "op": "open", "from": 1, "label": "shell" }));
        core.process_terminal_requests();
        let term = reply(&t)["id"].as_u64().unwrap() as usize;

        // What a mid-turn instance looks like on disk: the word its hook writes.
        std::fs::write(core.state_dir.join(busy.to_string()), "working").unwrap();

        let t = post(serde_json::json!({
            "op": "close_instance", "from": 1, "ids": [idle, busy, term, 999],
        }));
        assert!(core.process_terminal_requests());
        let r = reply(&t);
        assert_eq!(r["ok"], true, "a partly-refused batch is still a served request");
        assert_eq!(r["closed"], serde_json::json!([idle]));
        assert!(reason(&r, busy).contains("mid-turn"));
        assert!(
            reason(&r, term).contains("hub_terminal_close"),
            "a terminal must be refused with the tool that DOES close it"
        );
        assert!(reason(&r, 999).contains("no claude#999"));

        // The busy one is untouched: refusing has to mean not killing it.
        assert!(core.sessions.iter().any(|s| s.id == busy && s.is_alive()));

        let t = post(serde_json::json!({
            "op": "close_instance", "from": 1, "ids": [busy], "force": true,
        }));
        core.process_terminal_requests();
        assert_eq!(reply(&t)["closed"], serde_json::json!([busy]));

        // Both rows actually go away — `closing` is what makes a killed claude
        // removable rather than kept as a failed start.
        assert!(wait_until(|| {
            core.reap_dead();
            !core.sessions.iter().any(|s| s.id == idle || s.id == busy)
        }));

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
        let cmd = remote::remote_launch_command(Some("/tmp/mpx-probe"), &rules_b64, false, None);
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
        let cmd = remote::ssh_command(&target, Some("/tmp/mpx-probe"), &rules_b64, None);
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
        // NOTE: this is no longer how a remote gets its INITIAL task —
        // `hub_remote_open` puts that on the launch command line, because typing
        // capped it at 1022 characters (see ../../docs/remote-peers.md). What is
        // exercised here is talking to an ALREADY-RUNNING remote, which is still
        // typed (`hub_terminal_send`), and the signalling round-trip that follows.
        // Keep the task short for that reason, and keep the two writes separate:
        // a `\r` on the tail of the same burst is swallowed as paste content and
        // nothing is ever submitted. That is the shape that failed live.
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

/// Live tests on a real `codex` on the daemon's PTY. `#[ignore]`d: each one
/// starts codex, spends real turns and takes a minute or two. They use a
/// throwaway `CODEX_HOME` (your `auth.json` copied in) so nothing is written
/// into your own Codex history, and a throwaway `MULPEX_HOME`. Run:
///   cargo build -p mulpex-helper && cargo test -p mulpex --lib -- \
///     --ignored --nocapture --test-threads=1 codex_live
#[cfg(test)]
mod codex_live {
    use super::tests::env_guard;
    use super::*;

    struct Live {
        root: PathBuf,
        project_dir: PathBuf,
        helper: PathBuf,
    }

    fn setup(tag: &str) -> Live {
        let root = std::env::temp_dir().join(format!("mulpex-codexlive-{tag}-{}", persist::new_uuid()));
        let project_dir = root.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::env::set_var("MULPEX_HOME", root.join("mulpex-home"));
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
        // One per test process, not per test: the login-shell environment
        // `claude_bin` forwards to every child is probed once and inherits OUR
        // env, so it pins whatever CODEX_HOME the first test set.
        static CODEX_HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        let codex_home = CODEX_HOME
            .get_or_init(|| std::env::temp_dir().join(format!("mulpex-codexlive-home-{}", persist::new_uuid())))
            .clone();
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::copy(home.join(".codex/auth.json"), codex_home.join("auth.json"))
            .expect("codex must be logged in (~/.codex/auth.json)");
        std::fs::write(
            codex_home.join("config.toml"),
            "model_reasoning_effort = \"low\"\n",
        )
        .unwrap();
        std::env::set_var("CODEX_HOME", &codex_home);
        let built = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("target/debug/mulpex-helper");
        assert!(built.exists(), "build mulpex-helper first: {built:?}");
        let helper = root.join("mulpex-helper");
        std::fs::copy(&built, &helper).unwrap();
        Live { root, project_dir, helper }
    }

    fn done(l: Live) {
        std::env::remove_var("MULPEX_HOME");
        let _ = std::fs::remove_dir_all(&l.root);
    }

    /// One poll-loop tick's worth of what `hub.rs` runs, until `f` holds.
    fn poll(core: &mut Core, secs: u64, f: &dyn Fn(&Core) -> bool) -> bool {
        for _ in 0..(secs * 5) {
            std::thread::sleep(Duration::from_millis(200));
            core.reap_dead();
            core.refresh_worked();
            core.reconcile_session_ids();
            let _ = core.tick_wakes();
            if f(core) {
                return true;
            }
        }
        false
    }

    fn status(core: &Core, id: usize) -> String {
        std::fs::read_to_string(core.state_dir.join(id.to_string())).unwrap_or_default().trim().to_string()
    }

    fn listing(core: &Core) -> String {
        let mut out = String::new();
        for s in &core.sessions {
            let ring = s.desktop_tap().ring;
            let tail = String::from_utf8_lossy(&ring[ring.len().saturating_sub(3000)..]).into_owned();
            out.push_str(&format!("pane #{} (failed: {:?}):\n{tail}\n", s.id, core.failed.get(&s.id)));
        }
        for d in ["", "agents", "agentready", "sessionid", "spawning", "wake", "inbox"] {
            let names: Vec<String> = std::fs::read_dir(core.state_dir.join(d))
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            out.push_str(&format!("{d}/: {names:?}\n"));
        }
        out
    }

    /// The whole lifecycle of one codex row: a 12 KB `hub_spawn` task arrives
    /// whole on its command line and is verified by its own hook; Codex's
    /// thread id is adopted into the row and the store; ⌘⇧R resumes that same
    /// thread; and a hub message to the idle, resumed codex is rung through
    /// `codex queue` and acked by its hook.
    #[test]
    #[ignore]
    fn codex_live_spawn_reconcile_restart_and_queue_wake() {
        let _env = env_guard();
        let l = setup("life");
        let mut core = Core::open(1, l.project_dir.clone(), &l.helper, l.root.join("state"), (100, 40)).unwrap();

        // Padding the model is told to ignore; the hook compares every byte.
        let pad: String = (0..240).map(|i| format!("filler-{i:04}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx ")).collect();
        let task = format!("Reply with exactly the word READY and nothing else. Ignore this padding: {pad}");
        assert!(task.len() > 12_000, "{}", task.len());
        let info = core.spawn_instance_with_task_as(42, task, Some(Agent::Codex)).unwrap();
        let id = info.id;
        assert_eq!(info.kind, SessionKind::Codex);
        assert_eq!(core.sessions[0].session_id, "", "a codex has no id before its first prompt");
        assert_eq!(std::fs::read_to_string(mulpex_core::agents_path(&core.state_dir, id)).unwrap(), "codex");
        eprintln!("root {:?}", l.root);

        // The task reached the hook whole (no verdict file = delivered + verified),
        // Codex minted a thread and Mulpex adopted it.
        let ok = poll(&mut core, 150, &|c| {
            !c.sessions[0].session_id.is_empty()
                && !mulpex_core::spawn_delivery_path(&c.state_dir, id).exists()
                && status(c, id) == "waiting"
        });
        assert!(
            ok,
            "spawn never settled: alive={} sid={:?} status={:?} verdict={:?}\n{}",
            core.sessions[0].is_alive(),
            core.sessions[0].session_id,
            status(&core, id),
            std::fs::read_to_string(mulpex_core::spawn_delivery_path(&core.state_dir, id)).ok(),
            listing(&core),
        );
        let thread = core.sessions[0].session_id.clone();
        eprintln!("codex#{id} thread {thread}");
        let stored = core.store.load();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].session_id, thread);
        assert_eq!(stored[0].agent, Agent::Codex);

        // ⌘⇧R: same row, same thread, resumed.
        core.restart_instance(id).unwrap();
        assert_eq!(core.sessions[0].session_id, thread);
        assert_eq!(core.sessions[0].kind, pty::SessionKind::Codex);
        std::thread::sleep(Duration::from_secs(12));
        core.reap_dead();
        assert!(core.sessions[0].is_alive(), "the resumed codex died");
        assert!(!core.failed.contains_key(&id), "{:?}", core.failed.get(&id));

        // Mail to the idle, resumed codex: rung via codex queue, acked by its hook.
        core.send_app_message(id, "Test message. Read it with hub_inbox, then reply with the word DONE.");
        let rung = poll(&mut core, 90, &|c| c.wakes.mark(id).is_some_and(|m| m.state == "delivered"));
        let log = std::fs::read_to_string(mulpex_core::wake_log_path(&core.state_dir, id)).unwrap_or_default();
        assert!(rung, "the wake was never acked: {:?}\nlog:\n{log}\n{}", core.wakes.mark(id), listing(&core));
        eprintln!("wake ledger:\n{log}");
        // ...and it read the mail.
        let read = poll(&mut core, 120, &|c| {
            std::fs::read_dir(c.state_dir.join("inbox").join(id.to_string())).map(|d| d.count()).unwrap_or(0) == 0
        });
        assert!(read, "the woken codex never read its inbox\n{}", listing(&core));
        assert_eq!(core.sessions[0].session_id, thread, "the resumed codex moved to another thread");

        core.teardown();
        done(l);
    }

    /// `hub_spawn` from a codex makes a codex by default and hangs it under its
    /// spawner; ⌘⌥S's `replace_with_agent` swaps the row's agent in place,
    /// keeping its id, name, parent and mail, and the swap survives the store.
    #[test]
    #[ignore]
    fn codex_live_spawn_default_and_replace_with_agent() {
        let _env = env_guard();
        let l = setup("swap");
        // A claude that cannot log in or write into your own ~/.claude: it
        // only has to be the process on the row.
        let cfg = l.root.join("claude-cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &cfg);
        let mut core = Core::open(1, l.project_dir.clone(), &l.helper, l.root.join("state"), (100, 40)).unwrap();
        let parent = core.spawn_agent(Agent::Codex).unwrap().id;
        let child = core.spawn_instance_with_task_as(parent, "Reply with exactly READY.".into(), None).unwrap();
        assert_eq!(child.kind, SessionKind::Codex, "a codex's hub_spawn defaults to codex");
        assert_eq!(child.parent, Some(parent));
        let ok = poll(&mut core, 120, &|c| c.session_id_of(child.id).is_some_and(|s| !s.is_empty()));
        assert!(ok, "the child never got a thread\n{}", listing(&core));

        core.send_app_message(child.id, "kept across the swap");
        core.names.insert(child.id, "swapped row".into());
        core.replace_with_agent(child.id, Agent::Claude, "say hi".into()).unwrap();
        let s = core.sessions.iter().find(|s| s.id == child.id).unwrap();
        assert_eq!(s.kind, pty::SessionKind::Claude);
        assert!(!s.session_id.is_empty(), "a claude gets a minted id");
        assert_eq!(core.parents.get(&child.id), Some(&parent));
        assert_eq!(core.display_name(child.id).as_deref(), Some("swapped row"));
        assert!(!mulpex_core::agents_path(&core.state_dir, child.id).exists(), "agents/<id> still says codex");
        assert!(!mulpex_core::session_id_path(&core.state_dir, child.id).exists());
        assert_eq!(
            std::fs::read_dir(core.state_dir.join("inbox").join(child.id.to_string())).unwrap().count(),
            1,
            "the mail went with the old agent"
        );
        std::thread::sleep(Duration::from_secs(3));
        core.reap_dead();
        assert!(core.sessions.iter().any(|s| s.id == child.id && s.is_alive()), "{}", listing(&core));

        core.teardown();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        done(l);
    }

    /// A codex restored from the store whose thread Codex does not have exits
    /// 1 ("No saved session found") and becomes a failed row that says so,
    /// with its record kept.
    #[test]
    #[ignore]
    fn codex_live_a_bad_thread_id_restores_as_a_failed_row() {
        let _env = env_guard();
        let l = setup("bad");
        let bogus = persist::new_uuid();
        SessionStore::new(&l.project_dir).save(&[persist::SavedSession {
            session_id: bogus.clone(),
            name: Some("gone".into()),
            muted: false,
            id: Some(5),
            parent: None,
            collapsed: false,
            auto_compact: None,
            name_by: None,
            todos: false,
            agent: Agent::Codex,
        }]);
        let mut core = Core::open(1, l.project_dir.clone(), &l.helper, l.root.join("state"), (100, 40)).unwrap();
        assert_eq!(core.sessions.len(), 1);
        assert_eq!(core.sessions[0].kind, pty::SessionKind::Codex);
        let failed = poll(&mut core, 30, &|c| c.failed.contains_key(&5));
        assert!(failed, "the bad resume never failed: alive={}", core.sessions[0].is_alive());
        let reason = core.failed[&5].clone();
        eprintln!("reason: {reason}");
        assert!(reason.contains("Codex could not reopen") && reason.contains(&bogus), "{reason}");
        // The record is kept for a later try, as a codex.
        let stored = core.store.load();
        assert!(stored.iter().any(|s| s.session_id == bogus && s.agent == Agent::Codex), "{stored:?}");
        core.teardown();
        done(l);
    }
}
