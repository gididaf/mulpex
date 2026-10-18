// ⌘⇧K Request Compact. A claude can't run /compact on itself, so Mulpex does it
// in three steps:
//
//   requested   the hook adds "stop at a checkpoint" to every tool call
//               (`mulpex_core::hook::COMPACT_NOTE`) until the turn ends; an idle
//               claude skips straight to the next step
//   compacting  the turn ended (status `waiting`, or `working` with only
//               background work left; input box empty): Mulpex typed
//               `/compact`
//   resume      the compaction's `SessionStart` arrived (the hook flips the file);
//               Mulpex types RESUME and the request is done
//
// The phase lives in `compactreq/<id>` so the hook and the phone see it; this
// module drives the typing, because only the frontend can read the input box
// (`promptbox.ts`). Nothing here survives a reload — a request is in-memory by
// design, and a fresh scratch dir drops the file with it.
//
// While Checkpoint & Quit holds compacts (`checkpoint.ts::compactsHeld`) nothing
// new starts, a request that hasn't typed `/compact` yet is called off, and one
// already compacting finishes without the resume prompt — that prompt tells the
// claude to carry on, which is the opposite of what the quit asked.

import { derived, get, writable } from "svelte/store";
import { activeProjectHandle, descendantsOf, projects, saveStatusOf } from "./stores";
import { terminals } from "./terminals";
import { compactsHeld } from "./checkpoint";
import { isRemote } from "./hosts";
import {
  sendBytes,
  setCompactPhase,
  type CompactPhase,
  type ProjectHandle,
  type SessionInfo,
} from "./ipc";

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
  /** The orchestrator whose auto-compact rule started this, if one did. */
  by: number | null;
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
  if (active(h, id)) {
    put(h, id, null);
    void setCompactPhase(h, id, null);
    return null;
  }
  if (get(compactsHeld)) return "Mulpex is quitting — compact after it's back";
  startCompact(h, id, null);
  return null;
}

/** A request is in flight on claude#id (a lingering "didn't run" notice isn't). */
function active(h: ProjectHandle, id: number): boolean {
  const cur = get(reqs).get(keyOf(h, id));
  return !!cur && cur.phase !== "failed";
}

/** Start a request — never a toggle. `by` is the orchestrator whose rule fired. */
function startCompact(h: ProjectHandle, id: number, by: number | null) {
  const r: Req = { phase: "requested", typedAt: 0, sawStart: false, refusedBefore: false, by };
  setPhase(h, id, r, "requested");
  step(h, id);
}

// ---- Auto-compact (a row's context-menu rule) ----
//
// The user sets "auto-compact its claudes at X%" on a row; every claude in its
// sidebar subtree whose context reaches X gets the ⌘⇧K flow above, run by Mulpex
// itself — no model ever decides to compact. One attempt per crossing: a child
// is `suppressed` after its attempt (success or not) until its context reads
// below X again, which covers both a failed compact and the ctx file still
// showing the old number right after a good one.

const suppressed = new Set<string>();

/** Apply every auto-compact rule in project `h`. Called on each hub update. */
export function autoCompactTick(h: ProjectHandle) {
  // A remote host enforces its own rules, in its own window.
  if (isRemote(h)) return;
  const p = get(projects).get(h);
  if (!p) return;
  for (const s of p.sessions) suppressedRelease(h, s.id, p.ctx.get(s.id), ruleFor(p.sessions, s.id));
  if (get(compactsHeld)) return;
  for (const orch of p.sessions) {
    const pct = orch.auto_compact;
    if (pct == null || orch.kind === "shell") continue;
    for (const c of descendantsOf(p.sessions, orch.id)) {
      if (c.kind === "shell" || c.exited || c.failed) continue;
      const k = keyOf(h, c.id);
      const ctx = p.ctx.get(c.id);
      if (ctx == null || ctx < pct || suppressed.has(k) || active(h, c.id)) continue;
      if (saveStatusOf(h, c.id)) continue;
      suppressed.add(k);
      startCompact(h, c.id, orch.id);
    }
  }
}

