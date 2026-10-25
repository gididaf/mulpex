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
//               Mulpex types RESUME and the request is done. A `/compact`
//               someone typed by hand while still `requested` counts too.
//
// The turn can end with the box still untypeable (a draft in it, or a screen
// `promptbox.ts` does not recognize). Waiting for that would leave the claude
// stopped forever — it was told to stop and wait — so after IDLE_GIVE_UP_MS
// the request gives up and tells it by hub message to carry on (`giveUp`).
// Same for a phase file with no request behind it here (a reload, a crashed
// window): `sweepOrphans` ends it the same way.
//
// A codex runs the same three steps (docs/codex.md S9: `/compact` typed into an
// empty box compacts, PreCompact/PostCompact fire, and the hook ends the phase
// the way claude's SessionStart does). Only the typing differs — see
// `compactBytes`.
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
import { addrIn, agentOf } from "./agents";
import {
  compactGiveUp,
  sendBytes,
  setCompactPhase,
  type Agent,
  type CompactPhase,
  type ProjectHandle,
  type SessionInfo,
} from "./ipc";

const RESUME =
  "The conversation was just compacted at your checkpoint. Continue the task from where you stopped.";

/** What types `/compact` into `agent`'s empty box and submits it, in ONE write.
 *  claude: the plain keys. codex: a bracketed paste, then CR — typed fast, a
 *  plain `/compact\r` lands in codex's paste-burst and the CR becomes a newline
 *  in the box instead of a submit (measured, codex 0.149.1; the paste form
 *  compacted and printed `• Context compacted`). */
function compactBytes(agent: Agent): Uint8Array {
  return new TextEncoder().encode(agent === "codex" ? "\x1b[200~/compact\x1b[201~\r" : "/compact\r");
}

/** No `PreCompact` this long after typing `/compact` = it never started. */
const START_TIMEOUT_MS = 15_000;
/** A compaction that has not ended after this long is given up on. */
const RUN_TIMEOUT_MS = 15 * 60_000;
/** The turn is over but the box can't be typed into: give up after this long. */
const IDLE_GIVE_UP_MS = 60_000;
/** How long "compact didn't run" stays under the row. */
const NOTICE_MS = 6_000;
/** A phase file this soon after we cleared it is a hub snapshot lagging our
 *  own write, not an orphan. */
const CLEAR_LAG_MS = 5_000;
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
  /** The row whose auto-compact rule started this (maybe the claude itself), if one did. */
  by: number | null;
  /** Since when the turn has been over with the box untypeable (0: it isn't). */
  idleSince: number;
  /** Why it gave up, for the notice. */
  why?: string;
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

/** When each row's phase file was last cleared by us (see CLEAR_LAG_MS). */
const cleared = new Map<string, number>();

function writePhase(h: ProjectHandle, id: number, phase: CompactPhase | null) {
  if (phase == null) cleared.set(keyOf(h, id), Date.now());
  void setCompactPhase(h, id, phase);
}

function setPhase(h: ProjectHandle, id: number, r: Req, phase: CompactPhase) {
  put(h, id, { ...r, phase });
  writePhase(h, id, phase);
}

/** The request is over without the compact it was for (or, from `resume`,
 *  without the resume prompt): claude#id stopped at a checkpoint and waits, so
 *  tell it by hub message to carry on — typing is what can't be relied on. A
 *  "didn't run" notice stays under the row for a while. */
function giveUp(h: ProjectHandle, id: number, r: Req, why: string) {
  const compacted = r.phase === "resume";
  cleared.set(keyOf(h, id), Date.now());
  void compactGiveUp(h, id, compacted, why);
  if (compacted) return put(h, id, null);
  put(h, id, { ...r, phase: "failed", why });
  setTimeout(() => {
    if (get(reqs).get(keyOf(h, id))?.phase === "failed") put(h, id, null);
  }, NOTICE_MS);
}

