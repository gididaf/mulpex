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
  SaveProgressEvent,
  SaveState,
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
  /** Instances that ended their turn holding a watcher — see
   *  `StatusEntry.watching`. Read only by `updater.ts`'s busy guard: these rows
   *  are idle (`waiting`) and must look it. */
  watching: Set<number>;
  /** id → context window used, whole percent. Claudes that have replied only. */
  ctx: Map<number, number>;
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

// ---- display order: kind split + mute sinking, and the drag clamp they imply ----

/**
 * The block a row belongs to, top to bottom: unmuted claudes (0), muted claudes
 * (1), terminals (2).
 *
 * Kind is the OUTER grouping — every terminal sits below every claude, muted
 * ones included — because the two are different things you switch between, not
 * two states of one thing. Mute only ranks claudes: the backend refuses to
 * record the flag for a shell (a terminal produces none of the signals mute
 * silences), so a terminal's own flag is never trusted here either.
 */
function groupOf(s: SessionInfo): number {
  return s.kind === "shell" ? 2 : Number(s.muted);
}

/**
 * Sidebar order: claudes first (unmuted, then muted), then terminals, each block
 * keeping its creation/drag order. `Array.prototype.sort` is required to be
 * stable, so a plain key sort is enough — a muted instance keeps its place
 * relative to other muted ones, and unmuting drops it straight back where it
 * came from.
 *
 * This is the single source of the visible order: ⌘[ / ⌘] cycle through *this*
 * list, so what you see is what you cycle.
 *
 * Each top-level row is followed by its `hub_spawn` family, depth first, and
 * every set of siblings is sorted the same way the top level is — so a muted
 * child sinks to the bottom of its own siblings (taking its family along), and
 * never out of its parent's family.
 */
export function displayOrder(list: SessionInfo[]): SessionInfo[] {
  const kids = childrenOf(list);
  const roots = list
    .filter((s) => parentOf(list, s) == null)
    .sort((a, b) => groupOf(a) - groupOf(b));
  const out: SessionInfo[] = [];
  const visit = (s: SessionInfo) => {
    out.push(s);
    // Muted children sink to the bottom of their siblings, like muted roots do.
    const own = [...(kids.get(s.id) ?? [])].sort((a, b) => groupOf(a) - groupOf(b));
    own.forEach(visit);
  };
  roots.forEach(visit);
  return out;
}

/**
 * The row's parent, if it is a claude present in `list` — a `hub_spawn`
 * child nests under its spawner (the "family"). Anything else is a root: the
 * backend already re-parents children when a parent closes, so this is only a
 * guard against a link that names nothing. Links never form a cycle: a spawner
 * exists before it spawns, and drag-to-nest (`Core::reparent`, and `canNest`
 * here) refuses a parent from inside the row's own family.
 */
function parentOf(list: SessionInfo[], s: SessionInfo): SessionInfo | undefined {
  if (s.kind === "shell" || s.parent == null) return undefined;
  const p = list.find((x) => x.id === s.parent);
  return p && p.kind !== "shell" ? p : undefined;
}

/** parent id → its children, in `list` order (which is their order on screen). */
function childrenOf(list: SessionInfo[]): Map<number, SessionInfo[]> {
  const kids = new Map<number, SessionInfo[]>();
  for (const s of list) {
    const p = parentOf(list, s);
    if (p) kids.set(p.id, [...(kids.get(p.id) ?? []), s]);
  }
  return kids;
}

/** Rows folded away under a collapsed ancestor — the sidebar skips them, and
 *  so do ⌘[ / ⌘]. */
export function hiddenIds(list: SessionInfo[]): Set<number> {
  const hidden = new Set<number>();
  for (const s of list) {
    for (let p = parentOf(list, s); p; p = parentOf(list, p)) {
      if (p.collapsed) {
        hidden.add(s.id);
        break;
      }
    }
  }
  return hidden;
}

/** The folded rows above `id` — what has to unfold for it to be on screen. */
export function collapsedAncestors(list: SessionInfo[], id: number): number[] {
  const out: number[] = [];
  const row = list.find((s) => s.id === id);
  for (let p = row && parentOf(list, row); p; p = parentOf(list, p)) {
    if (p.collapsed) out.push(p.id);
  }
  return out;
}

/** Every row under `id` in its `hub_spawn` family, at any depth. */
export function descendantsOf(list: SessionInfo[], id: number): SessionInfo[] {
  const out: SessionInfo[] = [];
  const ids = new Set([id]);
  for (let grew = true; grew; ) {
    grew = false;
    for (const s of list) {
      const up = parentOf(list, s);
      if (up && ids.has(up.id) && !ids.has(s.id)) {
        ids.add(s.id);
        out.push(s);
        grew = true;
      }
    }
  }
  return out;
}

