// ⌘E Explain Selection: per-instance side-panel state. The explanation itself
// is written by a hidden Sonnet fork of the instance's conversation
// (`src-tauri/src/explain.rs`) and streams in as `explain-progress` events.
//
// Memory only: gone on quit, instance close, or project close. One thread per
// instance — a new ⌘E replaces it, and the old fork is killed backend-side.
// Follow-ups extend the thread by resuming that fork (the conversation is then
// read back from the prompt cache). Closing the panel deletes the fork.

import { get, writable } from "svelte/store";
import { explainClose, explainFollowup, explainStart, type ExplainProgressEvent } from "./ipc";

export interface ExplainTurn {
  /** The follow-up question; null on the first turn, which explains the quote. */
  question: string | null;
  /** The answer so far (markdown). */
  text: string;
  state: "streaming" | "done" | "error";
  /** On `error`: why. */
  error: string | null;
}

export interface Explain {
  /** The request in flight (or the last one), so a late event from a replaced
   *  one is ignored. */
  reqId: string;
  /** The selected text, shown as a quote on top. */
  quote: string;
  turns: ExplainTurn[];
}

/** Keyed `"<projectHandle>:<instanceId>"`, like `pins`. */
export const explains = writable<Map<string, Explain>>(new Map());

export const explainKey = (handle: number, id: number) => `${handle}:${id}`;

function set(key: string, e: Explain | null) {
  explains.update((m) => {
    const next = new Map(m);
    if (e) next.set(key, e);
    else next.delete(key);
    return next;
  });
}

/** Change the last turn of `key`'s thread, if `reqId` is still its request. */
function editLast(key: string, reqId: string, f: (t: ExplainTurn) => ExplainTurn) {
  const cur = get(explains).get(key);
  if (!cur || cur.reqId !== reqId || !cur.turns.length) return;
  const turns = cur.turns.slice();
  turns[turns.length - 1] = f(turns[turns.length - 1]);
  set(key, { ...cur, turns });
}

const newTurn = (question: string | null): ExplainTurn => ({
  question,
  text: "",
  state: "streaming",
  error: null,
});

/** Open (or replace) claude#id's panel and start explaining `quote`. */
export async function startExplain(handle: number, id: number, quote: string) {
  const key = explainKey(handle, id);
  const reqId = crypto.randomUUID();
  set(key, { reqId, quote, turns: [newTurn(null)] });
  try {
    await explainStart(handle, id, reqId, quote);
  } catch (err) {
    editLast(key, reqId, (t) => ({ ...t, state: "error", error: String(err) }));
  }
}

/** Ask a follow-up in claude#id's panel. Ignored while an answer is streaming. */
export async function askFollowup(handle: number, id: number, question: string) {
  const key = explainKey(handle, id);
  const cur = get(explains).get(key);
  const q = question.trim();
  if (!cur || !q || isStreaming(cur)) return;
  const reqId = crypto.randomUUID();
  set(key, { ...cur, reqId, turns: [...cur.turns, newTurn(q)] });
  try {
    await explainFollowup(handle, id, reqId, q);
  } catch (err) {
    editLast(key, reqId, (t) => ({ ...t, state: "error", error: String(err) }));
  }
}

export const isStreaming = (e: Explain) => e.turns[e.turns.length - 1]?.state === "streaming";

export function applyExplainProgress(ev: ExplainProgressEvent) {
  const key = explainKey(ev.handle, ev.id);
  if (ev.kind === "delta") editLast(key, ev.reqId, (t) => ({ ...t, text: t.text + ev.text }));
  else if (ev.kind === "done") editLast(key, ev.reqId, (t) => ({ ...t, state: "done" }));
  else editLast(key, ev.reqId, (t) => ({ ...t, state: "error", error: ev.text || "unknown error" }));
}

/** ✕ / Esc: close the panel, stop its explain and delete its fork. */
export function closeExplain(handle: number, id: number) {
  if (!get(explains).has(explainKey(handle, id))) return;
  explainClose(handle, id).catch(() => {});
  set(explainKey(handle, id), null);
}

/** Forget panels for a gone instance, or a whole closed project, stopping any
 *  fork still running for them. */
export function dropExplains(handle: number, id?: number) {
  for (const k of get(explains).keys()) {
    const [h, i] = k.split(":").map(Number);
    if (h === handle && (id == null || i === id)) closeExplain(h, i);
  }
}
