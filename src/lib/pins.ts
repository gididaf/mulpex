// Pins: a selection lifted off a claude's pane (⌘⇧P) and floated over the top of
// that same pane, so an important block — a QA checklist, a command — stays in
// view while the conversation scrolls on.
//
// A pin keeps claude's own colors. It is captured cell by cell from the xterm
// buffer (not from `getSelection()`, which is plain text), and every color is
// resolved to CSS at capture time, so a pin is self-contained data: nothing in
// it refers back to a palette or a terminal that may be gone.

import { get, writable } from "svelte/store";
import { getPins, setPin } from "./ipc";
import type { IBuffer, IBufferCell, ITheme, Terminal } from "@xterm/xterm";

/** One styled run of text. Flags are omitted when off. */
export interface PinRun {
  t: string;
  fg?: string;
  bg?: string;
  b?: 1; // bold
  d?: 1; // dim
  i?: 1; // italic
  u?: 1; // underline
  s?: 1; // strikethrough
}

export interface Pin {
  uid: string;
  lines: PinRun[][];
  /** The plain text, for the copy button. */
  text: string;
  collapsed: boolean;
}

// ── capture ────────────────────────────────────────────────────────────────

const ANSI_KEYS: (keyof ITheme)[] = [
  "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
  "brightBlack", "brightRed", "brightGreen", "brightYellow",
  "brightBlue", "brightMagenta", "brightCyan", "brightWhite",
];

const hex2 = (n: number) => n.toString(16).padStart(2, "0");
const rgb = (r: number, g: number, b: number) => `#${hex2(r)}${hex2(g)}${hex2(b)}`;

/** xterm's 256-colour palette: 16 theme colours, a 6×6×6 cube, 24 greys. */
function paletteColor(theme: ITheme, n: number): string {
  if (n < 16) return (theme[ANSI_KEYS[n]] as string) ?? "#ffffff";
  if (n < 232) {
    const levels = [0, 95, 135, 175, 215, 255];
    const i = n - 16;
    return rgb(levels[Math.floor(i / 36)], levels[Math.floor(i / 6) % 6], levels[i % 6]);
  }
  const g = 8 + (n - 232) * 10;
  return rgb(g, g, g);
}

function fgOf(cell: IBufferCell, theme: ITheme): string | undefined {
  if (cell.isFgRGB()) {
    const c = cell.getFgColor();
    return rgb((c >> 16) & 255, (c >> 8) & 255, c & 255);
  }
  if (cell.isFgPalette()) {
    let n = cell.getFgColor();
    // xterm's default `drawBoldTextInBrightColors`: bold 0–7 draws as 8–15.
    if (cell.isBold() && n < 8) n += 8;
    return paletteColor(theme, n);
  }
  return undefined;
}

function bgOf(cell: IBufferCell, theme: ITheme): string | undefined {
  if (cell.isBgRGB()) {
    const c = cell.getBgColor();
    return rgb((c >> 16) & 255, (c >> 8) & 255, c & 255);
  }
  if (cell.isBgPalette()) return paletteColor(theme, cell.getBgColor());
  return undefined;
}

function styleOf(cell: IBufferCell, theme: ITheme): Omit<PinRun, "t"> {
  let fg = fgOf(cell, theme);
  let bg = bgOf(cell, theme);
  if (cell.isInverse()) {
    [fg, bg] = [bg ?? (theme.background as string), fg ?? (theme.foreground as string)];
  }
  const s: Omit<PinRun, "t"> = {};
  if (fg) s.fg = fg;
  if (bg) s.bg = bg;
  if (cell.isBold()) s.b = 1;
  if (cell.isDim()) s.d = 1;
  if (cell.isItalic()) s.i = 1;
  if (cell.isUnderline()) s.u = 1;
  if (cell.isStrikethrough()) s.s = 1;
  return s;
}

const sameStyle = (a: Omit<PinRun, "t">, b: Omit<PinRun, "t">) =>
  a.fg === b.fg && a.bg === b.bg && a.b === b.b && a.d === b.d &&
  a.i === b.i && a.u === b.u && a.s === b.s;

/** Drop trailing blanks on unstyled background, the way `getSelection()` does. */
function trimEnd(line: PinRun[]): PinRun[] {
  while (line.length) {
    const last = line[line.length - 1];
    if (last.bg) break;
    const t = last.t.replace(/\s+$/, "");
    if (t) {
      last.t = t;
      break;
    }
    line.pop();
  }
  return line;
}

