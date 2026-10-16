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
//
// A round can also run on a remote HOST (⌘⇧Q with one of its tabs in front):
// the host stops its own claudes and this window runs the panel through
// `ipc.ts::call`. That round's final button restarts the host (`restart`
// mode), whose checkpoint then wakes by itself — nobody is at its screen.
// Rows' statuses come from the same `projects` map either way, since a host's
// handles are lifted into it.

import { derived, get, writable } from "svelte/store";
import { listen } from "@tauri-apps/api/event";
import {
  checkpointCancel,
  checkpointCommit,
  checkpointDismiss,
  checkpointPending,
  checkpointProgress,
  checkpointStop,
  checkpointWake,
  quitApp,
  restartApp,
  type CheckpointProgress,
  type CheckpointTarget,
} from "./ipc";
import { projects } from "./stores";
import { applyUpdate, updateState } from "./updater";
import { hosts } from "./hosts";

/** `restart`: a remote host's round, which ends in restarting that host. */
export type CheckpointMode = "quit" | "update" | "restart";

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
  /** Whose claudes: 0 this Mulpex, else a remote host's slot. */
  host: number;
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
  host: 0,
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
/** A round someone ELSE runs on this Mulpex's claudes — a remote desktop's
 *  ⌘⇧Q, or a remote-ordered update (`checkpoint-changed`). Holds compacts here
 *  exactly like our own round does. */
const heldElsewhere = writable(false);

export const checkpointOpen = derived(session, ($s) => $s.open);
export const checkpointMeta = derived([session, hosts], ([$s, $h]) => {
  const host = $s.host ? $h.get($s.host) : undefined;
  const lost = !!$s.host && host?.state !== "connected" && !$s.leaving;
  return {
    mode: $s.mode,
    host: $s.host,
    hostName: host?.name ?? null,
    sentAt: $s.sentAt,
    leaving: $s.leaving,
    /** The host's link is down: nothing can reach it until it is back. */
    lost,
    error: lost
      ? `Lost the connection to ${host?.name ?? "the host"}. Its claudes stay stopped — Cancel once it is back to let them carry on (Cancel now only closes this).`
      : $s.error,
  };
});

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
  const host = get(session).host;
  if (host && get(hosts).get(host)?.state !== "connected") {
    now.set(Date.now());
    return;
  }
  try {
    const list = await checkpointProgress(host);
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
 * panel is up only switches the mode — it does not ask everyone again. `host`
 * runs the round on a remote host instead (its mode is always `restart`).
 * Throws a sentence for the user when it can't start. */
export async function startCheckpoint(mode: CheckpointMode = "quit", host = 0): Promise<void> {
  const cur = get(session);
  if (cur.open) {
    if (cur.host !== host) {
      const where = (slot: number) => (slot ? (get(hosts).get(slot)?.name ?? "a remote host") : "this Mulpex");
      throw `A checkpoint is already running on ${where(cur.host)} — finish or cancel it first.`;
    }
    if (!host) session.update((s) => ({ ...s, mode }));
    return;
  }
  if (host) {
    const st = get(hosts).get(host);
    if (st?.state !== "connected") throw `${st?.name ?? "That host"} isn't connected.`;
    mode = "restart";
  } else {
    // Held before the messages go out: a claude may stop before this resolves.
    quitting.set(true);
  }
  let targets: CheckpointTarget[];
  try {
    targets = await checkpointStop(host);
  } catch (e) {
    quitting.set(false);
    throw e;
  }
  session.set({ ...closed, open: true, host, mode, targets, sentAt: Date.now() });
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
  const host = get(session).host;
  close();
  try {
    await checkpointCancel(host);
  } catch (e) {
    // A host that is unreachable: its claudes stay stopped (the panel said so).
    if (!host) throw e;
    console.error("checkpoint cancel on host failed:", e);
  }
}

/** The final button: record who was asked, then quit or restart into the update. */
export async function finishCheckpoint(): Promise<void> {
  const s = get(session);
  session.update((x) => ({ ...x, leaving: true, error: null }));
  try {
    await checkpointCommit(s.host);
    // The host answers before it goes down (`rpc.rs::RESTART_DELAY`); the tabs
    // grey out, reconnect and rebuild on their own, and its claudes wake.
    if (s.mode === "restart") await restartApp(s.host);
  } catch (e) {
    session.update((x) => ({ ...x, leaving: false, error: String(e) }));
    return;
  }
  if (s.mode === "restart") {
    close();
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
  [quitting, heldElsewhere, resumeKnown, resumeTargets],
  ([$q, $e, $k, $t]) => $q || $e || !$k || $t.length > 0,
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

/** "Wake all" — each claude gets "Mulpex is back" once its listener is up.
 *  `host` answers a remote host's offer instead. */
export async function wakeAll(host = 0): Promise<number> {
  setOffer(host, []);
  return checkpointWake(host);
}

/** "Not now" — forget the checkpoint. */
export async function dismissResume(host = 0): Promise<void> {
  setOffer(host, []);
  await checkpointDismiss(host);
}

/** Each remote host's "Wake all" offer — a host relaunched at a checkpoint
 *  shows it on its own screen, where nobody may be. Answering on either side
 *  closes it on both (`checkpoint-changed`). */
export const hostResumeTargets = writable<Map<number, CheckpointTarget[]>>(new Map());

function setOffer(host: number, targets: CheckpointTarget[]) {
  if (!host) {
    resumeTargets.set(targets);
    return;
  }
  hostResumeTargets.update((m) => {
    const n = new Map(m);
    if (targets.length) n.set(host, targets);
    else n.delete(host);
    return n;
  });
}

async function readHostOffer(host: number) {
  try {
    setOffer(host, await checkpointPending(host));
  } catch (e) {
    console.error(`checkpoint pending on host ${host} failed:`, e);
  }
}

/** Once, at startup: follow rounds and offers on this Mulpex and every host. */
export function initCheckpointEvents(): () => void {
  const un = [
    // This Mulpex's own: someone else started or ended a round, or answered
    // the wake offer from afar.
    listen<{ active: boolean }>("checkpoint-changed", (e) => {
      heldElsewhere.set(e.payload.active);
      if (!e.payload.active && get(resumeTargets).length) void checkResume();
    }),
    listen<{ slot: number }>("host-checkpoint-changed", (e) => void readHostOffer(e.payload.slot)),
  ];
  const connected = new Set<number>();
  const stopHosts = hosts.subscribe((m) => {
    for (const st of m.values()) {
      const on = st.state === "connected";
      if (on && !connected.has(st.slot)) void readHostOffer(st.slot);
      if (!on && connected.has(st.slot)) setOffer(st.slot, []);
      if (on) connected.add(st.slot);
      else connected.delete(st.slot);
    }
    for (const slot of [...connected]) {
      if (!m.has(slot)) {
        connected.delete(slot);
        setOffer(slot, []);
      }
    }
  });
  return () => {
    stopHosts();
    un.forEach((p) => p.then((f) => f()));
  };
}
