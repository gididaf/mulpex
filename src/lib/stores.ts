// The reactive projection of backend state. PTY bytes bypass these entirely
// (they go straight to xterm via each session's Channel); only the sidebar/hub
// surface lives here.

import { writable } from "svelte/store";
import type { BootstrapInfo, HubSnapshot, SessionInfo, Status } from "./ipc";

/** Ordered live sessions (matches sidebar + persistence order). */
export const sessions = writable<SessionInfo[]>([]);

/** id → status word, from the hub poll. */
export const statuses = writable<Map<number, Status>>(new Map());

/** id → current task line, from the hub poll. */
export const tasks = writable<Map<number, string>>(new Map());

/** The full hub snapshot (locks / waiting / messages / pending count). */
export const hub = writable<HubSnapshot | null>(null);

/** Focused session id (null when there are none). */
export const activeId = writable<number | null>(null);

/** The open project, or null while the picker is showing. */
export const project = writable<BootstrapInfo | null>(null);

/** Whether the ⌘M message reader panel is open. */
export const showMessages = writable(false);

/** The open rename dialog: { id, value } or null. */
export const rename = writable<{ id: number; value: string } | null>(null);

/** A transient status-strip notice (e.g. "✓ copied 42 chars"). */
export const notice = writable<string | null>(null);

let noticeTimer: number | undefined;
export function flashNotice(text: string, ms = 2000) {
  notice.set(text);
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => notice.set(null), ms) as unknown as number;
}
