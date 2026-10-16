// Typed wrappers over the Tauri command/event/channel surface. Mirrors the Rust
// `snapshot.rs` types and `commands.rs` handlers.

import { invoke, Channel } from "@tauri-apps/api/core";
import { slotOf } from "./hosts";

/**
 * Every command goes through here. A remote project's handle (`hosts.ts`)
 * sends the call to that host instead — the same command, run there by
 * `remote/rpc.rs`, with the handle translated on the way (`client.rs`) — so
 * no caller has to know whether a project is local. `host` routes a call that
 * names no project (recents, a secrets set by name) to a given host.
 */
function call<T>(cmd: string, args: Record<string, unknown> = {}, host?: number): Promise<T> {
  const h = args.projectHandle;
  const slot = host ?? (typeof h === "number" ? slotOf(h) : 0);
  if (slot > 0) return invoke<T>("host_invoke", { host: slot, cmd, args });
  return invoke<T>(cmd, args);
}

export type Status = "working" | "waiting" | "needs";

/** Stable id for one open project; matches the Rust `ProjectHandle` (u64). */
export type ProjectHandle = number;

/** What a session runs. Terminals share the list, the id space and every
 *  (project, id)-keyed mechanism with instances. */
export type SessionKind = "claude" | "shell";

export interface SessionInfo {
  id: number;
  name: string | null;
  /** Muted (⌘M): dimmed, sorted last, and left out of every attention badge.
   *  Purely presentational — the instance runs and coordinates as normal. */
  muted: boolean;
  /** The instance that `hub_spawn`ed this one, while it is open — the sidebar
   *  nests the row under it. `null` for a top-level row. */
  parent: number | null;
  /** Its family is folded away in the sidebar. */
  collapsed: boolean;
  /** Auto-compact every claude in this row's sidebar subtree at this context %
   *  (set by the user from the row's menu; `compact.ts` enforces it). */
  auto_compact: number | null;
  kind: SessionKind;
  /** A terminal whose shell has exited. Kept in the list (unlike a dead
   *  instance, which is removed) so its output stays readable until closed. */
  exited: boolean;
  /** Why this instance never started, if it didn't (it died within seconds of
   *  spawning). Such a row is kept rather than reaped so the reason stays on
   *  screen. It is absent from `statuses`, so the sidebar MUST check this
   *  before falling back to a status dot — the unknown-id default is "ready",
   *  which would paint a dead session green. */
  failed: string | null;
  /** The To Do list is ON for this claude (right-click toggle; off by default). */
  todos_on: boolean;
  /** "To Do" (`todos.ts`): open items on this claude's list, and how many of
   *  the user's edits wait for UPDATE. 0 for a terminal. */
  todos_open: number;
  todos_pending: number;
  /** Items with a claude comment the user has not opened yet (💬). */
  todos_unread: number;
  /** Open items marked urgent — the badge turns red. */
  todos_urgent: number;
}
export interface StatusEntry {
  id: number;
  status: Status;
  /** Ended its turn holding a watcher (hub listener, agentalk poll loop, a line
   *  of `watchers.txt`). NOT a status: such an instance is genuinely idle and
   *  `status` says `waiting`. Only the updater's busy guard reads it — a restart
   *  would kill the claude and drop whatever the watcher is attached to. */
  watching: boolean;
  /** Context window used, whole percent (Claude Code's own number, via the
   *  statusline). Null until the first reply. */
  ctx_pct: number | null;
  /** The ⌘⇧K compact request's phase, as on disk (`compactreq/<id>`). */
  compact_phase: CompactPhase | null;
  /** Between `PreCompact` and the `SessionStart` that ends it. */
  compacting: boolean;
}
export type CompactPhase = "requested" | "compacting" | "resume";
export interface TaskEntry {
  id: number;
  task: string;
}
export interface LockEntry {
  path: string;
  holder: number;
}
export interface WaitEntry {
  id: number;
  file: string;
  holder: number;
}
/** Both ends are *addresses*: `claude#2` in this project, `all` for a broadcast,
 *  `central-one#3` for an instance in another open project. `from` was a bare
 *  number until a sender could live in a different project — there was nowhere
 *  to put the project. Mirrors `snapshot.rs::MsgEntry` (no codegen; keep in sync). */