/** The current selection of `term` as styled lines, or null when nothing is
 *  selected. Soft-wrapped rows join into one line, like `getSelection()`. */
export function captureSelection(term: Terminal, theme: ITheme): PinRun[][] | null {
  const pos = term.getSelectionPosition();
  if (!pos || !term.hasSelection()) return null;
  // xterm 5.5 reports these 0-based with `end.x` exclusive, whatever the
  // typings say (`getSelectionPosition` returns the selection model's raw
  // coordinates).
  const { start, end } = pos;
  const buf: IBuffer = term.buffer.active;
  const cell = buf.getNullCell();
  const lines: PinRun[][] = [];
  let cur: PinRun[] = [];
  for (let y = start.y; y <= end.y; y++) {
    const line = buf.getLine(y);
    if (!line) break;
    const from = y === start.y ? start.x : 0;
    const to = y === end.y ? end.x : term.cols;
    for (let x = from; x < to; x++) {
      if (!line.getCell(x, cell)) break;
      if (cell.getWidth() === 0) continue; // tail of a wide character
      const ch = cell.getChars() || " ";
      const st = styleOf(cell, theme);
      const last = cur[cur.length - 1];
      if (last && sameStyle(last, st)) last.t += ch;
      else cur.push({ t: ch, ...st });
    }
    const next = buf.getLine(y + 1);
    if (y < end.y && next?.isWrapped) continue;
    lines.push(trimEnd(cur));
    cur = [];
  }
  // Leading/trailing empty lines are selection slop, not content.
  while (lines.length && lines[0].length === 0) lines.shift();
  while (lines.length && lines[lines.length - 1].length === 0) lines.pop();
  return lines.length ? lines : null;
}

export function plainText(lines: PinRun[][]): string {
  return lines.map((l) => l.map((r) => r.t).join("")).join("\n");
}

// ── state ──────────────────────────────────────────────────────────────────

/** Pins per instance, keyed `"<projectHandle>:<instanceId>"`. */
export const pins = writable<Map<string, Pin[]>>(new Map());

export const pinKey = (handle: number, id: number) => `${handle}:${id}`;

/** Change one instance's pins and save the result (`pins.rs` keeps one per
 *  instance on disk, which is all the UI allows). */
function edit(handle: number, id: number, f: (list: Pin[]) => Pin[]) {
  const key = pinKey(handle, id);
  pins.update((m) => {
    const next = new Map(m);
    const list = f(m.get(key) ?? []);
    if (list.length) next.set(key, list);
    else next.delete(key);
    return next;
  });
  setPin(handle, id, get(pins).get(key)?.[0] ?? null).catch(() => {});
}

/** One pin per instance: a new pin replaces the previous one. */
export function addPin(handle: number, id: number, lines: PinRun[][]) {
  const pin: Pin = { uid: crypto.randomUUID(), lines, text: plainText(lines), collapsed: false };
  edit(handle, id, () => [pin]);
}

export function removePin(handle: number, id: number, uid: string) {
  edit(handle, id, (l) => l.filter((p) => p.uid !== uid));
}

export function togglePin(handle: number, id: number, uid: string) {
  edit(handle, id, (l) =>
    l.map((p) => (p.uid === uid ? { ...p, collapsed: !p.collapsed } : p)),
  );
}

/** Unpin (handle, id) — saved, unlike `dropPins`. */
export function clearPin(handle: number, id: number) {
  edit(handle, id, () => []);
}

/** Load a project's saved pins (at launch / project open). */
export async function loadPins(handle: number) {
  let saved: Record<string, unknown>;
  try {
    saved = await getPins(handle);
  } catch {
    return;
  }
  pins.update((m) => {
    const next = new Map(m);
    for (const [id, pin] of Object.entries(saved)) {
      const p = pin as Pin;
      if (Array.isArray(p?.lines)) next.set(pinKey(handle, Number(id)), [p]);
    }
    return next;
  });
}

/** Forget pins in memory only — an instance whose terminal is gone, or a closed
 *  project. The disk copy is the backend's to prune (`persist_sessions`), because
 *  an app quit exits sessions too and must not lose them. */
export function dropPins(handle: number, id?: number) {
  pins.update((m) => {
    const next = new Map(m);
    for (const k of m.keys()) {
      if (id != null ? k === pinKey(handle, id) : k.startsWith(`${handle}:`)) next.delete(k);
    }
    return next;
  });
}
