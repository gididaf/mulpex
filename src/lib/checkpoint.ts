// ⌘⇧Q Checkpoint & Quit, and the "drive again" offer after the relaunch.
//
// The backend (`checkpoint.rs`) sends the hub messages and records who was
// asked; this module watches them stop. A claude counts as stopped once the stop
// message is read (its inbox file is gone) AND its status is `waiting` — a
// `watching` claude included, since a watcher is idle. If a monitor event wakes
// it again its row simply flips back, live: nothing here latches.
//
// `mode` is what the final button does: quit, or restart into a downloaded
// update (the update banner's "Checkpoint & update").

import { derived, get, writable } from "svelte/store";
import {
  checkpointCancel,
  checkpointCommit,
  checkpointDismiss,
  checkpointPending,
  checkpointProgress,
  checkpointStop,
  checkpointWake,
  quitApp,
  type CheckpointProgress,
  type CheckpointTarget,
} from "./ipc";
import { projects } from "./stores";
import { applyUpdate, updateState } from "./updater";

export type CheckpointMode = "quit" | "update";

/** How often the panel asks the backend which stop messages are still unread. */
const POLL_MS = 1000;
/** A message nobody has read after this long is "stuck", not "on its way". */
const UNREAD_STUCK_MS = 60_000;
/** A claude still working this long after reading it is "stuck" too. */
const WORKING_STUCK_MS = 5 * 60_000;
/** How long a read message must stay read-and-idle before the row counts. */
const SETTLE_MS = 1500;

export type RowState =
  /** Exited — nothing left to stop. */
  | "exited"
  /** Read the message and is idle. */
  | "stopped"
  /** Asking the user something; only the user can move it. */
  | "needs"
  /** Read the message, still finishing its step. */
  | "stopping"
  /** Mid-turn; it reads the message at its next tool call or turn end. */
  | "busy"
  /** Idle with a live listener — the message is on its way. */
  | "delivering"
  /** Idle with NO listener: it will never see the message by itself. */
  | "deaf";

export interface Row extends CheckpointTarget {
  state: RowState;
  done: boolean;
  /** Not done, and not going to be without help. */
  stuck: boolean;
  /** A claude that is idle while holding a monitor (agentalk, …). */
  watching: boolean;
  /** One line under the row saying what to do, when there is something. */
  hint: string | null;
}

interface Session {
  open: boolean;
  mode: CheckpointMode;
  targets: CheckpointTarget[];
  sentAt: number;
  progress: Map<string, CheckpointProgress>;
  /** key → when its message was first seen read. */
  readAt: Map<string, number>;
  /** The final button was pressed and the quit/update is under way. */
  leaving: boolean;
  error: string | null;
}

const closed: Session = {
  open: false,
  mode: "quit",
  targets: [],
  sentAt: 0,
  progress: new Map(),
  readAt: new Map(),
  leaving: false,
  error: null,
};

const session = writable<Session>({ ...closed });
/** Ticks once a second while the panel is open, so ages and "stuck" advance. */
const now = writable(Date.now());
let timer: ReturnType<typeof setInterval> | null = null;

const key = (h: number, id: number) => `${h}:${id}`;

/** Set before the stop messages go out, cleared when the panel closes. */
const quitting = writable(false);

export const checkpointOpen = derived(session, ($s) => $s.open);
export const checkpointMeta = derived(session, ($s) => ({
  mode: $s.mode,
  sentAt: $s.sentAt,
  leaving: $s.leaving,
  error: $s.error,
}));

export const checkpointRows = derived([session, projects, now], ([$s, $p, $now]) => {
  const rows: Row[] = [];
  for (const t of $s.targets) {
    const k = key(t.handle, t.id);
    const pr = $s.progress.get(k);
    const proj = $p.get(t.handle);
    const status = proj?.statuses.get(t.id) ?? "waiting";
    const watching = proj?.watching.has(t.id) ?? false;
    // Until the first progress poll lands, everything is unread and listening.
    const alive = pr?.alive ?? true;
    const unread = pr?.unread ?? true;
    const listener = pr?.listener ?? true;
    const sinceSent = $now - $s.sentAt;
    const sinceRead = $now - ($s.readAt.get(k) ?? $now);

    let state: RowState;
    if (!alive) state = "exited";
    else if (status === "needs") state = "needs";
    // The read and the status come from two different polls, so a claude can
    // look read-and-idle for a beat before its `working` lands. Hold "stopped"
    // back briefly so Quit never lights up on that flash.
    else if (!unread) state = status === "waiting" && sinceRead >= SETTLE_MS ? "stopped" : "stopping";
    else if (status === "working") state = "busy";
    else state = listener ? "delivering" : "deaf";

    const done = state === "exited" || state === "stopped";
    let hint: string | null = null;
    let stuck = false;
    switch (state) {
      case "needs":
        stuck = true;
        hint = "Waiting for your answer — click to open it.";
        break;
      case "deaf":
        stuck = true;
        hint = "Its hub listener isn't running, so it can't see the message. Type anything into it to wake it.";
        break;
      case "delivering":
        if (sinceSent > UNREAD_STUCK_MS) {
          stuck = true;
          hint = "Message not read after a minute — click to check on it.";
        }
        break;
      case "busy":
        if (sinceSent > WORKING_STUCK_MS) {
          stuck = true;
          hint = "Still mid-turn and hasn't read the message — click to check on it.";
        }
        break;
      case "stopping":
        if (sinceRead > WORKING_STUCK_MS) {
          stuck = true;
          hint = "Read the message but still working — click to check on it.";
        }
        break;
    }
    rows.push({ ...t, state, done, stuck, watching, hint });
  }
  return rows;
});