export interface MsgEntry {
  from: string;
  to: string;
  body: string;
  ts: number;
  /** Still in a live recipient's inbox (not yet `hub_inbox`ed). */
  unread: boolean;
}
export interface PendingEntry {
  id: number;
  count: number;
}
export interface HubSnapshot {
  statuses: StatusEntry[];
  tasks: TaskEntry[];
  locks: LockEntry[];
  waiting: WaitEntry[];
  messages: MsgEntry[];
  pending_messages: number;
  /** Per-recipient breakdown of `pending_messages`; empty inboxes omitted. */
  pending: PendingEntry[];
}
export interface BootstrapInfo {
  handle: ProjectHandle;
  project_dir: string;
  project_name: string;
  sessions: SessionInfo[];
  active: number;
}
export interface WorkspaceInfo {
  projects: BootstrapInfo[];
  active: ProjectHandle | null;
  /** The geometry every PTY is running at. Terminals must be built at exactly
   *  this size before they are attached — see `terminals.ts::create`. */
  cols: number;
  rows: number;
  /** The remote desktop driving that geometry, or null when this window does. */
  geometry_owner?: string | null;
}

export interface ClaudeStatus {
  found: boolean;
  path: string | null;
  searched_path: string;
}

// Scoped event payloads (mirror snapshot.rs).
export interface HubUpdateEvent {
  handle: ProjectHandle;
  snapshot: HubSnapshot;
}
/** `save-progress`: one step of a ⌘S save. `detail` is the path on `done` and
 *  the reason on `error`; `title` is the save's title on `done`. */
export type SaveState = "writing" | "checking" | "fixing" | "done" | "error";
export interface SaveProgressEvent {
  handle: ProjectHandle;
  id: number;
  state: SaveState;
  detail: string | null;
  title: string | null;
}
/** `explain-progress`: the next piece of an explanation (`delta`), its end
 *  (`done`), or why it failed (`error`, reason in `text`). */
export interface ExplainProgressEvent {
  handle: ProjectHandle;
  id: number;
  reqId: string;
  kind: "delta" | "done" | "error";
  text: string;
}
export interface SessionsChangedEvent {
  handle: ProjectHandle;
  sessions: SessionInfo[];
}
export interface SessionExitedEvent {
  handle: ProjectHandle;
  id: number;
}

/** Every open project + active handle, for the initial paint. */
export const bootstrap = (host = 0) => call<WorkspaceInfo>("bootstrap", {}, host);

export const listRecentProjects = (host = 0) =>
  call<string[]>("list_recent_projects", {}, host);

/** Whether the user's `claude` CLI was found (checked once, at startup). */
export const claudeStatus = (host = 0) => call<ClaudeStatus>("claude_status", {}, host);

/** One folder's subfolders on `host` — the remote project picker's browser
 *  (a remote disk has no native folder dialog). Empty `path`: the home folder. */
export interface DirListing {
  path: string;
  parent: string | null;
  dirs: string[];
}
export const browseDir = (path: string, host: number) => call<DirListing>("browse_dir", { path }, host);

/** Whether Mulpex has Full Disk Access (applies only after a relaunch). */
export const hasFullDiskAccess = () => invoke<boolean>("has_full_disk_access");

/** Open System Settings at Privacy & Security ▸ Full Disk Access. */
export const openFullDiskAccessSettings = () =>
  invoke<void>("open_full_disk_access_settings");

/** Open (or re-activate) a project; returns its bootstrap info. */
export const openProject = (path: string, host = 0) =>
  call<BootstrapInfo>("open_project", { path }, host);

/** Close a project (kills its sessions); returns the new workspace. */
export const closeProject = (projectHandle: ProjectHandle) =>
  call<WorkspaceInfo>("close_project", { projectHandle });

/** Make a project the active one on the backend. */
export const switchProject = async (projectHandle: ProjectHandle) => {
  // A remote host's own window picks what it shows; never moved from here.
  if (slotOf(projectHandle) === 0) await invoke<void>("switch_project", { projectHandle });
};

/** Commit a new tab order after a drag. The backend persists it to `open.txt`,
 *  so the arrangement survives relaunch (and ⌘1–⌘9 follow it). */
export const reorderProjects = (handles: ProjectHandle[]) =>
  call<void>("reorder_projects", { handles }, handles.length ? slotOf(handles[0]) : 0);

