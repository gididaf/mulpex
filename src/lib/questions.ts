// Async questions: an AskUserQuestion a claude flagged `metadata.source:
// "async"` lands here as a card instead of blocking it (`mulpex_core::questions`,
// docs/hub.md). This store mirrors the backend per project; `questions-changed`
// (a new card, an edit, a cancel, an answer) re-reads it.

import { get, writable } from "svelte/store";
import { questionAnswer, questionDismiss, questionsGet, type QAnswer, type QCard } from "./ipc";
import { activeProjectHandle, flashToast, patchProject, projects } from "./stores";
import { openPanel } from "./rightPanel";

/** Open cards per project handle, oldest first. */
export const questions = writable<Map<number, QCard[]>>(new Map());

/** Cards already seen, so only a NEW one auto-opens the panel. */
const seen = new Set<string>();
const key = (h: number, c: QCard) => `${h}:${c.instance}:${c.qid}`;

function counts(list: QCard[]): Map<number, number> {
  const m = new Map<number, number>();
  for (const c of list) m.set(c.instance, (m.get(c.instance) ?? 0) + 1);
  return m;
}

export async function loadQuestions(handle: number, first = false) {
  let list: QCard[];
  try {
    list = await questionsGet(handle);
  } catch {
    return;
  }
  const fresh = list.filter((c) => !seen.has(key(handle, c)));
  for (const c of list) seen.add(key(handle, c));
  questions.update((m) => new Map(m).set(handle, list));
  patchProject(handle, { qopen: counts(list) });
  // A new question from the claude you are looking at opens the tab.
  if (!first && fresh.length && get(activeProjectHandle) === handle) {
    const focused = get(projects).get(handle)?.activeSessionId;
    if (fresh.some((c) => c.instance === focused)) openPanel("questions");
  }
}

export function dropQuestions(handle: number) {
  questions.update((m) => {
    const n = new Map(m);
    n.delete(handle);
    return n;
  });
}

export async function answerCard(handle: number, c: QCard, answers: QAnswer[]): Promise<boolean> {
  try {
    await questionAnswer(handle, c.instance, c.qid, c.rev, answers);
  } catch (e) {
    flashToast(String(e));
    void loadQuestions(handle);
    return false;
  }
  void loadQuestions(handle);
  return true;
}

export async function dismissCard(handle: number, c: QCard) {
  try {
    await questionDismiss(handle, c.instance, c.qid);
  } catch (e) {
    flashToast(String(e));
  }
  void loadQuestions(handle);
}

/** Open cards in one project — the tab badge. */
export const countIn = (m: Map<number, QCard[]>, h: number) => m.get(h)?.length ?? 0;

/** What the user has picked so far on one question. */
export interface QDraft {
  picks: string[];
  otherOn: boolean;
  other: string;
  notes: string;
  /** The option whose preview is shown (hover/focus). */
  shown: string | null;
}

/** Unsent answers, per `handle:instance:qid`, so a half-answered card survives
 *  switching tabs or projects. Not persisted: the card dies with its claude. */
const drafts = new Map<string, { rev: number; qs: QDraft[] }>();

const blank = (): QDraft => ({ picks: [], otherOn: false, other: "", notes: "", shown: null });

/** The draft for card `c`, rebuilt when claude edited it: the new questions
 *  replace the old, and picks whose label still exists are kept. */
export function draftFor(h: number, c: QCard): QDraft[] {
  const k = key(h, c);
  const d = drafts.get(k);
  if (d && d.rev === c.rev) return d.qs;
  const qs = c.questions.map((q, i) => {
    const old = d?.qs[i];
    if (!old) return blank();
    const labels = new Set(q.options.map((o) => o.label));
    const picks = old.picks.filter((p) => labels.has(p));
    return { ...old, picks: q.multiSelect ? picks : picks.slice(0, 1), shown: null };
  });
  drafts.set(k, { rev: c.rev, qs });
  return qs;
}

export function saveDraft(h: number, c: QCard, qs: QDraft[]) {
  drafts.set(key(h, c), { rev: c.rev, qs });
}

export function forgetDraft(h: number, c: QCard) {
  drafts.delete(key(h, c));
}
