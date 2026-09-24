// Typed wrappers over the Tauri command/event/channel surface. Mirrors the Rust
// `snapshot.rs` types and `commands.rs` handlers.

import { invoke, Channel } from "@tauri-apps/api/core";

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
}
export interface StatusEntry {
  id: number;
  status: Status;
  /** Ended its turn holding a watcher (hub listener, agentalk poll loop, a line
   *  of `watchers.txt`). NOT a status: such an instance is genuinely idle and
   *  `status` says `waiting`. Only the updater's busy guard reads it — a restart
   *  would kill the claude and drop whatever the watcher is attached to. */
  watching: boolean;
}
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
}

export interface ClaudeStatus {
  found: boolean;
  path: string | null;
  searched_path: string;
}

/** A turn explanation's three fixed parts. The summarizer answers exactly these
 *  three questions; the PANEL draws the Hebrew headings, so their wording and
 *  typography are ours and never Sonnet's. Absent when the output didn't parse
 *  into three parts, or on a question/plan entry — the panel then shows `text`
 *  as-is rather than faking a structure. Mirrors `snapshot.rs::ExplainSections`. */
export interface ExplainSections {
  /** "על מה אנחנו עובדים" — the goal, from the user's own recent prompts. */
  work: string;
  /** "מה עשיתי בסבב זה" — what this turn did. */
  did: string;
  /** "מה אני צריך ממך" — always answered, explicitly "nothing" when nothing. */
  need: string;
}

/** One item of an instance's Explainer feed: the short Hebrew explanation of
 *  one turn, question or plan. Up to 10 per instance, newest first in the
 *  store. `ok: false` marks a summarizer failure (the text says so — rendered
 *  dim, with a retry button). Mirrors `snapshot.rs::ExplainEntry` (no codegen;
 *  keep in sync). */
export interface ExplainEntry {
  id: number;
  /** Unix epoch milliseconds. A retry that replaced a failed entry carries the
   *  retry's time, which is also how the panel notices its row came back. */
  ts: number;
  /** The summarizer's raw output: what the panel shows when `sections` is
   *  null, and where a failure's reason lives. */
  text: string;
  sections: ExplainSections | null;
  ok: boolean;
  /** "turn" explains a finished turn; "question" explains a pending
   *  AskUserQuestion (what's being asked + what each option means); "plan"
   *  explains a pending ExitPlanMode plan in one line, before you approve it. */
  kind: "turn" | "question" | "plan";
  /** Unique, stable id of this feed item: the retry address of a failed entry,
   *  and what an incoming `explain-update` matches to replace a row in place
   *  instead of prepending a new one. */
  seq: number;
}

// Scoped event payloads (mirror snapshot.rs).
export interface HubUpdateEvent {
  handle: ProjectHandle;
  snapshot: HubSnapshot;
}
export interface ExplainUpdateEvent {
  handle: ProjectHandle;
  id: number;
  entry: ExplainEntry;
}
/** Fires on transitions only: the Explainer started (active) or finished
 *  (summary, failure, or skip — all end it) working on instance `id`. */
export interface ExplainPendingEvent {
  handle: ProjectHandle;
  id: number;
  active: boolean;
}
/** `save-progress`: one step of a ⌘S save. `detail` is the path on `done` and
 *  the reason on `error`. */