/** Bind a session's PTY output stream (base64 chunks) to its xterm. */
export const attachSession = (
  projectHandle: ProjectHandle,
  id: number,
  channel: Channel<string>,
) => invoke<void>("attach_session", { projectHandle, id, channel });

export const createSession = (projectHandle: ProjectHandle) =>
  call<SessionInfo>("create_session", { projectHandle });

/** Open a plain shell terminal (⌘⇧T) in the project dir. */
export const createTerminal = (projectHandle: ProjectHandle) =>
  call<SessionInfo>("create_terminal", { projectHandle });

/** Rejects with the reason while a ⌘S save of that row is running. */
export const closeSession = (projectHandle: ProjectHandle, id: number) =>
  call<void>("close_session", { projectHandle, id });

/** Restart one claude in place (⌘⇧R): the backend kills it and relaunches on the
 *  same row with `--resume`, so it re-reads its environment while keeping its
 *  conversation, number, name and inbox. Rejects (having killed nothing) when
 *  that instance has no transcript to resume yet — the caller shows the reason.
 *  The PTY behind the row is a different process afterwards, so the pane has to
 *  be rebound with `terminals.reattach`. */
export const restartSession = (projectHandle: ProjectHandle, id: number) =>
  call<void>("restart_session", { projectHandle, id });

/** Commit a new sidebar order after a session-row drag. The backend persists it
 *  to the session store, so the arrangement survives relaunch (and ⌘[ / ⌘]
 *  follow it). */
export const reorderSessions = (projectHandle: ProjectHandle, ids: number[]) =>
  call<void>("reorder_sessions", { projectHandle, ids });

/** Drag-to-nest: move a claude (with its family) under `parent`, or to the top
 *  level with `null`, committing the sidebar order `ids` with it. Resolves
 *  false when the backend refused (a terminal, or a cycle). */
export const reparentSession = (
  projectHandle: ProjectHandle,
  id: number,
  parent: number | null,
  ids: number[],
) => call<boolean>("reparent_session", { projectHandle, id, parent, ids });

export const renameSession = (
  projectHandle: ProjectHandle,
  id: number,
  name: string,
) => call<void>("rename_session", { projectHandle, id, name });

/** Fold/unfold a row's `hub_spawn` family in the sidebar; persists. */
export const setSessionCollapsed = (
  projectHandle: ProjectHandle,
  id: number,
  collapsed: boolean,
) => call<void>("set_session_collapsed", { projectHandle, id, collapsed });

/** Set or (null) clear a row's auto-compact rule; persists, and the backend
 *  tells that claude once. False if nothing changed. */
export const setAutoCompact = (projectHandle: ProjectHandle, id: number, pct: number | null) =>
  call<boolean>("set_auto_compact", { projectHandle, id, pct });

/** Mute/unmute a session (⌘M or the sidebar 🔇); persists across restarts. */
export const setSessionMuted = (
  projectHandle: ProjectHandle,
  id: number,
  muted: boolean,
) => call<void>("set_session_muted", { projectHandle, id, muted });

/** The project's saved ⌘⇧P pins, by instance id (`pins.rs`). Pin data is the
 *  frontend's own (`pins.ts`); the backend stores it opaque. */
export const getPins = (projectHandle: ProjectHandle) =>
  call<Record<string, unknown>>("get_pins", { projectHandle });

export const setPin = (projectHandle: ProjectHandle, id: number, pin: unknown | null) =>
  call<void>("set_pin", { projectHandle, id, pin });

// ── "To Do" lists (src-tauri/src/todos.rs) ─────────────────────────────────

export type TodoKind = "action" | "decision" | "question";

export interface TodoItem {
  n: number;
  kind: TodoKind;
  title: string;
  detail: string | null;
  choices: string[];
  status: "open" | "done";
  done_by: "claude" | "user" | null;
  author: "claude" | "user";
  /** The follow-up thread, oldest first; append-only. */
  comments: TodoComment[];
  /** How many of `comments` the user has seen (opened the thread). */
  seen: number;
  chosen: string | null;
  created: number;
  updated: number;
  rev: number;
  /** The instance this ask is really for — a worker whose need an orchestrator
   *  collected onto its own list. */
  for: number | null;
  /** Set by the claude only; shown on top, in red. */
  urgent: boolean;
}

