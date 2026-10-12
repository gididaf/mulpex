// Remote Hosts, the lifecycle: keeping each paired host's tabs in step with
// the host (`hosts.ts` has the vocabulary, `client.rs` the connection).
//
// - **Connected**: the host's workspace is fetched (`bootstrap`, through the
//   host) and its projects are built like local ones, under their lifted
//   handles. From then on the host's own events keep them current —
//   `hub-update`, `sessions-changed`… arrive under the same names as local ones
//   (`client.rs` lifts the handles), and `host-projects-changed` is the host's
//   `projects-changed`.
// - **Offline**: the tabs stay, frozen, with every status cleared — a claude
//   on an unreachable host is not "waiting" for anyone, and must not light a
//   badge. The layout (names only) is cached, so even a fresh launch with the
//   host down draws its tabs.
// - **The host restarted** (its `boot` changed): every handle we hold for it
//   may now name something else, so its tabs are dropped and built again.
//
// The geometry: this Mulpex claims each connected host's PTY size when it
// connects, whenever this window gets focus, and when its pane resizes while it
// holds the lease (`desktop.rs`).

import { get } from "svelte/store";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  activeHost,
  hostCacheLayout,
  hostConnect,
  hostGeoClaim,
  hostList,
  hostPair,
  hostRemove,
  hostRename,
  hosts,
  slotOf,
  type HostGeo,
  type HostLayout,
  type HostStatus,
} from "./hosts";
import {
  applyHubFor,
  addProject,
  patchProject,
  projects,
  reorderProjects,
  setSessionsFor,
  type ProjectState,
} from "./stores";
import { bootstrap, getHubSnapshot, type BootstrapInfo, type WorkspaceInfo } from "./ipc";
import { terminals } from "./terminals";
import { loadPins } from "./pins";
import { quietHost } from "./attention";

/** What only App.svelte can do: build a project's UI (xterms, pins, hub), and
 *  forget one (terminals, pins, explain panels, the store). */
export interface HostHooks {
  bootstrapProject(info: BootstrapInfo, makeActive: boolean): Promise<void>;
  dropProject(handle: number): void;
  /** The host in front lost its last project / its tabs were rebuilt. */
  afterChange(slot: number): void;
}

let hooks: HostHooks;
/** The host run each slot's tabs were built for (`HostStatus.boot`). */
const built = new Map<number, string | null>();
const syncing = new Set<number>();

const ofHost = (slot: number): ProjectState[] =>
  [...get(projects).values()].filter((p) => slotOf(p.handle) === slot);

function blankProject(handle: number, dir: string, name: string, sessions: BootstrapInfo["sessions"]): ProjectState {
  return {
    handle,
    dir,
    name,
    sessions,
    statuses: new Map(),
    watching: new Set(),
    ctx: new Map(),
    tasks: new Map(),
    hub: null,
    activeSessionId: sessions[0]?.id ?? null,
  };
}

/** Draw a host's last known tabs, greyed, until it connects. */
function buildCached(st: HostStatus) {
  for (const p of st.layout?.projects ?? []) {
    if (slotOf(p.handle) !== st.slot || get(projects).has(p.handle)) continue;
    addProject(blankProject(p.handle, p.project_dir, p.project_name, p.sessions));
  }
}

/** The host went away: keep its tabs, forget every status they showed. */
function markOffline(slot: number) {
  for (const p of ofHost(slot)) {
    patchProject(p.handle, { statuses: new Map(), watching: new Set(), ctx: new Map(), hub: null });
  }
}

/** Bring a host's tabs in line with its workspace `ws` (handles already
 *  lifted). Also this Mulpex's own `projects-changed` (slot 0): a remote
 *  desktop can open, close and reorder projects here. */