/** How deep each row sits in its family: 0 for a top-level row. */
export function treeDepths(list: SessionInfo[]): Map<number, number> {
  const depth = new Map<number, number>();
  const of = (s: SessionInfo): number => {
    const known = depth.get(s.id);
    if (known != null) return known;
    const p = parentOf(list, s);
    const d = p ? of(p) + 1 : 0;
    depth.set(s.id, d);
    return d;
  };
  list.forEach(of);
  return depth;
}

/**
 * The rows a row can trade places with, as `[start, end)` index ranges of
 * their whole subtrees in `list` (the *displayed* order, i.e. already through
 * `displayOrder`), top to bottom — plus where the row's own range sits.
 *
 * Siblings are the other rows with the same parent (or none) in the same
 * block (see `groupOf`) — so a muted child moves only among its muted siblings. A row only ever moves
 * among them, and always together with its own `hub_spawn` family: dropping a
 * child into another family, or a parent between its own children, would snap
 * back the moment `displayOrder` re-applied the tree.
 *
 * Because `displayOrder` lists each family depth first, every subtree is one
 * contiguous range, and a set of siblings is one contiguous run of them.
 */
function siblingRanges(
  list: SessionInfo[],
  from: number,
): { ranges: [number, number][]; own: number } {
  const row = list[from];
  const depths = treeDepths(list);
  const depth = (i: number) => depths.get(list[i].id) ?? 0;
  const parent = parentOf(list, row)?.id;
  const isSibling = (s: SessionInfo) =>
    parentOf(list, s)?.id === parent && groupOf(s) === groupOf(row);
  const ranges: [number, number][] = [];
  for (let i = 0; i < list.length; i++) {
    if (!isSibling(list[i])) continue;
    let end = i + 1;
    while (end < list.length && depth(end) > depth(i)) end++;
    ranges.push([i, end]);
  }
  return { ranges, own: ranges.findIndex(([a]) => a === from) };
}

/** Which sibling slot index `to` falls in: the subtree holding it, or the
 *  nearest end. */
function slotAt(ranges: [number, number][], to: number): number {
  if (to < ranges[0][0]) return 0;
  const hit = ranges.findIndex(([a, b]) => to >= a && to < b);
  return hit >= 0 ? hit : ranges.length - 1;
}

/**
 * The slot a dragged sidebar row can actually land in: the first row of the
 * sibling subtree `to` points into (see `siblingRanges`), so the drop indicator
 * only ever appears where the row — and its family — can really go.
 *
 * This exists because manual order and the display grouping are composed, not
 * alternatives: `displayOrder` runs on top of the arrangement a drag commits, so
 * a drop across a block or family boundary could never stick — the row would
 * visibly snap back on release. Clamping keeps the indicator honest, and keeps
 * the order `dragOrder` emits already-grouped (so re-applying `displayOrder` to
 * it is the identity, and the frontend's optimistic repaint matches what the
 * backend echoes back).
 */
export function clampToGroup(
  list: SessionInfo[],
  from: number,
  to: number,
): number {
  if (!list[from]) return to;
  const { ranges } = siblingRanges(list, from);
  return ranges[slotAt(ranges, to)][0];
}

/** The slot one sibling up (`delta` -1) or down (+1) from row `from`, clamped
 *  at the ends — what ⌘⇧↑ / ⌘⇧↓ hand `dragOrder`. A row steps over a whole
 *  sibling family, never into it. */
export function stepSlot(list: SessionInfo[], from: number, delta: number): number {
  if (!list[from]) return from;
  const { ranges, own } = siblingRanges(list, from);
  const t = Math.min(Math.max(own + delta, 0), ranges.length - 1);
  return ranges[t][0];
}

/** The new top-to-bottom id order after dragging row `from` (with its whole
 *  family) onto slot `to`. */
export function dragOrder(
  list: SessionInfo[],
  from: number,
  to: number,
): number[] {
  const ids = list.map((s) => s.id);
  if (!list[from]) return ids;
  const { ranges, own } = siblingRanges(list, from);
  const units = ranges.map(([a, b]) => ids.slice(a, b));
  const [moved] = units.splice(own, 1);
  units.splice(slotAt(ranges, to), 0, moved);
  const lo = ranges[0][0];
  const hi = ranges[ranges.length - 1][1];
  return [...ids.slice(0, lo), ...units.flat(), ...ids.slice(hi)];
}

/** Whether row `from` may be dropped INTO row `target` (drag-to-nest): both
 *  claudes, the target outside `from`'s own family (a cycle), and not already
 *  its parent (a drop that would only shuffle it among its siblings). */