export interface TodoComment {
  by: "claude" | "user";
  text: string;
  at: number;
}

/** The user's draft over item `n` — what it should become at UPDATE. */
export interface TodoDraft {
  done: boolean | null;
  /** The user's reply, appended to the thread at UPDATE. One per item. */
  comment: string | null;
  chosen: string | null;
  delete: boolean;
  base_rev: number;
  /** Set when the claude removed the item while this draft had words on it. */
  orphan: { kind: TodoKind; title: string } | null;
}

export interface TodoAdded {
  tmp: string;
  kind: TodoKind;
  title: string;
  detail: string | null;
}

export interface TodoList {
  next: number;
  items: TodoItem[];
  drafts: { edits: Record<string, TodoDraft>; added: TodoAdded[] };
  /** An UPDATE is queued, waiting for the claude to be able to hear it. */
  sending: boolean;
}

export type TodoEdit =
  | { op: "done"; n: number; done: boolean }
  | { op: "comment"; n: number; text: string }
  | { op: "seen"; n: number }
  | { op: "choose"; n: number; choice: string | null }
  | { op: "dismiss"; n: number; on: boolean }
  | { op: "add"; kind: TodoKind; title: string; detail?: string | null }
  | { op: "edit_added"; tmp: string; kind?: TodoKind; title?: string; detail?: string }
  | { op: "remove_added"; tmp: string }
  | { op: "discard"; n: number }
  | { op: "discard_all" };

export interface TodoSend {
  id: number;
  state: "queued" | "refused";
  why: string | null;
}

export const todosGet = (projectHandle: ProjectHandle) =>
  call<Record<string, TodoList>>("todos_get", { projectHandle });

export const todosEdit = (projectHandle: ProjectHandle, id: number, edit: TodoEdit) =>
  call<TodoList>("todos_edit", { projectHandle, id, edit });

export const setTodosEnabled = (projectHandle: ProjectHandle, id: number, on: boolean) =>
  call<boolean>("set_todos_enabled", { projectHandle, id, on });

export const todosUpdate = (projectHandle: ProjectHandle, ids: number[]) =>
  call<TodoSend[]>("todos_update", { projectHandle, ids });

/** Tick/untick the "Mute Session" menu item for the focused session. */
export const setMuteMenuChecked = (checked: boolean) =>
  invoke<void>("set_mute_menu_checked", { checked });

/** ⌘⇧K: set or (null) clear claude#id's compact-request phase on disk. */
export const setCompactPhase = async (
  projectHandle: ProjectHandle,
  id: number,
  phase: CompactPhase | null,
) => {
  // A remote host's ⌘⇧K runs in its own window (`compact.ts` there).
  if (slotOf(projectHandle) === 0) await invoke<void>("set_compact_phase", { projectHandle, id, phase });
};

/** Keystrokes for a session; a remote one's go straight to its host's PTY. */
export const sendBytes = (
  projectHandle: ProjectHandle,
  id: number,
  data: Uint8Array,
) => {
  const slot = slotOf(projectHandle);
  if (slot > 0) return invoke<void>("host_input", { host: slot, projectHandle, id, data: Array.from(data) });
  return invoke<void>("send_bytes", { projectHandle, id, data });
};

export interface SecretRow {
  key: string;
  value: string;
}

/** ⌘K: write a one-off 0600 secrets file for claude#id; resolves to its path. */
export const secretsCreateEphemeral = (
  projectHandle: ProjectHandle,
  id: number,
  rows: SecretRow[],
) => call<string>("secrets_create_ephemeral", { projectHandle, id, rows });

/** A saved set as listed: never its values. */
export interface SavedSet {
  name: string;
  keys: string[];
  project_only: boolean;
}

/** Saved sets usable in this project (untagged ones + this project's). */
export const secretsList = (projectHandle: ProjectHandle) =>
  call<SavedSet[]>("secrets_list", { projectHandle });

/** Save a new set and, if `send`, hand it to claude#id; resolves to its keys. */
export const secretsSave = (
  projectHandle: ProjectHandle,
  id: number,
  name: string,
  projectOnly: boolean,
  rows: SecretRow[],
  send: boolean,
) => call<string[]>("secrets_save", { projectHandle, id, name, projectOnly, rows, send });

