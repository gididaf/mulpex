// ⌘⇧B Ask All Claudes: the round in flight and its summary.
//
// The backend (`broadcast.rs`) sends the question, decides each row's state and
// runs the Sonnet summary; this module polls it while a round is open — whether
// or not the popup is showing — and starts the summary by itself once every row
// is final. Memory only: a relaunch forgets the round.

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

const noSummary: Summary = { reqId: null, state: "idle", text: "", error: null, answers: 0 };

export const round = writable<BroadcastRound | null>(null);
export const summary = writable<Summary>(noSummary);

let timer: ReturnType<typeof setInterval> | null = null;

const answered = (r: BroadcastRound) => r.rows.filter((x) => x.reply != null).length;

async function poll() {
  let r: BroadcastRound | null;
  try {
    r = await broadcastProgress();
  } catch {
    return;
  }
  round.set(r);
  if (!r) {
    stopPolling();
    return;
  }
  const s = get(summary);
  if (r.complete) {
    stopPolling();
    const n = answered(r);
    if (n > 0 && s.state !== "streaming" && (s.state === "idle" || s.answers < n)) void summarize();
  }
}

function startPolling() {
  if (timer == null) timer = setInterval(() => void poll(), POLL_MS);
  void poll();
}

function stopPolling() {
  if (timer != null) clearInterval(timer);
  timer = null;
}

/** Ask the picked claudes, replacing any earlier round. */
export async function sendBroadcast(message: string, targets: { handle: ProjectHandle; id: number }[]) {
  await broadcastSend(message, targets);
  summary.set(noSummary);
  startPolling();
}

/** Summarize what has arrived so far. */
export async function summarize() {
  const r = get(round);
  if (!r) return;
  const reqId = crypto.randomUUID();
  summary.set({ reqId, state: "streaming", text: "", error: null, answers: answered(r) });
  try {
    await broadcastSummarize(reqId);
  } catch (e) {
    summary.update((s) => (s.reqId === reqId ? { ...s, state: "error", error: String(e) } : s));
  }
}

export function applySummaryEvent(e: BroadcastSummaryEvent) {
  summary.update((s) => {
    if (s.reqId !== e.reqId) return s;
    if (e.kind === "delta") return { ...s, text: s.text + e.text };
    if (e.kind === "done") return { ...s, state: "done" };
    return { ...s, state: "error", error: e.text || "failed" };
  });
}

/** End the round (backend takes back unread questions) and clear it. */
export async function endBroadcast() {
  stopPolling();
  await broadcastCancel();
  round.set(null);
  summary.set(noSummary);
}

/** Opening the popup: refresh from the backend, and keep polling if unfinished. */
export function resumeBroadcast() {
  void poll().then(() => {
    const r = get(round);
    if (r && !r.complete) startPolling();
  });
}
