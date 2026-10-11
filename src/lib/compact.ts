// ⌘⇧K Request Compact. A claude can't run /compact on itself, so Mulpex does it
// in three steps:
//
//   requested   the hook adds "stop at a checkpoint" to every tool call
//               (`mulpex_core::hook::COMPACT_NOTE`) until the turn ends; an idle
//               claude skips straight to the next step
//   compacting  the turn ended (status `waiting`, input box empty): Mulpex typed
//               `/compact`
//   resume      the compaction's `SessionStart` arrived (the hook flips the file);
//               Mulpex types RESUME and the request is done
//
// The phase lives in `compactreq/<id>` so the hook and the phone see it; this
// module drives the typing, because only the frontend can read the input box
// (`promptbox.ts`). Nothing here survives a reload — a request is in-memory by
// design, and a fresh scratch dir drops the file with it.

import { derived, get, writable } from "svelte/store";
import { activeProjectHandle, projects } from "./stores";
import { terminals } from "./terminals";
import { sendBytes, setCompactPhase, type CompactPhase, type ProjectHandle } from "./ipc";

const RESUME =
  "The conversation was just compacted at your checkpoint. Continue the task from where you stopped.";

/** No `PreCompact` this long after typing `/compact` = it never started. */
const START_TIMEOUT_MS = 15_000;
/** A compaction that has not ended after this long is given up on. */
const RUN_TIMEOUT_MS = 15 * 60_000;
/** How long "compact didn't run" stays under the row. */
const NOTICE_MS = 6_000;
/** Claude Code's answer when there is too little to compact (fires `PreCompact`
 *  and then nothing — no `SessionStart` ever ends it). */
const REFUSED = /Not enough messages to compact/i;

type Phase = CompactPhase | "failed";
interface Req {
  phase: Phase;
  /** When `/compact` was typed. */
  typedAt: number;
  /** `PreCompact` was seen after typing. */
  sawStart: boolean;
  /** The refusal text was already on screen when `/compact` was typed, so
   *  seeing it proves nothing. */
  refusedBefore: boolean;
}

const reqs = writable<Map<string, Req>>(new Map());
const keyOf = (h: ProjectHandle, id: number) => `${h}:${id}`;
const parse = (k: string): [ProjectHandle, number] => {
  const [h, id] = k.split(":");
  return [Number(h) as ProjectHandle, Number(id)];
};

function put(h: ProjectHandle, id: number, r: Req | null) {
  reqs.update((m) => {
    const next = new Map(m);
    if (r) next.set(keyOf(h, id), r);
    else next.delete(keyOf(h, id));
    return next;
  });
  ensureTimer();
}

function setPhase(h: ProjectHandle, id: number, r: Req, phase: Phase) {
  put(h, id, { ...r, phase });
  void setCompactPhase(h, id, phase === "failed" ? null : phase);
}

function fail(h: ProjectHandle, id: number, r: Req) {
  setPhase(h, id, r, "failed");
  setTimeout(() => {
    if (get(reqs).get(keyOf(h, id))?.phase === "failed") put(h, id, null);
  }, NOTICE_MS);
}

/** ⌘⇧K: start a request on claude#id, or cancel the one it has. Returns why it
 *  can't, for a toast. A cancel after `/compact` was typed can't stop the
 *  compaction itself — it only skips the resume prompt. */
export function toggleCompact(h: ProjectHandle, id: number): string | null {
  const s = get(projects).get(h)?.sessions.find((x) => x.id === id);
  if (!s || s.kind === "shell" || s.exited || s.failed) return "No claude to compact";
  const cur = get(reqs).get(keyOf(h, id));
  if (cur && cur.phase !== "failed") {
    put(h, id, null);
    void setCompactPhase(h, id, null);
    return null;
  }
  const r: Req = { phase: "requested", typedAt: 0, sawStart: false, refusedBefore: false };
  setPhase(h, id, r, "requested");
  step(h, id);
  return null;
}

/** Drop claude#id's request (the row exited). */
export function dropCompact(h: ProjectHandle, id: number) {
  if (get(reqs).has(keyOf(h, id))) put(h, id, null);
}

function ready(h: ProjectHandle, id: number): boolean {
  const p = get(projects).get(h);
  return p?.statuses.get(id) === "waiting" && terminals.promptBox(h, id) === "empty";
}

/** Advance one request as far as the claude's state allows. */
function step(h: ProjectHandle, id: number) {
  let r = get(reqs).get(keyOf(h, id));
  if (!r || r.phase === "failed") return;
  const p = get(projects).get(h);
  const s = p?.sessions.find((x) => x.id === id);
  if (!p || !s || s.exited) return put(h, id, null);
  const entry = p.hub?.statuses.find((e) => e.id === id);

  if (r.phase === "requested") {
    if (!ready(h, id)) return;
    sendBytes(h, id, new TextEncoder().encode("/compact\r"));
    const refusedBefore = REFUSED.test(terminals.screenText(h, id));
    put(h, id, { phase: "compacting", typedAt: Date.now(), sawStart: false, refusedBefore });
    void setCompactPhase(h, id, "compacting");
    return;
  }

  if (r.phase === "compacting") {
    // The hook flipped the file: the compaction we typed has ended.
    if (entry?.compact_phase === "resume") {
      put(h, id, { ...r, phase: "resume" });
      return step(h, id);
    }
    // The file can lag our own write by a poll; only the hook moves it on.
    const since = Date.now() - r.typedAt;
    if (entry?.compacting && !r.sawStart) {
      r = { ...r, sawStart: true };
      put(h, id, r);
    }
    if (!r.refusedBefore && REFUSED.test(terminals.screenText(h, id))) return fail(h, id, r);
    if (!r.sawStart && since > START_TIMEOUT_MS) return fail(h, id, r);
    // Started, then the flag cleared with no `SessionStart` (the next prompt or
    // turn end clears a refused one).
    if (r.sawStart && entry && !entry.compacting) return fail(h, id, r);
    if (since > RUN_TIMEOUT_MS) return fail(h, id, r);
    return;
  }

  if (r.phase === "resume") {
    if (!ready(h, id)) return;
    sendBytes(h, id, new TextEncoder().encode(`\x1b[200~${RESUME}\x1b[201~\r`));
    put(h, id, null);
    void setCompactPhase(h, id, null);
  }
}

// A poll, not just the hub update: the input box emptying (the user deleting a
// draft) changes nothing the hub reports.
let timer: ReturnType<typeof setInterval> | null = null;
function ensureTimer() {
  const any = get(reqs).size > 0;
  if (any && !timer) {
    timer = setInterval(() => {
      for (const k of get(reqs).keys()) step(...parse(k));
    }, 300);
  } else if (!any && timer) {
    clearInterval(timer);
    timer = null;
  }
}

const LABEL: Record<Phase, string> = {
  requested: "compact: waiting for checkpoint…",
  compacting: "compacting…",
  resume: "compacting…",
  failed: "compact didn't run",
};

/** The active project's compact-request lines, by instance id. */
export const compactLines = derived([reqs, activeProjectHandle], ([$m, $h]) => {
  const out = new Map<number, { text: string; failed: boolean }>();
  if ($h == null) return out;
  for (const [k, r] of $m) {
    const [h, id] = parse(k);
    if (h === $h) out.set(id, { text: LABEL[r.phase], failed: r.phase === "failed" });
  }
  return out;
});