export function canNest(list: SessionInfo[], from: number, target: number): boolean {
  const row = list[from];
  const t = list[target];
  if (!row || !t || row.kind === "shell" || t.kind === "shell") return false;
  if (t.id === row.id || parentOf(list, row)?.id === t.id) return false;
  return !descendantsOf(list, row.id).some((s) => s.id === t.id);
}

/** The new top-to-bottom id order after nesting row `from` (with its whole
 *  family) under row `target` as its LAST child: the family is lifted out and
 *  set down right after the end of the target's own subtree. `list` is the
 *  displayed order, where every subtree is one contiguous run. */
export function nestOrder(list: SessionInfo[], from: number, target: number): number[] {
  const ids = list.map((s) => s.id);
  if (!list[from] || !list[target]) return ids;
  const depths = treeDepths(list);
  const depth = (i: number) => depths.get(list[i].id) ?? 0;
  const end = (i: number) => {
    let e = i + 1;
    while (e < list.length && depth(e) > depth(i)) e++;
    return e;
  };
  const moved = ids.slice(from, end(from));
  const tEnd = end(target);
  const rest = [...ids.slice(0, from), ...ids.slice(end(from))];
  // Where the target's subtree ends once the moved run is gone from above it.
  const at = tEnd > from ? tEnd - moved.length : tEnd;
  rest.splice(at, 0, ...moved);
  return rest;
}

/** The new top-to-bottom id order after pulling nested row `from` (with its
 *  family) out to the top level, set down before row `at` — or after the last
 *  claude when `at` is `list.length`, since claudes always draw above
 *  terminals. */
export function unnestOrder(list: SessionInfo[], from: number, at: number): number[] {
  const ids = list.map((s) => s.id);
  if (!list[from]) return ids;
  const depths = treeDepths(list);
  const d0 = depths.get(list[from].id) ?? 0;
  let end = from + 1;
  while (end < list.length && (depths.get(list[end].id) ?? 0) > d0) end++;
  if (at >= list.length) {
    const firstShell = list.findIndex((s) => s.kind === "shell");
    at = firstShell >= 0 ? firstShell : list.length;
  }
  const moved = ids.slice(from, end);
  const rest = [...ids.slice(0, from), ...ids.slice(end)];
  rest.splice(at > from ? at - moved.length : at, 0, ...moved);
  return rest;
}

/** Set one row's parent locally (the backend persists it via `reparent_session`
 *  and echoes it back); unfolds the new parent like the backend does. */
