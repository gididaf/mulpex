// ⌘⇧B Ask All Claudes: the round in flight and its summary.
//
// The backend (`broadcast.rs`) sends the question, decides each row's state and
// runs the Sonnet summary; this module polls it while a round is open — whether
// or not the popup is showing — and starts the summary by itself once every row
// is final. Memory only: a relaunch forgets the round.
//
// One round per host: ⌘⇧B asks the claudes of the host in front (0 = this
// Mulpex, else a remote host's slot, where the round and its summary run), and
// a round on one host keeps going while another host is in front.

import { get, writable } from "svelte/store";
import {
  broadcastCancel,
  broadcastProgress,
  broadcastSend,
  broadcastSummarize,
  type BroadcastRound,
  type BroadcastSummaryEvent,
  type ProjectHandle,
} from "./ipc";

const POLL_MS = 1000;

export interface Summary {
  reqId: string | null;
  state: "idle" | "streaming" | "done" | "error";
  text: string;
  error: string | null;
  /** How many answers it was written from, so a summary made early ("Summarize
   *  now") is redone once the rest are in. */
  answers: number;
}

export const noSummary: Summary = { reqId: null, state: "idle", text: "", error: null, answers: 0 };

/** Each host's round, by slot; absent = none. */
export const rounds = writable<Map<number, BroadcastRound>>(new Map());
/** Each host's summary, by slot; absent = `noSummary`. */
export const summaries = writable<Map<number, Summary>>(new Map());

const timers = new Map<number, ReturnType<typeof setInterval>>();

const answered = (r: BroadcastRound) => r.rows.filter((x) => x.reply != null).length;

function setRound(host: number, r: BroadcastRound | null) {
  rounds.update((m) => {
    const n = new Map(m);
    if (r) n.set(host, r);
    else n.delete(host);
    return n;
  });
}

function setSummary(host: number, f: (s: Summary) => Summary) {
  summaries.update((m) => new Map(m).set(host, f(m.get(host) ?? noSummary)));
}

async function poll(host: number) {
  let r: BroadcastRound | null;
  try {
    r = await broadcastProgress(host);
  } catch {
    return;
  }
  setRound(host, r);
  if (!r) {
    stopPolling(host);
    return;
  }
  const s = get(summaries).get(host) ?? noSummary;
  if (r.complete) {
    stopPolling(host);
    const n = answered(r);
    if (n > 0 && s.state !== "streaming" && (s.state === "idle" || s.answers < n)) void summarize(host);
  }
}

function startPolling(host: number) {
  if (!timers.has(host)) timers.set(host, setInterval(() => void poll(host), POLL_MS));
  void poll(host);
}

function stopPolling(host: number) {
  const t = timers.get(host);
  if (t != null) clearInterval(t);
  timers.delete(host);
}

/** Ask the picked claudes on `host`, replacing any earlier round there. */
export async function sendBroadcast(host: number, message: string, targets: { handle: ProjectHandle; id: number }[]) {
  await broadcastSend(message, targets, host);
  setSummary(host, () => noSummary);
  startPolling(host);
}

/** Summarize what has arrived so far. */
export async function summarize(host: number) {
  const r = get(rounds).get(host);
  if (!r) return;
  const reqId = crypto.randomUUID();
  setSummary(host, () => ({ reqId, state: "streaming", text: "", error: null, answers: answered(r) }));
  try {
    await broadcastSummarize(reqId, host);
  } catch (e) {
    setSummary(host, (s) => (s.reqId === reqId ? { ...s, state: "error", error: String(e) } : s));
  }
}

/** A remote host's summary arrives as the same event as a local one; the
 *  `reqId` says whose it is. */
export function applySummaryEvent(e: BroadcastSummaryEvent) {
  for (const [host, s] of get(summaries)) {
    if (s.reqId !== e.reqId) continue;
    setSummary(host, (s) => {
      if (e.kind === "delta") return { ...s, text: s.text + e.text };
      if (e.kind === "done") return { ...s, state: "done" };
      return { ...s, state: "error", error: e.text || "failed" };
    });
  }
}

/** End the round (backend takes back unread questions) and clear it. */
export async function endBroadcast(host: number) {
  stopPolling(host);
  await broadcastCancel(host);
  setRound(host, null);
  setSummary(host, () => noSummary);
}

/** Opening the popup: refresh from the backend, and keep polling if unfinished. */
export function resumeBroadcast(host: number) {
  void poll(host).then(() => {
    const r = get(rounds).get(host);
    if (r && !r.complete) startPolling(host);
  });
}