export const checkpointSummary = derived(checkpointRows, ($rows) => {
  const done = $rows.filter((r) => r.done).length;
  const left = $rows.filter((r) => !r.done);
  return {
    total: $rows.length,
    done,
    allDone: left.length === 0,
    // "Quit anyway" appears only once everyone who can stop has, and the rest
    // are stuck — never as a way to skip claudes that are still on their way.
    onlyStuckLeft: left.length > 0 && left.every((r) => r.stuck),
  };
});

async function poll(): Promise<void> {
  try {
    const list = await checkpointProgress();
    const t = Date.now();
    session.update((s) => {
      if (!s.open) return s;
      const progress = new Map(list.map((p) => [key(p.handle, p.id), p]));
      const readAt = new Map(s.readAt);
      for (const [k, p] of progress) {
        if (!p.unread && !readAt.has(k)) readAt.set(k, t);
      }
      return { ...s, progress, readAt };
    });
  } catch (e) {
    console.error("checkpoint progress failed:", e);
  }
  now.set(Date.now());
}

/** ⌘⇧Q, or the update banner's "Checkpoint & update". A second press while the
 * panel is up only switches the mode — it does not ask everyone again. */
export async function startCheckpoint(mode: CheckpointMode = "quit"): Promise<void> {
  if (get(session).open) {
    session.update((s) => ({ ...s, mode }));
    return;
  }
  // Held before the messages go out: a claude may stop before this resolves.
  quitting.set(true);
  let targets: CheckpointTarget[];
  try {
    targets = await checkpointStop();
  } catch (e) {
    quitting.set(false);
    throw e;
  }
  session.set({ ...closed, open: true, mode, targets, sentAt: Date.now() });
  if (timer) clearInterval(timer);
  timer = setInterval(() => void poll(), POLL_MS);
  void poll();
}

function close(): void {
  if (timer) clearInterval(timer);
  timer = null;
  session.set({ ...closed });
  quitting.set(false);
}

/** Never mind: unread stop messages are taken back, the rest told to carry on. */
export async function cancelCheckpoint(): Promise<void> {
  close();
  await checkpointCancel();
}

/** The final button: record who was asked, then quit or restart into the update. */
export async function finishCheckpoint(): Promise<void> {
  const s = get(session);
  session.update((x) => ({ ...x, leaving: true, error: null }));
  try {
    await checkpointCommit();
  } catch (e) {
    session.update((x) => ({ ...x, leaving: false, error: String(e) }));
    return;
  }
  if (s.mode === "quit") {
    await quitApp();
    return;
  }
  // `applyUpdate` restarts on success and parks the banner in `error` on
  // failure. On failure nothing quit, so the checkpoint must not be offered on
  // some later launch — and the claudes are still stopped, so keep the panel up
  // for a Cancel.
  await applyUpdate(true);
  if (get(updateState).phase === "error") {
    await checkpointDismiss();
    session.update((x) => ({
      ...x,
      leaving: false,
      error: "The update failed (see the update card). Nothing quit — Cancel to let the claudes continue.",
    }));
  }
}

// ---- after the relaunch: "drive again" ----

/** The claudes from the last checkpoint that came back, for the resume dialog. */
export const resumeTargets = writable<CheckpointTarget[]>([]);
/** `checkResume` has answered (hub updates arrive before it does). */
const resumeKnown = writable(false);

/** True while no compact may be typed: from the moment the stop messages go out
 * until the panel closes, and after a relaunch until the wake offer is answered.
 * A compact ends with "continue the task", which undoes the stop — on 2026-10-12
 * auto-compact typed `/compact` into cloudraw#157 the second it stopped for the
 * quit, and it went back to work. `compact.ts` obeys this. */
export const compactsHeld = derived(
  [quitting, resumeKnown, resumeTargets],
  ([$q, $k, $t]) => $q || !$k || $t.length > 0,
);

/** After bootstrap: is there a checkpoint to offer waking? */
export async function checkResume(): Promise<void> {
  try {
    resumeTargets.set(await checkpointPending());
  } catch (e) {
    console.error("checkpoint pending failed:", e);
  } finally {
    resumeKnown.set(true);
  }
}

/** "Wake all" — each claude gets "Mulpex is back" once its listener is up. */
export async function wakeAll(): Promise<number> {
  resumeTargets.set([]);
  return checkpointWake();
}

/** "Not now" — forget the checkpoint. */
export async function dismissResume(): Promise<void> {
  resumeTargets.set([]);
  await checkpointDismiss();
}