/** A saved set's rows, values included — only for the edit form. */
/** Mulpex ▸ Live Token… (`live_token.rs`). `host` is the machine whose
 *  settings these are: 0 for this one, a slot for a remote host. */
export interface LiveTokenStatus {
  token_file: string;
  refresh_command: string;
  running: boolean;
  token_problem: string | null;
}
export const liveTokenGet = (host: number) => call<LiveTokenStatus>("live_token_get", {}, host);
export const liveTokenSet = (host: number, tokenFile: string, refreshCommand: string) =>
  call<LiveTokenStatus>("live_token_set", { tokenFile, refreshCommand }, host);

export const secretsGet = (name: string, projectHandle: ProjectHandle) =>
  call<{ rows: SecretRow[]; project_only: boolean }>("secrets_get", { name }, slotOf(projectHandle));

/** Replace a saved set's rows; `projectOnly` re-tags it to this project. */
export const secretsUpdate = (
  projectHandle: ProjectHandle,
  name: string,
  projectOnly: boolean,
  rows: SecretRow[],
) => call<void>("secrets_update", { projectHandle, name, projectOnly, rows });

export const secretsDelete = (name: string, projectHandle: ProjectHandle) =>
  call<void>("secrets_delete", { name }, slotOf(projectHandle));

/** Hand saved set `name` to claude#id; resolves to its keys. */
export const secretsAttach = (projectHandle: ProjectHandle, id: number, name: string) =>
  call<string[]>("secrets_attach", { projectHandle, id, name });

/** Bring every PTY in every open project to the center pane's geometry. One call
 *  for the whole workspace — see `resize_terminals` in commands.rs. */
export const resizeTerminals = (cols: number, rows: number) =>
  invoke<void>("resize_terminals", { cols, rows });

export const focusSession = async (projectHandle: ProjectHandle, id: number) => {
  if (slotOf(projectHandle) === 0) await invoke<void>("focus_session", { projectHandle, id });
};

/** "Take back" on the banner a remote desktop's geometry lease puts up. */
export const geometryTakeBack = () => invoke<void>("geometry_take_back");

/** This window's xterm answering a `host-serialize` (`remote/desktop.rs`). */
export const remoteSnapshotReply = (rid: string, data: string | null, offset: number, cols: number, rows: number) =>
  invoke<void>("remote_snapshot_reply", { rid, data, offset, cols, rows });

export const getHubSnapshot = (projectHandle: ProjectHandle) =>
  call<HubSnapshot | null>("get_hub_snapshot", { projectHandle });

/** Why ⌘S can't save claude#id right now, or null. Asked before the confirm. */
export const saveCheck = (projectHandle: ProjectHandle, id: number) =>
  call<string | null>("save_check", { projectHandle, id });

/** Save one claude's work as a handoff doc in the repo (⌘S, and the row's retry).
 *  Stops the claude and holds its row. Resolves once the save is under way —
 *  progress arrives as `save-progress` — and rejects with the reason it was
 *  refused (see `saveCheck`) or when one is already running. */
export const saveSession = (projectHandle: ProjectHandle, id: number) =>
  call<void>("save_session", { projectHandle, id });

/** ⌘⇧E: explain `selection` from claude#id's pane in the side panel, in a fresh
 *  fork of its conversation (`explain.rs`). Resolves once the fork is running;
 *  the answer streams as `explain-progress` events carrying `reqId`. Rejects
 *  when there is no conversation to fork yet. */
export const explainStart = (
  projectHandle: ProjectHandle,
  id: number,
  reqId: string,
  selection: string,
) => call<void>("explain_start", { projectHandle, id, reqId, selection });

/** A follow-up question in claude#id's panel, answered by its fork. */
export const explainFollowup = (
  projectHandle: ProjectHandle,
  id: number,
  reqId: string,
  question: string,
) => call<void>("explain_followup", { projectHandle, id, reqId, question });

/** Claude#id's panel closed: stop its explain and delete its fork. */
export const explainClose = (projectHandle: ProjectHandle, id: number) =>
  call<void>("explain_close", { projectHandle, id });

/** One save in the repo's `mulpex/saves/`, as the ⌘L list shows it. `file` is
 *  the bare file name — the only handle passed back to the backend. */
