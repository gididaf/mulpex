// The reactive projection of backend state. PTY bytes bypass these entirely
// (they go straight to xterm via each session's Channel); only the sidebar/hub/tab
// surface lives here.
//
// Multi-project: `projects` holds one `ProjectState` per open project (insertion
// order == tab order). The classic single-project stores (`sessions`, `statuses`,
// `tasks`, `hub`, `activeId`, `project`) are now DERIVED read-only projections of
// the active project, so the sidebar/hub components consume them unchanged. All
// writes go through the mutator helpers below.

import { writable, derived, get } from "svelte/store";
import type {
  HubSnapshot,
  ProjectHandle,
  SessionInfo,
  Status,
} from "./ipc";

export type { ProjectHandle } from "./ipc";

/** Everything the UI tracks for one open project. */
export interface ProjectState {
  handle: ProjectHandle;
  dir: string;
  name: string;
  sessions: SessionInfo[];
  statuses: Map<number, Status>;
  tasks: Map<number, string>;
  hub: HubSnapshot | null;
  activeSessionId: number | null;
}

/** All open projects, keyed by handle in tab order. */
export const projects = writable<Map<ProjectHandle, ProjectState>>(new Map());

/** The active (front-most) project's handle, or null when none are open. */
export const activeProjectHandle = writable<ProjectHandle | null>(null);

/** The active project's full state, or null. */
export const activeProject = derived(
  [projects, activeProjectHandle],
  ([$p, $h]) => ($h != null ? ($p.get($h) ?? null) : null),
);

// ---- mute: display order + the badge counts that must exclude muted ----

/**
 * Sidebar order: unmuted first, muted sunk to the bottom, each group keeping its
 * creation order. `Array.prototype.sort` is required to be stable, so a plain
 * key sort is enough — a muted instance keeps its place relative to other muted
 * ones, and unmuting drops it straight back where it came from.
 *
 * This is the single source of the visible order: ⌘[ / ⌘] cycle through *this*
 * list, so what you see is what you cycle.
 */
export function displayOrder(list: SessionInfo[]): SessionInfo[] {
  return [...list].sort((a, b) => Number(a.muted) - Number(b.muted));
}

/** Ids muted in this project — the set every badge count subtracts. */
function mutedIds(p: ProjectState): Set<number> {
  return new Set(p.sessions.filter((s) => s.muted).map((s) => s.id));
}

/** Sessions of `p` stopped on a question, muted ones excluded (the tab's red badge). */
export function needsCount(p: ProjectState): number {
  return p.sessions.filter((s) => !s.muted && p.statuses.get(s.id) === "needs")
    .length;
}

/**
 * Unread hub messages in `p`, muted recipients excluded (the tab's amber badge,
 * and the same number the hub panel and status strip show).
 *
 * `pending_messages` is a project-wide total, so the muted share has to come off
 * it via the per-recipient `pending` breakdown. The message *log* is untouched —
 * mute silences the count that pulls your eye, not the record of what happened.
 */
export function unreadCount(p: ProjectState): number {
  if (!p.hub) return 0;
  if (!p.sessions.some((s) => s.muted)) return p.hub.pending_messages;
  const muted = mutedIds(p);
  const silenced = p.hub.pending
    .filter((e) => muted.has(e.id))
    .reduce((n, e) => n + e.count, 0);
  return Math.max(0, p.hub.pending_messages - silenced);
}

/**
 * Claudes blocked on the user across **every** open project — the dock badge.
 *
 * Deliberately `needs` only, not `needs + waiting`: a finished (`waiting`)
 * session isn't asking for anything, so counting it would leave the badge lit
 * permanently and stop meaning "there is something for you to do". Muted
 * sessions are excluded for the same reason the tab badges exclude them.
 */
export const blockedTotal = derived(projects, ($p) =>
  [...$p.values()].reduce((n, p) => n + needsCount(p), 0),
);

// ---- classic single-project projections (read-only) of the active project ----

/** Live sessions of the active project, in sidebar order (muted last). */
export const sessions = derived(activeProject, (p) =>
  p ? displayOrder(p.sessions) : [],
);

/** Unread hub messages in the active project, muted recipients excluded. */
export const unread = derived(activeProject, (p) => (p ? unreadCount(p) : 0));
/** id → status word, active project. */
export const statuses = derived(
  activeProject,
  (p) => p?.statuses ?? new Map<number, Status>(),
);
/** id → current task line, active project. */
export const tasks = derived(
  activeProject,
  (p) => p?.tasks ?? new Map<number, string>(),
);
/** The active project's hub snapshot (locks / waiting / messages / pending). */
export const hub = derived(activeProject, (p) => p?.hub ?? null);
/** Focused session id within the active project (null when none). */
export const activeId = derived(activeProject, (p) => p?.activeSessionId ?? null);
/** Non-null while any project is open — App.svelte's shell gate. */
export const project = activeProject;

