// "For me": each claude's list of what it needs from the user (an action, a
// decision, a question), written by the claude through `hub_user_asks` and
// owned by the backend (`src-tauri/src/asks.rs`). The user's checks, notes,
// choices, new items and dismissals are DRAFTS on the backend too — so the
// desktop, a remote desktop and the phone all show the same pending state —
// until UPDATE sends each claude a message listing exactly what changed.
//
// This store is only a mirror: every edit goes to the backend and the returned
// list replaces what is shown, and `asks-changed` (any writer) re-reads it.

import { get, writable } from "svelte/store";
import { asksEdit, asksGet, asksUpdate, type AskEdit, type AskList } from "./ipc";
import { flashToast } from "./stores";

/** Lists per instance, keyed `"<projectHandle>:<instanceId>"`, like `pins`. */
export const asks = writable<Map<string, AskList>>(new Map());

export const askKey = (handle: number, id: number) => `${handle}:${id}`;

/** The "All" view: every claude of the project, grouped. Per viewer. */
const ALL_KEY = "mulpex.asksAll";
function loadAll(): boolean {
  try {
    return localStorage.getItem(ALL_KEY) === "1";
  } catch {
    return false;
  }
}
export const asksAll = writable<boolean>(loadAll());
asksAll.subscribe((v) => {
  try {
    localStorage.setItem(ALL_KEY, v ? "1" : "0");
  } catch {
    /* not remembered */
  }
});

function put(handle: number, id: number, list: AskList | null) {
  asks.update((m) => {
    const next = new Map(m);
    if (list && (list.items.length || pendingCount(list) || list.sending)) next.set(askKey(handle, id), list);
    else next.delete(askKey(handle, id));
    return next;
  });
}

export const openCount = (l: AskList | undefined) => l?.items.filter((i) => i.status === "open").length ?? 0;
export const pendingCount = (l: AskList | undefined) =>
  l ? Object.keys(l.drafts.edits).length + l.drafts.added.length : 0;

/** Load (or re-load) every list of a project, replacing what is shown. */
export async function loadAsks(handle: number) {
  let all: Record<string, AskList>;
  try {
    all = await asksGet(handle);
  } catch {
    return;
  }
  asks.update((m) => {
    const next = new Map(m);
    for (const k of m.keys()) if (k.startsWith(`${handle}:`)) next.delete(k);
    for (const [id, list] of Object.entries(all)) {
      if (list.items.length || pendingCount(list) || list.sending) next.set(askKey(handle, Number(id)), list);
    }
    return next;
  });
}

/** `asks-changed {handle,id}`: re-read. The whole project is one small call. */
export const reloadAsks = (handle: number, _id: number) => loadAsks(handle);

/** Forget lists in memory only (instance gone, project closed) — the disk copy
 *  is the backend's to prune, as with pins. */
export function dropAsks(handle: number, id?: number) {
  asks.update((m) => {
    const next = new Map(m);
    for (const k of m.keys()) {
      if (id != null ? k === askKey(handle, id) : k.startsWith(`${handle}:`)) next.delete(k);
    }
    return next;
  });
}

/** One user edit. The backend's answer is the truth; an error is said, not swallowed. */
export async function editAsk(handle: number, id: number, edit: AskEdit) {
  try {
    put(handle, id, await asksEdit(handle, id, edit));
  } catch (err) {
    flashToast(String(err));
  }
}

/** UPDATE: send each claude its pending edits. */
export async function updateAsks(handle: number, ids: number[]) {
  if (!ids.length) return;
  try {
    const res = await asksUpdate(handle, ids);
    const refused = res.filter((r) => r.state === "refused" && r.why).map((r) => r.why);
    const sent = res.length - refused.length;
    if (refused.length) flashToast(refused.join(" · "));
    else if (sent) flashToast(sent === 1 ? "Sent" : `Sent to ${sent} claudes`);
  } catch (err) {
    flashToast(String(err));
  }
  loadAsks(handle);
}

/** The ids in `handle` with something waiting for UPDATE. */
export function pendingIds(handle: number): number[] {
  const out: number[] = [];
  for (const [k, l] of get(asks)) {
    const [h, i] = k.split(":").map(Number);
    if (h === handle && pendingCount(l) && !l.sending) out.push(i);
  }
  return out;
}
