// Two ways Mulpex tells you a claude is stuck on you while you're looking at
// something else: the dock badge (a live count) and a macOS notification (a
// one-shot alert at the moment it happens).
//
// Both key off `needs` — the status the `AskUserQuestion` / idle-prompt hooks
// write (see `config.rs`). Deliberately NOT `waiting`: `waiting` only means a
// turn ended, which happens constantly and asks nothing of you, so counting or
// announcing it would train you to ignore both signals.
//
// Muted sessions are excluded from both, matching the tab badges — mute means
// "I'm deliberately not watching this one".

import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
  onAction,
} from "@tauri-apps/plugin-notification";
import { projects, blockedTotal, type ProjectState } from "./stores";
import type { ProjectHandle } from "./ipc";

/** Jump the UI to one claude — App.svelte passes its own select path in. */
export type Reveal = (handle: ProjectHandle, id: number) => void;

const keyOf = (handle: ProjectHandle, id: number) => `${handle}:${id}`;

/** Sessions currently blocked, so we notify on the *transition* into `needs`
 *  rather than re-announcing the same question on every store update. */
let blocked = new Set<string>();
/** Whether `blocked` has been primed. The first sweep only records state: at
 *  launch, restored sessions can already be in `needs`, and firing a banner per
 *  one would bury the live event we actually care about under stale ones. */
let primed = false;
let granted = false;

/** One instance's ask, resolved to display strings. */
interface Ask {
  handle: ProjectHandle;
  id: number;
  title: string;
  body: string;
}

function describe(p: ProjectState, id: number, name: string | null): Ask {
  // Lead with *which* claude: a stack of banners is only scannable if the first
  // line distinguishes them. Unnamed instances fall back to their hub id.
  const who = name ?? `#${id}`;
  const task = p.tasks.get(id)?.trim();
  return {
    handle: p.handle,
    id,
    title: `${who} needs you`,
    body: task ? `${p.name} · ${task}` : p.name,
  };
}

/**
 * Recompute who is blocked, push the dock badge, and announce anything newly
 * blocked. Returns the asks that deserve a banner (empty on the priming pass).
 *
 * `blocked` is replaced *before* any await so concurrent sweeps can't both see
 * the same session as new and double-notify.
 */
function collect(m: Map<ProjectHandle, ProjectState>): Ask[] {
  const next = new Set<string>();
  const fresh: Ask[] = [];
  for (const p of m.values()) {
    for (const s of p.sessions) {
      if (s.muted) continue;
      if (p.statuses.get(s.id) !== "needs") continue;
      const k = keyOf(p.handle, s.id);
      next.add(k);
      if (!blocked.has(k)) fresh.push(describe(p, s.id, s.name));
    }
  }
  const priming = !primed;
  blocked = next;
  primed = true;
  return priming ? [] : fresh;
}

/**
 * Wire up the dock badge and notifications. Returns a teardown function.
 *
 * `reveal` is called when a notification is clicked, with the project and
 * session that asked — App.svelte passes its own selection path so a click lands
 * you on the question exactly as clicking the sidebar row would.
 */
export async function initAttention(reveal: Reveal): Promise<() => void> {
  const win = getCurrentWindow();

  granted = await isPermissionGranted();
  if (!granted) granted = (await requestPermission()) === "granted";

  // Clicking a banner: raise Mulpex, then jump to the claude that sent it. The
  // window may be hidden or minimised, so `show`/`unminimize` both run before
  // `setFocus` — `setFocus` alone won't restore a minimised window.
  const action = await onAction((n) => {
    const extra = n.extra as { handle?: number; id?: number } | undefined;
    if (extra?.handle == null || extra.id == null) return;
    void (async () => {
      await win.show().catch(() => {});
      await win.unminimize().catch(() => {});
      await win.setFocus().catch(() => {});
      reveal(extra.handle as ProjectHandle, extra.id as number);
    })();
  });

  // macOS clears the badge when passed no count, so 0 must become `undefined`
  // rather than a literal 0 — otherwise the dock shows a permanent "0".
  const stopBadge = blockedTotal.subscribe((n) => {
    void win.setBadgeCount(n > 0 ? n : undefined).catch(() => {});
  });

  const stopNotify = projects.subscribe((m) => {
    const fresh = collect(m);
    if (!granted || fresh.length === 0) return;
    void (async () => {
      // Nothing to announce while you're already looking at Mulpex — the
      // sidebar dot and tab badge have already told you.
      if (await win.isFocused().catch(() => false)) return;
      for (const ask of fresh) {
        // No `sound`: a banner per blocked claude is already enough with several
        // instances running, and macOS lets you add a sound per-app if you want.
        sendNotification({
          title: ask.title,
          body: ask.body,
          extra: { handle: ask.handle, id: ask.id },
        });
      }
    })();
  });

  return () => {
    stopBadge();
    stopNotify();
    void action.unregister().catch(() => {});
  };
}
