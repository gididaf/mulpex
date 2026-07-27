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

import { check, type Update } from "@tauri-apps/plugin-updater";
import { get, writable } from "svelte/store";

import { restartApp } from "./ipc";
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
}

const initial: UpdateState = {
  phase: "idle",
  version: null,
  notes: null,
  percent: null,
  error: null,
  busy: 0,
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
 * restart: `--resume` brings it back where it was. */
export function busySessionCount(): number {
  let n = 0;
  for (const p of get(projects).values()) {
    for (const status of p.statuses.values()) {
      if (status === "working" || status === "needs") n++;
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
  // question on screen awaiting an answer, `installing` is a download in flight.
  const phase = get(updateState).phase;
  if (phase === "installing" || phase === "confirming") return;
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

/**
 * Download, install, and relaunch.
 *
 * With sessions mid-turn this first parks in `confirming` and returns; the
 * banner's confirm button calls back in with `force`.
 */
export async function applyUpdate(force = false): Promise<void> {
  if (!pending) return;
  if (!force) {
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
    // `AppHandle::restart` so teardown kills every `claude` process group and
    // removes the scratch root first — a plain `relaunch()` would orphan them.
    await restartApp();
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
