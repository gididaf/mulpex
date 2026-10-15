// "To Do": each claude's list of what it needs from the user (an action, a
// decision, a question), written by the claude through `hub_todo` and
// owned by the backend (`src-tauri/src/todos.rs`). The user's checks, replies,
// choices, new items and dismissals are DRAFTS on the backend too — so the
// desktop, a remote desktop and the phone all show the same pending state —
// until UPDATE sends each claude a message listing exactly what changed.
//
// This store is only a mirror: every edit goes to the backend and the returned
// list replaces what is shown, and `todos-changed` (any writer) re-reads it.

import { get, writable } from "svelte/store";
import { todosEdit, todosGet, todosUpdate, type TodoEdit, type TodoItem, type TodoList } from "./ipc";
import { flashToast } from "./stores";

/** Lists per instance, keyed `"<projectHandle>:<instanceId>"`, like `pins`. */
export const todos = writable<Map<string, TodoList>>(new Map());

export const todoKey = (handle: number, id: number) => `${handle}:${id}`;

/** The "All" view: every claude of the project, grouped. Per viewer. */
const ALL_KEY = "mulpex.todosAll";
function loadAll(): boolean {
  try {
    return localStorage.getItem(ALL_KEY) === "1";
  } catch {
    return false;
  }
}
export const todosAll = writable<boolean>(loadAll());
todosAll.subscribe((v) => {
  try {
    localStorage.setItem(ALL_KEY, v ? "1" : "0");
  } catch {
    /* not remembered */
  }
});

function put(handle: number, id: number, list: TodoList | null) {
  todos.update((m) => {
    const next = new Map(m);
    if (list && (list.items.length || pendingCount(list) || list.sending)) next.set(todoKey(handle, id), list);
    else next.delete(todoKey(handle, id));
    return next;
  });
}

/** Urgent items first, otherwise in list order (the sort is stable). */
export const byUrgency = (items: TodoItem[]) => [...items].sort((a, b) => Number(b.urgent) - Number(a.urgent));

/** A claude comment past what the user has seen — the 💬 dot. */
export const unread = (i: TodoItem) => i.comments.slice(i.seen).some((c) => c.by === "claude");
export const openCount = (l: TodoList | undefined) => l?.items.filter((i) => i.status === "open").length ?? 0;
export const pendingCount = (l: TodoList | undefined) =>
  l ? Object.keys(l.drafts.edits).length + l.drafts.added.length : 0;

/** Load (or re-load) every list of a project, replacing what is shown. */
export async function loadTodos(handle: number) {
  let all: Record<string, TodoList>;
  try {
    all = await todosGet(handle);
  } catch {
    return;
  }
  todos.update((m) => {
    const next = new Map(m);
    for (const k of m.keys()) if (k.startsWith(`${handle}:`)) next.delete(k);
    for (const [id, list] of Object.entries(all)) {
      if (list.items.length || pendingCount(list) || list.sending) next.set(todoKey(handle, Number(id)), list);
    }
    return next;
  });
}

/** `todos-changed {handle,id}`: re-read. The whole project is one small call. */
export const reloadTodos = (handle: number, _id: number) => loadTodos(handle);

/** Forget lists in memory only (instance gone, project closed) — the disk copy
 *  is the backend's to prune, as with pins. */
export function dropTodos(handle: number, id?: number) {
  todos.update((m) => {
    const next = new Map(m);
    for (const k of m.keys()) {
      if (id != null ? k === todoKey(handle, id) : k.startsWith(`${handle}:`)) next.delete(k);
    }
    return next;
  });
}

/** One user edit. The backend's answer is the truth; an error is said, not swallowed. */
export async function editTodo(handle: number, id: number, edit: TodoEdit) {
  try {
    put(handle, id, await todosEdit(handle, id, edit));
  } catch (err) {
    flashToast(String(err));
  }
}

/** UPDATE: send each claude its pending edits. */
export async function updateTodos(handle: number, ids: number[]) {
  if (!ids.length) return;
  try {
    const res = await todosUpdate(handle, ids);
    const refused = res.filter((r) => r.state === "refused" && r.why).map((r) => r.why);
    const sent = res.length - refused.length;
    if (refused.length) flashToast(refused.join(" · "));
    else if (sent) flashToast(sent === 1 ? "Sent" : `Sent to ${sent} claudes`);
  } catch (err) {
    flashToast(String(err));
  }
  loadTodos(handle);
}

/** The ids in `handle` with something waiting for UPDATE. */
export function pendingIds(handle: number): number[] {
  const out: number[] = [];
  for (const [k, l] of get(todos)) {
    const [h, i] = k.split(":").map(Number);
    if (h === handle && pendingCount(l) && !l.sending) out.push(i);
  }
  return out;
}