export async function applyWorkspace(slot: number, ws: WorkspaceInfo) {
  const have = new Set(ofHost(slot).map((p) => p.handle));
  const want = new Set(ws.projects.map((p) => p.handle));
  for (const h of have) if (!want.has(h)) hooks.dropProject(h);
  for (const info of ws.projects) {
    if (!get(projects).has(info.handle)) {
      await hooks.bootstrapProject(info, false);
    } else {
      setSessionsFor(info.handle, info.sessions);
      patchProject(info.handle, { name: info.project_name, dir: info.project_dir });
    }
  }
  // Tab order: this host's projects in the host's order. Other hosts' tabs
  // never share the bar, so where they sit relative to these doesn't matter.
  const others = [...get(projects).keys()].filter((h) => slotOf(h) !== slot);
  reorderProjects([...others, ...ws.projects.map((p) => p.handle)]);
  hooks.afterChange(slot);
}

/** The host just connected: build (or rebuild) its tabs from its workspace. */
async function syncHost(slot: number) {
  if (syncing.has(slot)) return;
  syncing.add(slot);
  try {
    const st = get(hosts).get(slot);
    if (!st || st.state !== "connected") return;
    if (built.has(slot) && built.get(slot) !== st.boot) {
      // A different run of the host: handles we hold may name other projects.
      for (const p of ofHost(slot)) hooks.dropProject(p.handle);
    }
    const ws = await bootstrap(slot);
    terminals.setHostGeo(slot, ws.cols, ws.rows);
    built.set(slot, st.boot);
    await applyWorkspace(slot, ws);
    // Tabs that already existed came back with no statuses; mirrors were
    // re-snapshotted by `client.rs` on its own.
    for (const p of ofHost(slot)) {
      void getHubSnapshot(p.handle)
        .then((snap) => snap && applyHubFor(p.handle, snap))
        .catch(() => {});
      void loadPins(p.handle);
    }
    claim(slot);
    cacheLayout(slot);
  } catch (e) {
    console.error(`syncing host ${slot} failed:`, e);
  } finally {
    syncing.delete(slot);
  }
}

function onHostChanged(st: HostStatus) {
  const prev = get(hosts).get(st.slot);
  hosts.update((m) => new Map(m).set(st.slot, { ...st, layout: st.layout ?? prev?.layout ?? null }));
  if (st.geo) terminals.setHostGeo(st.slot, st.geo.cols, st.geo.rows);
  const was = prev?.state === "connected";
  if (st.state === "connected" && !was) {
    // Whatever already needs you over there is not news: no burst of banners.
    quietHost(st.slot, 6000);
    void syncHost(st.slot);
  } else if (st.state !== "connected" && was) {
    claimed.delete(st.slot);
    markOffline(st.slot);
  }
}

/** Drive host `slot`'s geometry from this window's pane. */
function claim(slot: number) {
  if (get(hosts).get(slot)?.state !== "connected") return;
  // Not before this window's pane has a real size: the spawn default would
  // resize every PTY over there twice for nothing. The first measurement
  // claims (`onLocalGeometry`).
  const geo = terminals.geometry();
  if (!geo) return;
  claimed.add(slot);
  void hostGeoClaim(slot, geo.cols, geo.rows).catch(() => {});
}

/** Hosts claimed since they last connected. */
const claimed = new Set<number>();

export function claimHost(slot: number) {
  claim(slot);
}

let geoTimer: ReturnType<typeof setTimeout> | undefined;
/** This window's pane changed size: follow it on every host it drives. */
function onLocalGeometry() {
  clearTimeout(geoTimer);
  geoTimer = setTimeout(() => {
    for (const st of get(hosts).values()) if (st.geo?.mine || !claimed.has(st.slot)) claim(st.slot);
  }, 150);
}

const cacheTimers = new Map<number, ReturnType<typeof setTimeout>>();
/** Remember a host's tabs (names only) for drawing them while it is down. */
function cacheLayout(slot: number) {
  clearTimeout(cacheTimers.get(slot));
  cacheTimers.set(
    slot,
    setTimeout(() => {
      const st = get(hosts).get(slot);
      if (!st || st.state !== "connected") return;
      const layout: HostLayout = {
        projects: ofHost(slot).map((p) => ({
          handle: p.handle,
          project_dir: p.dir,
          project_name: p.name,
          sessions: p.sessions,
        })),
      };
      void hostCacheLayout(slot, built.get(slot) ?? null, layout).catch(() => {});
    }, 1500),
  );
}

