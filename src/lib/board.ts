// The Board: one per project — columns the user defines, cards assigned to
// claudes, comments and images (`src-tauri/src/board.rs`). Replaced the
// per-claude "To Do" lists.
//
// This store is only a mirror: every edit goes to the backend and the returned
// board replaces what is shown, and `board-changed` (any writer: a claude's
// `hub_board`, the phone, a remote desktop) re-reads it. There are no drafts:
// an edit is live at once, and is queued for the card's claude until the user
// presses UPDATE.

import { get, writable } from "svelte/store";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import {
  boardEdit,
  boardGet,
  boardImageGet,
  boardImageImport,
  boardImagePut,
  boardSetOn,
  boardUpdate,
  type Board,
  type BoardCard,
  type BoardEdit,
} from "./ipc";
import { flashToast, projects } from "./stores";
import { addrIn } from "./agents";
import { hostUpload, slotOf } from "./hosts";

/** Boards per project handle. */
export const boards = writable<Map<number, Board>>(new Map());

function put(handle: number, b: Board | null) {
  boards.update((m) => {
    const next = new Map(m);
    if (b) next.set(handle, b);
    else next.delete(handle);
    return next;
  });
}

/** The "All" view (every card) vs the focused claude's cards. Per viewer. */
const ALL_KEY = "mulpex.boardAll";
function loadAll(): boolean {
  try {
    return localStorage.getItem(ALL_KEY) !== "0";
  } catch {
    return true;
  }
}
export const boardAll = writable<boolean>(loadAll());
boardAll.subscribe((v) => {
  try {
    localStorage.setItem(ALL_KEY, v ? "1" : "0");
  } catch {
    /* not remembered */
  }
});

export async function loadBoard(handle: number) {
  try {
    put(handle, await boardGet(handle));
  } catch {
    /* a closed project, or a host that went away */
  }
}

export const dropBoard = (handle: number) => put(handle, null);

/** One user edit. The backend's answer is the truth; an error is said, not swallowed. */
export async function editBoard(handle: number, edit: BoardEdit): Promise<boolean> {
  try {
    put(handle, await boardEdit(handle, edit));
    return true;
  } catch (err) {
    flashToast(String(err));
    return false;
  }
}

export async function setBoardOn(handle: number, on: boolean): Promise<boolean> {
  try {
    const changed = await boardSetOn(handle, on);
    await loadBoard(handle);
    if (changed) flashToast(on ? "Board on — every claude in this project is told" : "Board off");
    return changed;
  } catch (err) {
    flashToast(String(err));
    return false;
  }
}

/** UPDATE: tell every claude with queued changes. */
export async function sendBoard(handle: number) {
  try {
    const r = await boardUpdate(handle);
    if (r.refused.length) flashToast(r.refused.join(" · "));
    else if (r.sent.length) flashToast(r.sent.length === 1 ? `Sent to ${addrIn(get(projects).get(handle)?.sessions ?? [], r.sent[0])}` : `Sent to ${r.sent.length} claudes`);
  } catch (err) {
    flashToast(String(err));
  }
  await loadBoard(handle);
}

/** Card numbers with changes waiting for UPDATE. */
export const pendingCards = (b: Board | undefined) => new Set(Object.values(b?.pending ?? {}).flat());

/** Active cards of the board's last column don't count as open work. */
export const lastCol = (b: Board | undefined) => b?.columns[b.columns.length - 1]?.id;

export const unreadCount = (b: Board | undefined, only?: number | null) =>
  b?.cards.filter((c) => !c.archived && c.unread && (only == null || c.assignee === only)).length ?? 0;

/** Column proposals waiting for the user's Approve / Reject. */
export const proposalCount = (b: Board | undefined) => (b?.on ? (b.proposals?.length ?? 0) : 0);

export const imageCount = (c: BoardCard) => c.images.length + c.comments.reduce((n, m) => n + m.images.length, 0);

// ── images ─────────────────────────────────────────────────────────────────

/** Bytes per chunk sent: base64 and sealed, it stays far under a relay frame. */
const PUT_CHUNK = 2 * 1024 * 1024;
export const MAX_IMAGE_BYTES = 10 * 1024 * 1024;
export const MAX_IMAGES = 10;
export const IMAGE_TYPES = ["image/png", "image/jpeg", "image/gif", "image/webp"];

