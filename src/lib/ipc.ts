// Typed wrappers over the Tauri command/event/channel surface. Mirrors the Rust
// `snapshot.rs` types and `commands.rs` handlers.

import { invoke, Channel } from "@tauri-apps/api/core";

export type Status = "working" | "waiting" | "needs";

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
  project_dir: string;
  project_name: string;
  sessions: SessionInfo[];
  active: number;
}

export const currentProject = () =>
  invoke<BootstrapInfo | null>("current_project");

export const listRecentProjects = () =>
  invoke<string[]>("list_recent_projects");

export const openProject = (path: string) =>
  invoke<BootstrapInfo>("open_project", { path });

/** Bind a session's PTY output stream (base64 chunks) to its xterm. */
export const attachSession = (id: number, channel: Channel<string>) =>
  invoke<void>("attach_session", { id, channel });

export const createSession = () => invoke<SessionInfo>("create_session");

export const closeSession = (id: number) =>
  invoke<void>("close_session", { id });

export const renameSession = (id: number, name: string) =>
  invoke<void>("rename_session", { id, name });

export const sendBytes = (id: number, data: Uint8Array) =>
  invoke<void>("send_bytes", { id, data });

export const resizeSession = (cols: number, rows: number) =>
  invoke<void>("resize_session", { cols, rows });

export const focusSession = (id: number) =>
  invoke<void>("focus_session", { id });

export const getHubSnapshot = () =>
  invoke<HubSnapshot | null>("get_hub_snapshot");

export { Channel };