export interface SaveEntry {
  file: string;
  title: string;
  description: string;
  author: string;
  created: string;
  updated: string;
  /** The conversation that last saved it still exists on this machine, in this
   *  project — ⌘L offers "Continue conversation". */
  continuable: boolean;
  /** That conversation is already open as this claude; continuing focuses it. */
  open_as: number | null;
}

/** What `loadSave` did: started `info`, or (`existing`) found that conversation
 *  already open as `info` and the caller should just focus it. */
export interface LoadResult {
  info: SessionInfo;
  existing: boolean;
}

/** The saves of the project's repo, most recently updated first. */
export const listSaves = (projectHandle: ProjectHandle) =>
  call<SaveEntry[]>("list_saves", { projectHandle });

/** Delete one save file. */
export const deleteSave = (projectHandle: ProjectHandle, file: string) =>
  call<void>("delete_save", { projectHandle, file });

/** Start a claude on a save. `fresh`: it reads the doc, checks the repo, reports
 *  and waits. `continue`: resume the conversation that last saved it (or hand
 *  back its row if it is already open). Named after the save's title. */
export const loadSave = (
  projectHandle: ProjectHandle,
  file: string,
  mode: "fresh" | "continue",
) => call<LoadResult>("load_save", { projectHandle, file, mode });

/** One guide pointer in the repo's `mulpex/guides/` — a recurring-incident
 *  runbook that stays where it is (`source`, relative to the repo root). */
export interface GuideEntry {
  file: string;
  title: string;
  description: string;
  source: string;
  /** The runbook is not on disk any more. */
  missing: boolean;
}

export const listGuides = (projectHandle: ProjectHandle) =>
  call<GuideEntry[]>("list_guides", { projectHandle });

/** Retire a guide: deletes its runbook AND its pointer. */
export const deleteGuide = (projectHandle: ProjectHandle, file: string) =>
  call<void>("delete_guide", { projectHandle, file });

/** Start a claude on a guide: it reads it and asks what the user needs. */
export const loadGuide = (projectHandle: ProjectHandle, file: string) =>
  call<SessionInfo>("load_guide", { projectHandle, file });

// ---- Import Docs (docs_import.rs) ----

export type ImportKind = "save" | "guide" | "stale" | "skip";

/** One markdown file of an import. `status` is `pending` while Sonnet sorts it. */
export interface ImportItem {
  file: string;
  status: "pending" | "done" | "error";
  kind: ImportKind | "";
  slug: string;
  title: string;
  description: string;
  reason: string;
  error: string | null;
}
export interface ImportState {
  running: boolean;
  items: ImportItem[];
}
export interface ImportDecision {
  file: string;
  kind: ImportKind;
  slug: string;
  title: string;
  description: string;
}
export interface ApplyReport {
  saves: number;
  guides: number;
  deleted: number;
  skipped: number;
  errors: string[];
  /** `<short hash> <subject>` of the one commit Apply made. */
  commit: string | null;
  /** Why no commit was made (a hook failed…); the files are changed anyway. */
  commit_error: string | null;
  /** Places still naming a deleted doc that were not safe to edit. */
  leftovers: string[];
}

/** Start the project's import, or get the one already running / in review.
 *  Progress arrives as `import-update` (payload: the project handle). */
export const importStart = (projectHandle: ProjectHandle) =>
  call<ImportState>("import_start", { projectHandle });
export const importState = (projectHandle: ProjectHandle) =>
  call<ImportState | null>("import_state", { projectHandle });
export const importDiscard = (projectHandle: ProjectHandle) =>
  call<void>("import_discard", { projectHandle });
export const importApply = (projectHandle: ProjectHandle, decisions: ImportDecision[]) =>
  call<ApplyReport>("import_apply", { projectHandle, decisions });

/** Relaunch the app through `AppHandle::restart` — the only restart path that
 * runs teardown (kills every project's `claude` process group, removes the
 * scratch root) before re-execing. Used to apply a downloaded update. */
export const restartApp = () => invoke<void>("restart_app");

/** Quit through the normal teardown — the end of ⌘⇧Q Checkpoint & Quit. */
export const quitApp = () => invoke<void>("quit_app");

