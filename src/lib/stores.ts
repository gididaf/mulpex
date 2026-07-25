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

// ---- classic single-project projections (read-only) of the active project ----

/** Ordered live sessions of the active project. */
export const sessions = derived(activeProject, (p) => p?.sessions ?? []);
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

/** Whether the ⌘M message reader panel is open. */
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