/** ⌘⇧K: start a request on claude#id, or cancel the one it has. Returns why it
 *  can't, for a toast. A cancel after `/compact` was typed can't stop the
 *  compaction itself — it only skips the resume prompt. */
export function toggleCompact(h: ProjectHandle, id: number): string | null {
  const s = get(projects).get(h)?.sessions.find((x) => x.id === id);
  if (!s || s.kind === "shell" || s.exited || s.failed) return "Nothing to compact — no running claude or codex";
  if (active(h, id)) {
    put(h, id, null);
    writePhase(h, id, null);
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
  const r: Req = { phase: "requested", typedAt: 0, sawStart: false, refusedBefore: false, by, idleSince: 0 };
  setPhase(h, id, r, "requested");
  step(h, id);
}

// ---- Auto-compact (a row's context-menu rule) ----
//
// The user sets "auto-compact at X%" on a row; that row and every claude in its
// sidebar subtree whose context reaches X gets the ⌘⇧K flow above, run by Mulpex
// itself — no model ever decides to compact. One attempt per crossing: a claude
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
  // Not only the timer: a status change is what most steps wait for.
  for (const k of get(reqs).keys()) {
    const [kh, id] = parse(k);
    if (kh === h) safeStep(kh, id);
  }
  sweepOrphans(h);
  for (const s of p.sessions) suppressedRelease(h, s.id, p.ctx.get(s.id), ruleFor(p.sessions, s.id));
  if (get(compactsHeld)) return;
  for (const orch of p.sessions) {
    const pct = orch.auto_compact;
    if (pct == null || orch.kind === "shell") continue;
    for (const c of [orch, ...descendantsOf(p.sessions, orch.id)]) {
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

/** The lowest threshold claude#id's own rule or any ancestor's puts on it, if any. */
function ruleFor(list: SessionInfo[], id: number): number | null {
  let best: number | null = null;
  for (const o of list) {
    if (o.auto_compact == null) continue;
    if (o.id === id || descendantsOf(list, o.id).some((d) => d.id === id)) {
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

/** Drop claude#id's request (the row exited or restarted). */
export function dropCompact(h: ProjectHandle, id: number) {
  suppressed.delete(keyOf(h, id));
  if (get(reqs).has(keyOf(h, id))) {
    put(h, id, null);
    writePhase(h, id, null);
  }
}

/** A phase file on disk that no request here is behind: the hook keeps telling
 *  that claude to stop, and nothing will ever move it on. End it, and tell the
 *  claude — it may already be stopped and waiting. */
function sweepOrphans(h: ProjectHandle) {
  const p = get(projects).get(h);
  for (const e of p?.hub?.statuses ?? []) {
    const k = keyOf(h, e.id);
    if (!e.compact_phase || get(reqs).has(k)) continue;
    if (Date.now() - (cleared.get(k) ?? 0) < CLEAR_LAG_MS) continue;
    if (!p?.sessions.some((s) => s.id === e.id && !s.exited)) continue;
    cleared.set(k, Date.now());
    void compactGiveUp(h, e.id, e.compact_phase !== "requested", "Mulpex lost track of the request");
  }
}

/** Why `/compact` (or RESUME) can't be typed into claude#id now, or null if it can.
 *  `idle` = its turn is over, so only the box stands in the way. */
function blocker(h: ProjectHandle, id: number): { idle: boolean; why: string } | null {
  const p = get(projects).get(h);
  const status = p?.statuses.get(id);
  // A turn that ended with background shells still running reads `working`, but
  // the claude has stopped — waiting for `waiting` would wait for the test run.
  const bgOnly =
    status === "working" && !!p?.hub?.statuses.find((e) => e.id === id)?.background_only;
  if (status !== "waiting" && !bgOnly) return { idle: false, why: `status ${status ?? "unknown"}` };
  const box = terminals.promptBox(h, id);
  if (box === "empty") return null;
  return { idle: true, why: box === "draft" ? "its input box has a draft" : "Mulpex can't find its input box" };
}

/** Wait for the box, but not forever: an idle row with an untypeable box gives up. */
function waitForBox(h: ProjectHandle, id: number, r: Req, b: { idle: boolean; why: string }) {
  if (!b.idle) {
    if (r.idleSince) put(h, id, { ...r, idleSince: 0 });
    return;
  }
  if (!r.idleSince) return put(h, id, { ...r, idleSince: Date.now() });
  if (Date.now() - r.idleSince > IDLE_GIVE_UP_MS) giveUp(h, id, r, b.why);
}

/** One request's step, isolated: a throw here must not stall every other row. */
function safeStep(h: ProjectHandle, id: number) {
  try {
    step(h, id);
  } catch (e) {
    console.error(`compact step ${h}:${id}`, e);
  }
}

/** Advance one request as far as the claude's state allows. */
function step(h: ProjectHandle, id: number) {
  let r = get(reqs).get(keyOf(h, id));
  if (!r || r.phase === "failed") return;
  const p = get(projects).get(h);
  const s = p?.sessions.find((x) => x.id === id);
  if (!p || !s || s.exited) {
    put(h, id, null);
    return writePhase(h, id, null);
  }
  const entry = p.hub?.statuses.find((e) => e.id === id);

  if (get(compactsHeld) && (r.phase === "requested" || r.phase === "resume")) {
    // A request called off before `/compact` may try again after the hold.
    if (r.phase === "requested") suppressed.delete(keyOf(h, id));
    put(h, id, null);
    writePhase(h, id, null);
    return;
  }

  if (r.phase === "requested") {
    // Compacted already: a `/compact` typed by hand (the hook counted it).
    if (entry?.compact_phase === "resume") {
      put(h, id, { ...r, phase: "resume", idleSince: 0 });
      return step(h, id);
    }
    const b = blocker(h, id);
    if (b) return waitForBox(h, id, r, b);
    sendBytes(h, id, compactBytes(agentOf(s.kind) ?? "claude"));
    const refusedBefore = REFUSED.test(terminals.screenText(h, id));
    put(h, id, { ...r, phase: "compacting", typedAt: Date.now(), sawStart: false, refusedBefore, idleSince: 0 });
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
    if (!r.refusedBefore && REFUSED.test(terminals.screenText(h, id))) return giveUp(h, id, r, "too little to compact");
    if (!r.sawStart && since > START_TIMEOUT_MS) return giveUp(h, id, r, "it never started");
    // Started, then the flag cleared with no `SessionStart` (the next prompt or
    // turn end clears a refused one).
    if (r.sawStart && entry && !entry.compacting) return giveUp(h, id, r, "it ended without finishing");
    if (since > RUN_TIMEOUT_MS) return giveUp(h, id, r, "it ran too long");
    return;
  }

  if (r.phase === "resume") {
    const b = blocker(h, id);
    if (b) return waitForBox(h, id, r, b);
    sendBytes(h, id, new TextEncoder().encode(`\x1b[200~${RESUME}\x1b[201~\r`));
    put(h, id, null);
    writePhase(h, id, null);
  }
}

// A poll, not just the hub update: the input box emptying (the user deleting a
// draft) changes nothing the hub reports.
let timer: ReturnType<typeof setInterval> | null = null;
function ensureTimer() {
  const any = get(reqs).size > 0;
  if (any && !timer) {
    timer = setInterval(() => {
      for (const k of get(reqs).keys()) safeStep(...parse(k));
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
    // An auto-compact names the orchestrator whose rule fired it (unless it's its own).
    const label = (l: Record<Phase, string>) => (r.why ? `${l[r.phase]}: ${r.why}` : l[r.phase]);
    const text =
      r.by == null
        ? label(LABEL)
        : r.by === id
          ? `auto-compact: ${label(AUTO_LABEL)}`
          : `auto-compact (${addrIn($p.get(h)?.sessions ?? [], r.by)}): ${label(AUTO_LABEL)}`;
    out.set(id, { text, failed: r.phase === "failed" });
  }
  return out;
});