export type SaveState = "writing" | "checking" | "fixing" | "done" | "error";
export interface SaveProgressEvent {
  handle: ProjectHandle;
  id: number;
  state: SaveState;
  detail: string | null;
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
export const bootstrap = () => invoke<WorkspaceInfo>("bootstrap");

export const listRecentProjects = () =>
  invoke<string[]>("list_recent_projects");

/** Whether the user's `claude` CLI was found (checked once, at startup). */
export const claudeStatus = () => invoke<ClaudeStatus>("claude_status");

/** Open (or re-activate) a project; returns its bootstrap info. */
export const openProject = (path: string) =>
  invoke<BootstrapInfo>("open_project", { path });

/** Close a project (kills its sessions); returns the new workspace. */
export const closeProject = (projectHandle: ProjectHandle) =>
  invoke<WorkspaceInfo>("close_project", { projectHandle });

/** Make a project the active one on the backend. */
export const switchProject = (projectHandle: ProjectHandle) =>
  invoke<void>("switch_project", { projectHandle });

/** Commit a new tab order after a drag. The backend persists it to `open.txt`,
 *  so the arrangement survives relaunch (and ⌘1–⌘9 follow it). */
export const reorderProjects = (handles: ProjectHandle[]) =>
  invoke<void>("reorder_projects", { handles });

/** Bind a session's PTY output stream (base64 chunks) to its xterm. */
export const attachSession = (
  projectHandle: ProjectHandle,
  id: number,
  channel: Channel<string>,
) => invoke<void>("attach_session", { projectHandle, id, channel });

export const createSession = (projectHandle: ProjectHandle) =>
  invoke<SessionInfo>("create_session", { projectHandle });

/** Open a plain shell terminal (⌘⇧T) in the project dir. */
export const createTerminal = (projectHandle: ProjectHandle) =>
  invoke<SessionInfo>("create_terminal", { projectHandle });

export const closeSession = (projectHandle: ProjectHandle, id: number) =>
  invoke<void>("close_session", { projectHandle, id });

/** Restart one claude in place (⌘⇧R): the backend kills it and relaunches on the
 *  same row with `--resume`, so it re-reads its environment while keeping its
 *  conversation, number, name and inbox. Rejects (having killed nothing) when
 *  that instance has no transcript to resume yet — the caller shows the reason.
 *  The PTY behind the row is a different process afterwards, so the pane has to
 *  be rebound with `terminals.reattach`. */
export const restartSession = (projectHandle: ProjectHandle, id: number) =>
  invoke<void>("restart_session", { projectHandle, id });

/** Commit a new sidebar order after a session-row drag. The backend persists it
 *  to the session store, so the arrangement survives relaunch (and ⌘[ / ⌘]
 *  follow it). */
export const reorderSessions = (projectHandle: ProjectHandle, ids: number[]) =>
  invoke<void>("reorder_sessions", { projectHandle, ids });

export const renameSession = (
  projectHandle: ProjectHandle,
  id: number,
  name: string,
) => invoke<void>("rename_session", { projectHandle, id, name });

/** Mute/unmute a session (⌘M or the sidebar 🔇); persists across restarts. */
export const setSessionMuted = (
  projectHandle: ProjectHandle,
  id: number,
  muted: boolean,
) => invoke<void>("set_session_muted", { projectHandle, id, muted });

/** Tick/untick the "Mute Session" menu item for the focused session. */
export const setMuteMenuChecked = (checked: boolean) =>
  invoke<void>("set_mute_menu_checked", { checked });

export const sendBytes = (
  projectHandle: ProjectHandle,
  id: number,
  data: Uint8Array,
) => invoke<void>("send_bytes", { projectHandle, id, data });

/** Bring every PTY in every open project to the center pane's geometry. One call
 *  for the whole workspace — see `resize_terminals` in commands.rs. */
export const resizeTerminals = (cols: number, rows: number) =>
  invoke<void>("resize_terminals", { cols, rows });

export const focusSession = (projectHandle: ProjectHandle, id: number) =>
  invoke<void>("focus_session", { projectHandle, id });

export const getHubSnapshot = (projectHandle: ProjectHandle) =>
  invoke<HubSnapshot | null>("get_hub_snapshot", { projectHandle });

/** A project's whole Explainer feed, for the initial paint (thereafter pushed
 *  via `explain-update`). Flat; per instance newest first. */
export const getExplains = (projectHandle: ProjectHandle) =>
  invoke<ExplainEntry[]>("get_explains", { projectHandle });

/** Re-run the summarizer for one failed entry; its result replaces that row in
 *  place (an ordinary `explain-update` carrying the same `seq`). False means the
 *  backend has nothing stashed under that seq any more — the entry aged out of
 *  the 10-entry feed, or its instance is gone — and the button should come back. */
export const retryExplain = (
  projectHandle: ProjectHandle,
  id: number,
  seq: number,
) => invoke<boolean>("retry_explain", { projectHandle, id, seq });

/** Save one claude's work as a handoff doc in the repo (⌘S, and the row's retry).
 *  Resolves once the save is under way — progress arrives as `save-progress` —
 *  and rejects with the reason when there is nothing to save or one is already
 *  running for that instance. */
export const saveSession = (projectHandle: ProjectHandle, id: number) =>
  invoke<void>("save_session", { projectHandle, id });

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
  invoke<SaveEntry[]>("list_saves", { projectHandle });

/** Delete one save file. */
export const deleteSave = (projectHandle: ProjectHandle, file: string) =>
  invoke<void>("delete_save", { projectHandle, file });

/** Start a claude on a save. `fresh`: it reads the doc, checks the repo, reports
 *  and waits. `continue`: resume the conversation that last saved it (or hand
 *  back its row if it is already open). Named after the save's title. */
export const loadSave = (
  projectHandle: ProjectHandle,
  file: string,
  mode: "fresh" | "continue",
) => invoke<LoadResult>("load_save", { projectHandle, file, mode });

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
  invoke<GuideEntry[]>("list_guides", { projectHandle });

/** Retire a guide: deletes its runbook AND its pointer. */
export const deleteGuide = (projectHandle: ProjectHandle, file: string) =>
  invoke<void>("delete_guide", { projectHandle, file });

/** Start a claude on a guide: it reads it and asks what the user needs. */
export const loadGuide = (projectHandle: ProjectHandle, file: string) =>
  invoke<SessionInfo>("load_guide", { projectHandle, file });

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
  invoke<ImportState>("import_start", { projectHandle });
export const importState = (projectHandle: ProjectHandle) =>
  invoke<ImportState | null>("import_state", { projectHandle });
export const importDiscard = (projectHandle: ProjectHandle) =>
  invoke<void>("import_discard", { projectHandle });
export const importApply = (projectHandle: ProjectHandle, decisions: ImportDecision[]) =>
  invoke<ApplyReport>("import_apply", { projectHandle, decisions });

/** Relaunch the app through `AppHandle::restart` — the only restart path that
 * runs teardown (kills every project's `claude` process group, removes the
 * scratch root) before re-execing. Used to apply a downloaded update. */
export const restartApp = () => invoke<void>("restart_app");

export { Channel };