// ---- mutators (every write reassigns a fresh Map for Svelte-5 reactivity) ----

function mutate(fn: (m: Map<ProjectHandle, ProjectState>) => void) {
  projects.update((m) => {
    const next = new Map(m);
    fn(next);
    return next;
  });
}

/** Insert (or replace) a project. Does not change the active handle. */
export function addProject(p: ProjectState): void {
  mutate((m) => m.set(p.handle, p));
}

/** Remove a project; if it was active, re-pick the neighbor that shifts into its
 * slot (else the last, else null) — mirrors the backend's re-pick. */
export function removeProject(handle: ProjectHandle): void {
  const keys = [...get(projects).keys()];
  const pos = keys.indexOf(handle);
  mutate((m) => m.delete(handle));
  activeProjectHandle.update((a) => {
    if (a !== handle) return a;
    if (pos < 0) return a;
    const remaining = keys.filter((k) => k !== handle);
    return remaining[pos] ?? remaining[remaining.length - 1] ?? null;
  });
}

/**
 * Rearrange the tab order to `order` (a drag's new left-to-right handle list).
 * A `Map`'s iteration order is insertion order and that *is* the tab order, so
 * reordering means rebuilding the Map — there is no key to sort on.
 *
 * Handles missing from `order` are appended in their existing order rather than
 * dropped, so a stale caller can never make a project vanish from the tab bar.
 * Persisting the new order is the backend's job (`reorderProjects`).
 */
export function reorderProjects(order: ProjectHandle[]): void {
  projects.update((m) => {
    const next = new Map<ProjectHandle, ProjectState>();
    for (const h of order) {
      const p = m.get(h);
      if (p) next.set(h, p);
    }
    for (const [h, p] of m) if (!next.has(h)) next.set(h, p);
    return next;
  });
}

/** Shallow-merge a patch into one project's state. */
export function patchProject(
  handle: ProjectHandle,
  patch: Partial<ProjectState>,
): void {
  mutate((m) => {
    const p = m.get(handle);
    if (p) m.set(handle, { ...p, ...patch });
  });
}

export function setActiveProject(handle: ProjectHandle | null): void {
  activeProjectHandle.set(handle);
}

export function setActiveSession(
  handle: ProjectHandle,
  id: number | null,
): void {
  patchProject(handle, { activeSessionId: id });
}

export function setSessionsFor(
  handle: ProjectHandle,
  sessions: SessionInfo[],
): void {
  patchProject(handle, { sessions });
}

/** Flip one session's muted flag locally (the backend persists it separately;
 *  mute doesn't go through the `sessions-changed` event). */
export function setSessionMutedLocal(
  handle: ProjectHandle,
  id: number,
  muted: boolean,
): void {
  const p = get(projects).get(handle);
  if (!p) return;
  patchProject(handle, {
    sessions: p.sessions.map((s) => (s.id === id ? { ...s, muted } : s)),
  });
}

/** Apply a hub snapshot to one project (also derives its statuses + tasks maps). */
export function applyHubFor(handle: ProjectHandle, snap: HubSnapshot): void {
  patchProject(handle, {
    hub: snap,
    statuses: new Map(snap.statuses.map((e) => [e.id, e.status])),
    tasks: new Map(snap.tasks.map((e) => [e.id, e.task])),
  });
}

/** An already-open project whose dir matches `dir` (best-effort exact match; the
 * backend still dedups canonically). */
export function findByDir(dir: string): ProjectState | undefined {
  for (const p of get(projects).values()) if (p.dir === dir) return p;
  return undefined;
}

// ---- UI-only stores ----

/** Whether the ⌘⇧M message reader panel is open. */
export const showMessages = writable(false);

/** Whether the ⌘P project quick-switcher overlay is open. */
export const showPalette = writable(false);

/** The open rename dialog: { handle, id, value } or null. */
export const rename = writable<{
  handle: ProjectHandle;
  id: number;
  value: string;
} | null>(null);

/** A transient status-strip notice (e.g. "✓ copied 42 chars"). */
export const notice = writable<string | null>(null);

let noticeTimer: number | undefined;
export function flashNotice(text: string, ms = 2000) {
  notice.set(text);
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => notice.set(null), ms) as unknown as number;
}
