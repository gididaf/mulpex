// Auto-update: check GitHub releases, download, install, relaunch.
//
// The updater does NOT ship the .dmg — it consumes the `Mulpex.app.tar.gz` +
// `.sig` pair that `createUpdaterArtifacts` emits, verifies the minisign
// signature against the pubkey baked into tauri.conf.json, and swaps the bundle
// in place. The .dmg remains the first-install channel only.
//
// Why this sidesteps Gatekeeper: `com.apple.quarantine` is written by the
// *downloading* application (a browser, via LaunchServices). The updater fetches
// over the app's own HTTP client, so nothing sets the xattr and the extracted
// bundle has none to inherit — no `xattr -dr` needed after the first install.

import { hosts, hostUpdate, hostUpdateCancel, hostUpdateClear, isRemote } from "./hosts";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { get, writable } from "svelte/store";

import { restartApp, updateSurvives } from "./ipc";
import { projects } from "./stores";

/** How often the background check runs. Deliberately hours, not minutes: a
 * single-user app polling GitHub every few minutes is pure noise, and nothing is
 * lost by learning about a release a few hours late. */
const CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;

export type UpdatePhase =
  /** Nothing to show. */
  | "idle"
  /** A manual check is in flight (automatic checks stay silent). */
  | "checking"
  /** A newer version exists and is waiting for the user to press the button. */
  | "available"
  /** Confirming a restart because sessions are mid-turn. */
  | "confirming"
  /** Updating every connected remote host first (`remotes`), so they still
   *  match this Mulpex after it restarts. */
  | "remotes"
  /** Downloading + installing. */
  | "installing"
  /** Manual check found nothing — a transient "you're up to date". */
  | "uptodate"
  /** A manual check or an install failed. */
  | "error";

export interface UpdateState {
  phase: UpdatePhase;
  /** The available version, once known. */
  version: string | null;
  /** Release notes from latest.json, if the release supplied any. */
  notes: string | null;
  /** 0–100 while installing, or null when the server sent no content-length. */
  percent: number | null;
  error: string | null;
  /** How many sessions are mid-turn or waiting on the user, at confirm time. */
  busy: number;
  /** The hosts ordered to update in the `remotes` phase, by slot. Their
   *  progress lives on each `HostStatus.update`. */
  remotes: number[];
}

const initial: UpdateState = {
  phase: "idle",
  version: null,
  notes: null,
  percent: null,
  error: null,
  busy: 0,
  remotes: [],
};

export const updateState = writable<UpdateState>({ ...initial });

/** The pending `Update` handle. Kept out of the store: it's a live object with
 * methods, not serialisable state, and only one can ever be in flight. */
let pending: Update | null = null;

function patch(p: Partial<UpdateState>) {
  updateState.update((s) => ({ ...s, ...p }));
}

export function dismissUpdate(): void {
  // Keeps `pending` — the banner comes back on the next check or menu invocation
  // without re-downloading metadata.
  patch({ phase: "idle", error: null, percent: null });
}

/** Sessions that would lose work to a restart, across EVERY open project — not
 * just the visible one. `working` is mid-turn; `needs` is stopped on a question
 * that a restart would discard. `waiting` (done with its turn) is free to
 * restart: `--resume` brings it back where it was.
 *
 * ...with one exception that is NOT a status word: an instance holding a
 * **watcher** reads `waiting` because it genuinely is idle, but `--resume`
 * restores only the conversation. Whatever the watcher was attached to — a live
 * agentalk channel, a `tail -f` — is gone, and re-establishing it is not
 * something a restart does. So it counts as busy while looking idle. */
export function busySessionCount(): number {
  let n = 0;
  for (const p of get(projects).values()) {
    // Updating this app touches only this machine's claudes.
    if (isRemote(p.handle)) continue;
    for (const [id, status] of p.statuses) {
      if (status === "working" || status === "needs" || p.watching.has(id)) n++;
    }
  }
  return n;
}

/**
 * Ask the endpoint whether a newer version exists.
 *
 * `manual` separates the two callers: the menu item reports everything back
 * (including "you're up to date" and network errors), while the periodic check
 * is silent on both — a laptop that spends the day on flaky wifi must not
 * accumulate error banners nobody asked for.
 */
export async function checkForUpdate(manual = false): Promise<void> {
  // Never clobber a phase the user is interacting with: `confirming` is a
  // question on screen awaiting an answer, `installing` is a download in flight,
  // `remotes` is remote hosts updating.
  const phase = get(updateState).phase;
  if (phase === "installing" || phase === "confirming" || phase === "remotes") return;
  if (manual) patch({ phase: "checking", error: null });
  try {
    const found = await check();
    if (found) {
      pending = found;
      patch({
        phase: "available",
        version: found.version,
        notes: found.body ?? null,
        error: null,
      });
    } else {
      pending = null;
      if (manual) {
        patch({ phase: "uptodate", version: null, error: null });
        setTimeout(() => {
          if (get(updateState).phase === "uptodate") dismissUpdate();
        }, 3000);
      }
    }
  } catch (e) {
    const msg = String(e);
    if (manual) patch({ phase: "error", error: msg });
    else console.error("update check failed:", msg);
  }
}

/** The version every connected remote host was brought to this run. Remote
 * Hosts needs both sides on the same wire fingerprint (`wire.rs`), so a host
 * on another wire must update BEFORE this Mulpex restarts — after it, the host
 * refuses our hello. */