function toBase64(bytes: Uint8Array): string {
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

function fromBase64(b64: string): Uint8Array {
  const s = atob(b64);
  const out = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
  return out;
}

/** Store a pasted image (a clipboard File) on the board's host; its id. */
export async function uploadImage(handle: number, file: Blob): Promise<string> {
  if (file.size > MAX_IMAGE_BYTES) throw new Error("An image can be at most 10 MB");
  const bytes = new Uint8Array(await file.arrayBuffer());
  let upload: string | null = null;
  for (let off = 0; ; off += PUT_CHUNK) {
    const end = Math.min(off + PUT_CHUNK, bytes.length);
    const done = end >= bytes.length;
    const r = await boardImagePut(handle, toBase64(bytes.subarray(off, end)), upload, done);
    if (done) {
      if (!r.id) throw new Error("the image was not saved");
      return r.id;
    }
    upload = r.upload ?? null;
  }
}

/** Files on THIS machine (picked or dropped) → board image ids. A remote
 *  host's board gets them uploaded there first, then imported by its path. */
export async function importImages(handle: number, paths: string[]): Promise<string[]> {
  if (!paths.length) return [];
  const slot = slotOf(handle);
  if (slot > 0) paths = await hostUpload(slot, paths);
  return boardImageImport(handle, paths);
}

/** The 📎 button: pick images on this machine. */
export async function pickImages(handle: number): Promise<string[]> {
  const picked = await openDialog({
    multiple: true,
    directory: false,
    filters: [{ name: "Images", extensions: ["png", "jpg", "jpeg", "gif", "webp"] }],
  });
  const paths = picked == null ? [] : Array.isArray(picked) ? picked : [picked];
  if (paths.length > MAX_IMAGES) throw new Error(`At most ${MAX_IMAGES} images at once`);
  return importImages(handle, paths);
}

const MIME: Record<string, string> = { png: "image/png", jpg: "image/jpeg", gif: "image/gif", webp: "image/webp" };

/** Blob URLs by `"<handle>:<id>"` — an id names immutable bytes, so one fetch
 *  serves every thumbnail and the lightbox for the app's life. */
const urls = new Map<string, Promise<string>>();

export function imageUrl(handle: number, id: string): Promise<string> {
  const key = `${handle}:${id}`;
  let p = urls.get(key);
  if (!p) {
    p = (async () => {
      const parts: Uint8Array[] = [];
      let got = 0;
      for (;;) {
        const r = await boardImageGet(handle, id, got);
        const b = fromBase64(r.data);
        parts.push(b);
        got += b.length;
        if (got >= r.total || b.length === 0) break;
      }
      const blob = new Blob(parts as BlobPart[], { type: MIME[id.split(".").pop() ?? ""] ?? "application/octet-stream" });
      return URL.createObjectURL(blob);
    })();
    p.catch(() => urls.delete(key));
    urls.set(key, p);
  }
  return p;
}

// ── drops ──────────────────────────────────────────────────────────────────

/** Where a file dropped on the window goes instead of the terminal: the open
 *  card's composer registers itself here while it is mounted. */
type DropTarget = { el: HTMLElement; accept: (paths: string[]) => void };
let dropTarget: DropTarget | null = null;

export function setDropTarget(t: DropTarget | null) {
  dropTarget = t;
}

/** A drop at window position (x, y) in CSS pixels — taken by the open card if
 *  it lands on the side panel. True when taken. */
export function takeDrop(paths: string[], x: number, y: number): boolean {
  if (!dropTarget || !dropTarget.el.isConnected) return false;
  const panel = dropTarget.el.closest(".panel") ?? dropTarget.el;
  const r = panel.getBoundingClientRect();
  if (x < r.left || x > r.right || y < r.top || y > r.bottom) return false;
  const imgs = paths.filter((p) => /\.(png|jpe?g|gif|webp)$/i.test(p));
  if (!imgs.length) {
    flashToast("Only PNG, JPG, GIF and WebP images can be attached");
    return true;
  }
  dropTarget.accept(imgs);
  return true;
}

export const currentBoard = (handle: number) => get(boards).get(handle);