/** ⌘⇧B Ask All Claudes (`broadcast.rs`): one claude that can be asked. */
export interface BroadcastTarget {
  handle: ProjectHandle;
  project: string;
  id: number;
  name: string | null;
  status: Status;
}
/** A row's state: `sent` (unread), `busy` (read, still working or asking),
 *  `read` (read, idle, settling), then a final one — `replied` (via the tool),
 *  `fallback` (its last message, it never called the tool), `noanswer`, `gone`. */
export type BroadcastRowState = "sent" | "busy" | "read" | "replied" | "fallback" | "noanswer" | "gone";
export interface BroadcastRow {
  handle: ProjectHandle;
  project: string;
  id: number;
  name: string | null;
  state: BroadcastRowState;
  reply: string | null;
}
export interface BroadcastRound {
  bid: string;
  message: string;
  rows: BroadcastRow[];
  complete: boolean;
}
/** `broadcast-summary`: the next piece of the summary, its end, or why it failed. */
export interface BroadcastSummaryEvent {
  reqId: string;
  kind: "delta" | "done" | "error";
  text: string;
}
export const broadcastTargets = () => invoke<BroadcastTarget[]>("broadcast_targets");
export const broadcastSend = (message: string, targets: { handle: ProjectHandle; id: number }[]) =>
  invoke<string>("broadcast_send", { message, targets });
export const broadcastProgress = () => invoke<BroadcastRound | null>("broadcast_progress");
export const broadcastSummarize = (reqId: string) => invoke<void>("broadcast_summarize", { reqId });
export const broadcastCancel = () => invoke<void>("broadcast_cancel");

/** ⌘⇧Q Checkpoint & Quit (`checkpoint.rs`): one claude asked to stop, or to wake. */
export interface CheckpointTarget {
  handle: ProjectHandle;
  project: string;
  id: number;
  name: string | null;
}

/** What only the backend knows about a claude asked to stop; its status word
 * comes from the hub snapshot like everywhere else. */
export interface CheckpointProgress {
  handle: ProjectHandle;
  id: number;
  /** Still running. One that exited has stopped, trivially. */
  alive: boolean;
  /** The stop message is still in its inbox. */
  unread: boolean;
  /** Its hub listener is running — an idle claude without one never reads it. */
  listener: boolean;
}

export const checkpointStop = () => invoke<CheckpointTarget[]>("checkpoint_stop");
export const checkpointProgress = () => invoke<CheckpointProgress[]>("checkpoint_progress");
export const checkpointCancel = () => invoke<void>("checkpoint_cancel");
export const checkpointCommit = () => invoke<void>("checkpoint_commit");
export const checkpointPending = () => invoke<CheckpointTarget[]>("checkpoint_pending");
export const checkpointWake = () => invoke<number>("checkpoint_wake");
export const checkpointDismiss = () => invoke<void>("checkpoint_dismiss");

/** ⌘⇧O Remote Control (`remote.rs`). Also the `remote-changed` event payload. */
export interface RemoteStatus {
  enabled: boolean;
  relay_url: string;
  /** What the dialog's Reset button puts back. */
  default_relay_url: string;
  state: "off" | "connecting" | "connected" | "error";
  error: string | null;
  /** Phones connected right now. */
  clients: number;
  /** What a NEW phone opens to pair (one use, 10 minutes); the QR code
   *  encodes exactly this. Empty while off. */
  pair_url: string;
  qr_svg: string;
  devices: RemoteDevice[];
}
export interface RemoteDevice {
  id: string;
  name: string;
  /** Unix seconds. */
  added: number;
  online: boolean;
  /** Gets notifications. */
  push: boolean;
  /** `desktop`: another Mulpex that paired to drive this one. */
  kind: "phone" | "desktop";
}
/** Answer a phone's typing request (`remote-type`). */
export const remoteReply = (rid: string, ok: boolean, why: string | null, id: number | null = null) =>
  invoke<void>("remote_reply", { rid, ok, why, id });
export const remoteRevokeDevice = (id: string) =>
  invoke<RemoteStatus>("remote_revoke_device", { id });
export const remoteStatus = () => invoke<RemoteStatus>("remote_status");
export const remoteSetEnabled = (on: boolean) =>
  invoke<RemoteStatus>("remote_set_enabled", { on });
export const remoteSetRelayUrl = (url: string) =>
  invoke<RemoteStatus>("remote_set_relay_url", { url });

export { Channel };
