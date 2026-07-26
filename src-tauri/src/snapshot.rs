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
}

/// One instance as the sidebar shows it.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct SessionInfo {
    pub id: usize,
    /// Custom name (from rename), if any — overrides the auto task line.
    pub name: Option<String>,
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

/// One record from the persistent cross-instance conversation log.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct MsgEntry {
    pub from: usize,
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
