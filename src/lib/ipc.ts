// Typed wrappers over the Tauri command/event/channel surface. Mirrors the Rust
// `snapshot.rs` types and `commands.rs` handlers.

import { invoke, Channel } from "@tauri-apps/api/core";

export type Status = "working" | "waiting" | "needs";

/** Stable id for one open project; matches the Rust `ProjectHandle` (u64). */
export type ProjectHandle = number;

export interface SessionInfo {
  id: number;
  name: string | null;
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
export interface HubSnapshot {
  statuses: StatusEntry[];
  tasks: TaskEntry[];
  locks: LockEntry[];
  waiting: WaitEntry[];
  messages: MsgEntry[];
  pending_messages: number;
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

/** Bind a session's PTY output stream (base64 chunks) to its xterm. */
export const attachSession = (
  projectHandle: ProjectHandle,
  id: number,
  channel: Channel<string>,
) => invoke<void>("attach_session", { projectHandle, id, channel });

export const createSession = (projectHandle: ProjectHandle) =>
  invoke<SessionInfo>("create_session", { projectHandle });

export const closeSession = (projectHandle: ProjectHandle, id: number) =>
  invoke<void>("close_session", { projectHandle, id });

export const renameSession = (
  projectHandle: ProjectHandle,
  id: number,
  name: string,
) => invoke<void>("rename_session", { projectHandle, id, name });

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
