//! Serializable types shared with the Svelte frontend (Tauri command returns +
//! emitted events). These mirror what the old TUI held in `App` and rendered in
//! `pane.rs`; here the frontend does the rendering, so they are plain data.

use serde::Serialize;

/// A stable, monotonically-allocated id for one open project. Never reused after a
/// project closes, so a stale frontend reference resolves to "no such project"
/// rather than aliasing a newly-opened one. Serializes to a JS number.
pub type ProjectHandle = u64;

/// What a Claude instance is doing, derived from its lifecycle hooks. Serializes
/// to the lowercase word the frontend maps to a status-dot color.
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Mid-turn: a prompt was submitted or a tool just ran. (yellow)
    Working,
    /// Finished its turn (Stop), or freshly spawned and ready. (green)
    Waiting,
    /// Blocked on the user: a question, permission, or idle wait. (red)
    Needs,
}

impl Status {
    /// Parse the single word a hook writes into the state file.
    pub fn from_word(word: &str) -> Option<Self> {
        match word {
            "working" => Some(Status::Working),
            "waiting" => Some(Status::Waiting),
            "needs" => Some(Status::Needs),
            _ => None,
        }
    }

    /// Back to that word — for the workspace registry, which another project's
    /// helper reads as plain text rather than through serde.
    pub fn word(self) -> &'static str {
        match self {
            Status::Working => "working",
            Status::Waiting => "waiting",
            Status::Needs => "needs",
        }
    }
}

/// What a session runs. Terminals share the session list, the id space and every
/// `(project, id)`-keyed mechanism with instances; they differ in what the hub
/// knows about them (nothing) and how the sidebar draws them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
    Claude,
    Shell,
}

/// One session as the sidebar shows it.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct SessionInfo {
    pub id: usize,
    /// Custom name (from rename), if any — overrides the auto task line.
    pub name: Option<String>,
    /// Muted (⌘M): the frontend dims its row, sinks it below the unmuted ones,
    /// and drops it from every attention badge. Purely presentational — the
    /// instance keeps running and stays a full hub peer.
    pub muted: bool,
    pub kind: SessionKind,
    /// A terminal whose shell has exited. Unlike a dead instance it is *kept*,
    /// so its output stays readable until it's explicitly closed — otherwise a
    /// command's output would vanish the moment the command finished. Always
    /// false for a Claude session, which is removed the instant it dies.
    pub exited: bool,
    /// Why this instance never started, if it didn't — it died within
    /// `EARLY_DEATH_GRACE` of spawning. Such a row is kept rather than reaped,
    /// so the reason survives on screen; without it the session appeared and
    /// vanished in about 100 ms, telling the user nothing.
    ///
    /// The sidebar must key its status readout off this, because a failed
    /// instance is deliberately absent from `statuses` and would otherwise fall
    /// back to the unknown-id default and paint a green "ready" dot on a
    /// session that is dead.
    pub failed: Option<String>,
}

/// A locked file → holder, for the hub panel's Locks view.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct LockEntry {
    pub path: String,
    pub holder: usize,
}

/// An instance blocked waiting on a locked file, for the ⏳ indicator.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct WaitEntry {
    pub id: usize,
    pub file: String,
    pub holder: usize,
}

/// One record from the persistent conversation log.
///
/// Both ends are *addresses*, not numbers: `claude#2` for an instance in this
/// project, `all` for a broadcast, `central-one#3` for one in another open
/// project. `from` used to be a bare `usize`, which had nowhere to put the
/// project once a sender could live in a different one. The log is written into a
/// per-pid scratch dir, so changing its shape costs no compatibility.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct MsgEntry {
    pub from: String,
    pub to: String,
    pub body: String,
    pub ts: u64,
}

/// The live coordination state the backend poll loop emits to the frontend on
/// change. Everything the sidebar + hub panel render, in one payload.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct HubSnapshot {
    /// Instance id → status word, one per live instance.
    pub statuses: Vec<StatusEntry>,
    /// Instance id → current task line (empty ones omitted).
    pub tasks: Vec<TaskEntry>,
    pub locks: Vec<LockEntry>,
    pub waiting: Vec<WaitEntry>,
    /// Newest-first tail of the conversation log.
    pub messages: Vec<MsgEntry>,
    /// Total queued (unread) hub messages across all inboxes.
    pub pending_messages: usize,
    /// Per-recipient breakdown of `pending_messages` (empty inboxes omitted).
    /// The total alone can't answer "how much of this is mail for a *muted*
    /// instance", which every unread badge has to subtract out.
    pub pending: Vec<PendingEntry>,
}

/// Queued (unread) hub messages sitting in one instance's inbox.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct PendingEntry {
    pub id: usize,
    pub count: usize,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct StatusEntry {
    pub id: usize,
    pub status: Status,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct TaskEntry {
    pub id: usize,
    pub task: String,
}

/// One open project as the frontend needs it to build a tab + one xterm per
/// restored session.
#[derive(Clone, Serialize)]
pub struct BootstrapInfo {
    pub handle: ProjectHandle,
    pub project_dir: String,
    pub project_name: String,
    pub sessions: Vec<SessionInfo>,
    /// Index into `sessions` of the initially focused instance.
    pub active: usize,
}

/// What `bootstrap` returns (and `projects-changed` carries): every open project
/// plus which one is active. Replaces the single-project startup payload.
#[derive(Clone, Serialize)]
pub struct WorkspaceInfo {
    pub projects: Vec<BootstrapInfo>,
    pub active: Option<ProjectHandle>,
}

/// Result of the startup `claude_status` probe: whether the user's `claude` CLI
/// was found, and the `PATH` we looked along (shown in the error so the user can
/// see what we searched).
#[derive(Clone, Serialize)]
pub struct ClaudeStatus {
    pub found: bool,
    pub path: Option<String>,
    pub searched_path: String,
}

/// `hub-update` event payload, now scoped to the project it describes.
#[derive(Clone, Serialize)]
pub struct HubUpdate {
    pub handle: ProjectHandle,
    pub snapshot: HubSnapshot,
}

/// `sessions-changed` event payload, scoped to its project.
#[derive(Clone, Serialize)]
pub struct SessionsChanged {
    pub handle: ProjectHandle,
    pub sessions: Vec<SessionInfo>,
}

/// `session-exited` event payload, scoped to its project.
#[derive(Clone, Serialize)]
pub struct SessionExited {
    pub handle: ProjectHandle,
    pub id: usize,
}
