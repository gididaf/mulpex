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
export interface MsgEntry {
  from: number;
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

export const resizeSession = (
  projectHandle: ProjectHandle,
  cols: number,
  rows: number,
) => invoke<void>("resize_session", { projectHandle, cols, rows });

export const focusSession = (projectHandle: ProjectHandle, id: number) =>
  invoke<void>("focus_session", { projectHandle, id });

export const getHubSnapshot = (projectHandle: ProjectHandle) =>
  invoke<HubSnapshot | null>("get_hub_snapshot", { projectHandle });

/** Relaunch the app through `AppHandle::restart` — the only restart path that
 * runs teardown (kills every project's `claude` process group, removes the
 * scratch root) before re-execing. Used to apply a downloaded update. */
export const restartApp = () => invoke<void>("restart_app");

export { Channel };