export async function initHosts(h: HostHooks): Promise<() => void> {
  hooks = h;
  const list = await hostList().catch(() => [] as HostStatus[]);
  hosts.set(new Map(list.map((s) => [s.slot, s])));
  for (const st of list) {
    built.set(st.slot, st.cached_boot);
    if (st.geo) terminals.setHostGeo(st.slot, st.geo.cols, st.geo.rows);
    if (st.layout) buildCached(st);
    if (st.state === "connected") void syncHost(st.slot);
  }
  terminals.onGeometry = onLocalGeometry;

  // Re-cache a connected host's layout whenever its tabs or rows change.
  let lastSig = new Map<number, string>();
  const stopCache = projects.subscribe((m) => {
    const sig = new Map<number, string>();
    for (const p of m.values()) {
      const slot = slotOf(p.handle);
      if (!slot) continue;
      sig.set(slot, (sig.get(slot) ?? "") + `|${p.handle}:${p.name}:${p.sessions.map((s) => `${s.id}${s.name ?? ""}`).join(",")}`);
    }
    for (const [slot, s] of sig) if (lastSig.get(slot) !== s) cacheLayout(slot);
    lastSig = sig;
  });

  const un = [
    listen<HostStatus>("host-changed", (e) => onHostChanged(e.payload)),
    listen<{ slot: number; ws: WorkspaceInfo }>("host-projects-changed", (e) => {
      if (get(hosts).get(e.payload.slot)?.state === "connected" && !syncing.has(e.payload.slot)) {
        void applyWorkspace(e.payload.slot, e.payload.ws);
      }
    }),
    listen<{ slot: number; geo: HostGeo }>("host-geo", (e) => {
      const { slot, geo } = e.payload;
      terminals.setHostGeo(slot, geo.cols, geo.rows);
      hosts.update((m) => {
        const st = m.get(slot);
        return st ? new Map(m).set(slot, { ...st, geo }) : m;
      });
    }),
    // Coming back to this window takes the geometry back on every host.
    getCurrentWindow().onFocusChanged(({ payload: focused }) => {
      if (focused) for (const st of get(hosts).values()) claim(st.slot);
    }),
  ];
  return () => {
    stopCache();
    terminals.onGeometry = null;
    un.forEach((p) => p.then((f) => f()));
  };
}

// ---- what the host switcher's menu and the Connect dialog do ----

export async function pairHost(link: string): Promise<HostStatus> {
  const st = await hostPair(link);
  hosts.update((m) => new Map(m).set(st.slot, st));
  return st;
}

export async function removeHost(slot: number) {
  await hostRemove(slot);
  for (const p of ofHost(slot)) hooks.dropProject(p.handle);
  built.delete(slot);
  hosts.update((m) => {
    const next = new Map(m);
    next.delete(slot);
    return next;
  });
  if (get(activeHost) === slot) hooks.afterChange(slot);
}

export async function renameHost(slot: number, name: string) {
  await hostRename(slot, name);
}

export async function connectHost(slot: number, on: boolean) {
  await hostConnect(slot, on);
  if (!on) {
    markOffline(slot);
    hosts.update((m) => {
      const st = m.get(slot);
      return st ? new Map(m).set(slot, { ...st, enabled: false, state: "disconnected" }) : m;
    });
  }
}

/** The project to show when switching to host `slot`: the first one with a
 *  claude that needs you, else the one last in front there, else the first. */
export function landingProject(slot: number, last: number | undefined): number | null {
  const list = ofHost(slot);
  const needs = list.find((p) => p.sessions.some((s) => !s.muted && p.statuses.get(s.id) === "needs"));
  if (needs) return needs.handle;
  if (last != null && list.some((p) => p.handle === last)) return last;
  return list[0]?.handle ?? null;
}