let remotesAt: string | null = null;

/** A number from latest.json, or null when the release predates the field. */
function numOf(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/** The wire fingerprint `release.sh` put in latest.json, or null: a release
 * from before it, which every host must follow. */
function wireOf(u: Update): string | null {
  const w = u.rawJson?.wire;
  return typeof w === "string" && w ? w : null;
}

/** Order every connected host whose wire `version` changes to update to it,
 * and wait until each is back on it. A host already on that wire keeps running
 * (its claudes are not stopped); right-click › Update host brings it along when
 * the user wants. Rejects with the first failure, after calling the others off. */
function updateRemotes(version: string, wire: string | null): Promise<void> {
  const slots = [...get(hosts).values()]
    .filter((h) => h.state === "connected" && (wire === null || h.wire !== wire))
    .map((h) => h.slot);
  if (slots.length === 0) return Promise.resolve();
  patch({ phase: "remotes", remotes: slots, error: null });
  return new Promise(async (resolve, reject) => {
    let settled = false;
    let unsub = () => {};
    const finish = (err: string | null) => {
      if (settled) return;
      settled = true;
      queueMicrotask(() => unsub());
      if (err) {
        for (const s of slots) void hostUpdateCancel(s).catch(() => {});
        reject(new Error(err));
      } else resolve();
    };
    try {
      await Promise.all(slots.map((s) => hostUpdate(s, version)));
    } catch (e) {
      return finish(String(e));
    }
    // What the backend just set, so a stale `done`/`error` from an earlier
    // round can't be read before its `host-changed` arrives.
    hosts.update((m) => {
      const n = new Map(m);
      for (const s of slots) {
        const h = n.get(s);
        if (h) n.set(s, { ...h, update: { to: version, step: "sent", pending: [], percent: null, error: null } });
      }
      return n;
    });
    unsub = hosts.subscribe((m) => {
      if (get(updateState).phase !== "remotes") return finish("cancelled");
      let done = 0;
      for (const s of slots) {
        const h = m.get(s);
        if (!h || h.state === "disconnected") return finish(`${h?.name ?? "a remote host"} was disconnected`);
        // Not ordered yet: `host-changed` can land after this first look.
        const u = h.update;
        if (!u || u.to !== version) continue;
        if (u.step === "error" || u.step === "cancelled") {
          return finish(`${h.name}: ${u.error ?? "the update was cancelled"}`);
        }
        if (u.step === "done") done++;
      }
      if (done === slots.length) finish(null);
    });
  });
}

/** The banner's Cancel while remote hosts update: their claudes carry on, and
 * this Mulpex does not update either. */
export function cancelRemoteUpdates(): void {
  const { remotes } = get(updateState);
  for (const s of remotes) {
    void hostUpdateCancel(s).catch(() => {});
    void hostUpdateClear(s).catch(() => {});
  }
  patch({ phase: "available", remotes: [] });
}

/**
 * Update every connected remote host, then download, install, and relaunch.
 *
 * With sessions mid-turn this first parks in `confirming` and returns; the
 * banner's confirm button calls back in with `force`.
 */
export async function applyUpdate(force = false): Promise<void> {
  if (!pending) return;
  if (remotesAt !== pending.version) {
    const version = pending.version;
    try {
      await updateRemotes(version, wireOf(pending));
    } catch (e) {
      // Cancel already put the banner back; anything else is a failure, and
      // this Mulpex stays on the version its hosts still run.
      if (get(updateState).phase === "remotes") {
        patch({
          phase: "error",
          error: `Remote host update failed, so this Mulpex did not update either. ${String(e).replace(/^Error: /, "")}`,
        });
      }
      return;
    }
    remotesAt = version;
  }
  // When the running PTY daemon is one the new version drives, every claude
  // and terminal keeps running through the restart (`ptyd.rs`) — there is
  // nothing to warn about, and nothing to stop first.
  const survives = await updateSurvives(numOf(pending.rawJson?.ptyd), numOf(pending.rawJson?.ptyd_min)).catch(() => false);
  if (!force && !survives) {
    const busy = busySessionCount();
    if (busy > 0) {
      patch({ phase: "confirming", busy });
      return;
    }
  }

  patch({ phase: "installing", percent: null, error: null });
  try {
    let total = 0;
    let got = 0;
    await pending.downloadAndInstall((event) => {
      switch (event.event) {
        case "Started":
          total = event.data.contentLength ?? 0;
          patch({ percent: total ? 0 : null });
          break;
        case "Progress":
          got += event.data.chunkLength;
          if (total) patch({ percent: Math.round((got / total) * 100) });
          break;
        case "Finished":
          patch({ percent: 100 });
          break;
      }
    });
    // The bundle on disk is now the new version; restart into it. Goes through
    // `AppHandle::restart` so teardown runs first — detached (every session
    // kept for the new app to adopt) when they survive, otherwise killing every
    // `claude` process group, which a plain `relaunch()` would orphan.
    await restartApp(0, survives);
  } catch (e) {
    patch({ phase: "error", error: String(e) });
  }
}

/** Check once at startup, then every `CHECK_INTERVAL_MS`. Returns a teardown fn. */
export function startUpdateChecks(): () => void {
  void checkForUpdate(false);
  const timer = setInterval(() => void checkForUpdate(false), CHECK_INTERVAL_MS);
  return () => clearInterval(timer);
}