export function setSessionParentLocal(
  handle: ProjectHandle,
  id: number,
  parent: number | null,
): void {
  const p = get(projects).get(handle);
  if (!p) return;
  patchProject(handle, {
    sessions: p.sessions.map((s) =>
      s.id === id ? { ...s, parent } : s.id === parent ? { ...s, collapsed: false } : s,
    ),
  });
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
 * Claudes of `p` sitting idle, muted ones excluded (the tab's green badge).
 *
 * `statuses` holds claudes only — a terminal is never a hub peer — so this never
 * counts a shell. `working` is deliberately uncounted: the tab says what is
 * *finished* and what is *blocked on you*, not what is busy.
 */
export function readyCount(p: ProjectState): number {
  return p.sessions.filter((s) => !s.muted && p.statuses.get(s.id) === "waiting")
    .length;
}

/**
 * Unread hub messages in `p`, muted recipients excluded (shown in the hub panel
 * and the bottom bar; the project tab no longer badges it),
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
 * Is there a claude in some project *other* than `handle`?
 *
 * The one condition that makes a bare `claude#3` ambiguous, so it is what
 * decides whether Copy address qualifies an instance with its project name
 * (`App.svelte::openRowMenu`). Terminals elsewhere deliberately don't count: a
 * terminal is never a hub peer and one in another project can't be driven from
 * here either, so a project holding only shells adds nobody you could address.
 */
export function claudeInAnotherProject(handle: ProjectHandle | null): boolean {
  return [...get(projects).values()].some(
    (p) => p.handle !== handle && p.sessions.some((s) => s.kind === "claude"),
  );
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
/** id → context window used (whole %), active project. */
export const ctx = derived(activeProject, (p) => p?.ctx ?? new Map<number, number>());
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

/**
 * Rearrange one project's sessions to `ids` (the sidebar's new top-to-bottom
 * order after a drag), for an immediate repaint ahead of the backend's own
 * reorder + `sessions-changed` echo.
 *
 * `p.sessions` is the *base* order; the sidebar shows `displayOrder(p.sessions)`,
 * which sinks muted rows. `InstanceList` only ever emits an already-grouped
 * order (drops are clamped inside a group), so re-applying `displayOrder` to
 * what it sends is the identity and the two stay consistent.
 *
 * Ids missing from `ids` are appended in their existing order rather than
 * dropped, so a stale caller can't make a session vanish — same contract as
 * `reorderProjects`.
 */
export function reorderSessions(handle: ProjectHandle, ids: number[]): void {
  const p = get(projects).get(handle);
  if (!p) return;
  const seen = new Set<number>();
  const next: SessionInfo[] = [];
  for (const id of ids) {
    const s = p.sessions.find((x) => x.id === id);
    if (s && !seen.has(id)) {
      seen.add(id);
      next.push(s);
    }
  }
  for (const s of p.sessions) if (!seen.has(s.id)) next.push(s);
  patchProject(handle, { sessions: next });
}

/** Flip a session's muted flag locally, along with its whole `hub_spawn`
 *  family — the same cascade `Core::set_muted` applies (the backend persists
 *  it separately; mute doesn't go through the `sessions-changed` event). */
export function setSessionMutedLocal(
  handle: ProjectHandle,
  id: number,
  muted: boolean,
): void {
  const p = get(projects).get(handle);
  if (!p) return;
  const family = new Set([id, ...descendantsOf(p.sessions, id).map((s) => s.id)]);
  patchProject(handle, {
    sessions: p.sessions.map((s) => (family.has(s.id) ? { ...s, muted } : s)),
  });
}

/** Fold or unfold one row's family locally (the backend persists it). */
export function setSessionCollapsedLocal(
  handle: ProjectHandle,
  id: number,
  collapsed: boolean,
): void {
  const p = get(projects).get(handle);
  if (!p) return;
  patchProject(handle, {
    sessions: p.sessions.map((s) => (s.id === id ? { ...s, collapsed } : s)),
  });
}

/** Apply a hub snapshot to one project (also derives its statuses + tasks maps). */
export function applyHubFor(handle: ProjectHandle, snap: HubSnapshot): void {
  patchProject(handle, {
    hub: snap,
    statuses: new Map(snap.statuses.map((e) => [e.id, e.status])),
    watching: new Set(snap.statuses.filter((e) => e.watching).map((e) => e.id)),
    ctx: new Map(
      snap.statuses.filter((e) => e.ctx_pct != null).map((e) => [e.id, e.ctx_pct!]),
    ),
    tasks: new Map(snap.tasks.map((e) => [e.id, e.task])),
  });
}

// ---- ⌘S save progress (saves.rs) ----

export interface SaveStatus {
  state: SaveState;
  /** The path on `done`, the reason on `error`. */
  detail: string | null;
  /** The save's title on `done`. */
  title: string | null;
}

/** Every instance's save status, keyed `handle:id`. Kept outside `ProjectState`
 *  because it is transient: it lasts until the row ⌘S holds is closed (a save
 *  ends the claude), and nothing about it survives a reload. */
const saveStates = writable<Map<string, SaveStatus>>(new Map());
const saveKey = (handle: ProjectHandle, id: number) => `${handle}:${id}`;

/** Mirror one `save-progress` event. */
export function applySaveProgress(ev: SaveProgressEvent): void {
  const status: SaveStatus = { state: ev.state, detail: ev.detail, title: ev.title };
  saveStates.update((m) => new Map(m).set(saveKey(ev.handle, ev.id), status));
}

/** claude#id's save status in any project, if ⌘S holds its row. */
export function saveStatusOf(handle: ProjectHandle, id: number): SaveStatus | undefined {
  return get(saveStates).get(saveKey(handle, id));
}

/** Drop an instance's save status (the row exited). */
export function clearSaveState(handle: ProjectHandle, id: number): void {
  saveStates.update((m) => {
    if (!m.has(saveKey(handle, id))) return m;
    const next = new Map(m);
    next.delete(saveKey(handle, id));
    return next;
  });
}

/** The active project's save statuses, by instance id. */
export const saves = derived([saveStates, activeProjectHandle], ([$m, $h]) => {
  const out = new Map<number, SaveStatus>();
  if ($h == null) return out;
  const prefix = `${$h}:`;
  for (const [k, v] of $m) if (k.startsWith(prefix)) out.set(Number(k.slice(prefix.length)), v);
  return out;
});

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

/** A short toast over the terminal — for "that key did nothing, here's why",
 *  where the bottom strip is too far from where the user is looking. */
export const toast = writable<string | null>(null);

let toastTimer: number | undefined;
export function flashToast(text: string, ms = 2000) {
  toast.set(text);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => toast.set(null), ms) as unknown as number;
}