/** The lowest threshold any ancestor rule puts on claude#id, if any. */
function ruleFor(list: SessionInfo[], id: number): number | null {
  let best: number | null = null;
  for (const o of list) {
    if (o.auto_compact == null) continue;
    if (descendantsOf(list, o.id).some((d) => d.id === id)) {
      best = best == null ? o.auto_compact : Math.min(best, o.auto_compact);
    }
  }
  return best;
}

function suppressedRelease(h: ProjectHandle, id: number, ctx: number | undefined, pct: number | null) {
  const k = keyOf(h, id);
  if (!suppressed.has(k) || active(h, id)) return;
  if (pct == null || ctx == null || ctx < pct) suppressed.delete(k);
}

// Hub updates only come on a change, so a child already over the line when the
// hold lifts would wait for its next one. Catch up at once instead.
let wasHeld = false;
compactsHeld.subscribe((held) => {
  if (wasHeld && !held) for (const h of get(projects).keys()) autoCompactTick(h);
  wasHeld = held;
});

/** Drop claude#id's request (the row exited). */
export function dropCompact(h: ProjectHandle, id: number) {
  suppressed.delete(keyOf(h, id));
  if (get(reqs).has(keyOf(h, id))) put(h, id, null);
}

// A turn that ended with background shells still running reads `working`, but
// the claude has stopped — waiting for `waiting` would wait for the test run.
function ready(h: ProjectHandle, id: number): boolean {
  const p = get(projects).get(h);
  const status = p?.statuses.get(id);
  const bgOnly =
    status === "working" && !!p?.hub?.statuses.find((e) => e.id === id)?.background_only;
  return (status === "waiting" || bgOnly) && terminals.promptBox(h, id) === "empty";
}

/** Advance one request as far as the claude's state allows. */
function step(h: ProjectHandle, id: number) {
  let r = get(reqs).get(keyOf(h, id));
  if (!r || r.phase === "failed") return;
  const p = get(projects).get(h);
  const s = p?.sessions.find((x) => x.id === id);
  if (!p || !s || s.exited) return put(h, id, null);
  const entry = p.hub?.statuses.find((e) => e.id === id);

  if (get(compactsHeld) && (r.phase === "requested" || r.phase === "resume")) {
    // A request called off before `/compact` may try again after the hold.
    if (r.phase === "requested") suppressed.delete(keyOf(h, id));
    put(h, id, null);
    void setCompactPhase(h, id, null);
    return;
  }

  if (r.phase === "requested") {
    if (!ready(h, id)) return;
    sendBytes(h, id, new TextEncoder().encode("/compact\r"));
    const refusedBefore = REFUSED.test(terminals.screenText(h, id));
    put(h, id, { ...r, phase: "compacting", typedAt: Date.now(), sawStart: false, refusedBefore });
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

const AUTO_LABEL: Record<Phase, string> = {
  requested: "waiting for checkpoint…",
  compacting: "compacting…",
  resume: "compacting…",
  failed: "didn't run",
};

/** The active project's compact-request lines, by instance id. A remote host's
 *  requests run in ITS window, so for its projects the line comes from the
 *  phase its hub reports (`compactreq/<id>`) instead. */
export const compactLines = derived([reqs, activeProjectHandle, projects], ([$m, $h, $p]) => {
  const out = new Map<number, { text: string; failed: boolean }>();
  if ($h == null) return out;
  if (isRemote($h)) {
    for (const e of $p.get($h)?.hub?.statuses ?? []) {
      if (e.compact_phase) out.set(e.id, { text: LABEL[e.compact_phase], failed: false });
    }
    return out;
  }
  for (const [k, r] of $m) {
    const [h, id] = parse(k);
    if (h !== $h) continue;
    // An auto-compact names the orchestrator whose rule fired it.
    const text = r.by != null ? `auto-compact (claude#${r.by}): ${AUTO_LABEL[r.phase]}` : LABEL[r.phase];
    out.set(id, { text, failed: r.phase === "failed" });
  }
  return out;
});
