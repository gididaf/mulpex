// One xterm.js Terminal per Claude session, kept alive (hidden, not destroyed)
// when not focused so background Claudes keep rendering. All terminals share one
// geometry — the center pane — exactly as the TUI kept every PTY at one size.
//
// This module is imperative and owns its DOM subtree; Svelte never renders into
// a terminal's container after `open()`.
//
// The DOM renderer is DELIBERATE — do not re-add the WebGL addon for speed. WebGL
// draws one glyph quad per cell, so column n always gets character n and RTL text
// (Hebrew/Arabic) renders mirrored; the DOM renderer emits each styled run as a
// span of real text, which the browser's own BiDi engine reorders for free.
// Measured: the same frame through xterm 5.5.0 renders "שלום זאת בדיקה" under the
// DOM renderer and "הקידב תאז םולש" under WebGL. (xterm itself has zero BiDi code —
// `grep -c bidi` on the bundle is 0 — so the browser is the only implementation
// available to us.) Caveat: reordering is per styled run, so a color change
// mid-phrase still breaks ordering at that seam, and the caret is column-based.

import { Terminal } from "@xterm/xterm";
import type { ITheme } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Channel, attachSession, sendBytes, resizeSession } from "./ipc";
import type { SessionKind } from "./ipc";

const THEME: ITheme = {
  background: "#0d0d0f",
  foreground: "#e6e6e6",
  cursor: "#e6e6e6",
  selectionBackground: "#264f78",
  black: "#1a1a1a",
  red: "#e06c75",
  green: "#98c379",
  yellow: "#e5c07b",
  blue: "#61afef",
  magenta: "#c678dd",
  cyan: "#56b6c2",
  white: "#dcdcdc",
  brightBlack: "#5c6370",
  brightRed: "#e06c75",
  brightGreen: "#98c379",
  brightYellow: "#e5c07b",
  brightBlue: "#61afef",
  brightMagenta: "#c678dd",
  brightCyan: "#56b6c2",
  brightWhite: "#ffffff",
};

const FONT_FAMILY =
  'ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Monaco, "Cascadia Code", monospace';

const encoder = new TextEncoder();

function b64ToBytes(s: string): Uint8Array {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

interface Entry {
  handle: number;
  id: number;
  term: Terminal;
  fit: FitAddon;
  container: HTMLElement;
}

/** Composite key: instance ids are per-project, so (handle, id) disambiguates. */
function keyOf(handle: number, id: number): string {
  return `${handle}:${id}`;
}

class TerminalManager {
  private entries = new Map<string, Entry>();
  private activeKey: string | null = null;
  private cols = 80;
  private rows = 24;

  has(handle: number, id: number): boolean {
    return this.entries.has(keyOf(handle, id));
  }

  /** Create the xterm for (handle, id) inside `container` and bind its PTY stream.
   *  `kind` gates only the Shift+Enter carve-out below — everything else is one
   *  code path for both kinds, deliberately (the DOM-renderer / no-WebGL rule
   *  that makes RTL work is not a Claude-specific concern). */
  create(
    handle: number,
    id: number,
    container: HTMLElement,
    kind: SessionKind = "claude",
  ): void {
    const key = keyOf(handle, id);
    if (this.entries.has(key)) return;

    const term = new Terminal({
      scrollback: 10000,
      macOptionIsMeta: true,
      allowProposedApi: true,
      fontFamily: FONT_FAMILY,
      fontSize: 13,
      lineHeight: 1.1,
      theme: THEME,
      cursorBlink: true,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.loadAddon(
      new WebLinksAddon((_e, uri) => {
        openUrl(uri).catch(() => {});
      }),
    );
    term.open(container);

    // xterm encodes all keys → PTY bytes; forward them raw.
    term.onData((data) => {
      sendBytes(handle, id, encoder.encode(data));
    });
    // The one carve-out: Shift+Enter (and Option+Enter) insert a newline instead
    // of submitting. We send ESC+CR — meta-Return, what `/terminal-setup` installs
    // for VS Code / Terminal.app / Alacritty (a bare \n also works, but this is the
    // sequence Claude documents). The two bytes cross the IPC in one write, which
    // matters: split across reads, the ESC is consumed alone and the CR submits.
    //
    // `preventDefault()` is load-bearing. Returning false makes xterm bail out of
    // `_keyDown` *before* it sets `_keyDownHandled`, and it never calls
    // preventDefault itself on that path — so the browser goes on to fire
    // `keypress`, and xterm's `_keyPress` (which only short-circuits on
    // `_keyDownHandled`) turns charCode 13 into a `\r` and submits the message.
    // That extra `\r` was the actual bug: our newline byte arrived, and Claude
    // then got a submit right behind it. Cancelling the event suppresses the
    // keypress entirely; the `keypress` arm below is a second line of defence.
    //
    // Claude sessions only. In a shell, Shift+Enter has no "insert a newline in
    // my multi-line composer" meaning to rescue — ESC+CR there is meta-Return,
    // which bash/zsh bind to something else entirely.
    const isNewlineKey = (e: KeyboardEvent): boolean =>
      e.key === "Enter" && (e.shiftKey || e.altKey);
    if (kind === "claude") {
      term.attachCustomKeyEventHandler((e) => {
        if (e.type === "keydown" && isNewlineKey(e)) {
          e.preventDefault();
          e.stopPropagation();
          sendBytes(handle, id, new Uint8Array([0x1b, 0x0d]));
          return false;
        }
        if (e.type === "keypress" && isNewlineKey(e)) return false;
        return true;
      });
    }

    // PTY output arrives base64-chunked over this session's channel.
    const channel = new Channel<string>();
    channel.onmessage = (chunk) => term.write(b64ToBytes(chunk));
    attachSession(handle, id, channel);

    const entry: Entry = { handle, id, term, fit, container };
    this.entries.set(key, entry);
    container.style.visibility = key === this.activeKey ? "visible" : "hidden";
  }

  /** An exited terminal stays visible and scrollable but stops accepting input.
   *  Without this, typing into a dead shell is a silent no-op: the write lands
   *  on a master with no slave, the EIO is swallowed backend-side, and nothing
   *  tells the user why their keystrokes vanish. */
  setExited(handle: number, id: number, exited: boolean): void {
    const e = this.entries.get(keyOf(handle, id));
    if (!e) return;
    if (e.term.options.disableStdin !== exited) {
      e.term.options.disableStdin = exited;
      e.term.options.cursorBlink = !exited;
    }
  }

  dispose(handle: number, id: number): void {
    const key = keyOf(handle, id);
    const e = this.entries.get(key);
    if (!e) return;
    e.term.dispose();
    this.entries.delete(key);
  }

  /** Dispose every terminal belonging to a project (on project close). */
  disposeProject(handle: number): void {
    for (const e of [...this.entries.values()]) {
      if (e.handle === handle) this.dispose(handle, e.id);
    }
  }

  /** Show (handle, id) — exactly one terminal visible across ALL projects — hide
   * the rest (keeping them alive), fit + refocus. A non-existent key (e.g. a
   * project with no sessions) hides everything. */
  focus(handle: number, id: number): void {
    this.activeKey = keyOf(handle, id);
    for (const [ekey, e] of this.entries) {
      e.container.style.visibility = ekey === this.activeKey ? "visible" : "hidden";
    }
    const e = this.activeKey ? this.entries.get(this.activeKey) : undefined;
    this.refit();
    if (e) e.term.focus();
  }

  /** Measure the visible terminal and bring every terminal + all backend PTYs to
   * that one shared geometry. Resizes any terminal that's out of sync — not just
   * on a pane-size change — so a newly opened instance (whose xterm starts at the
   * 80×24 default while its PTY spawned at another size) gets corrected too;
   * otherwise Claude draws at one width and xterm shows another (garbled wrap). */
  refit(): void {
    const active = this.activeKey
      ? this.entries.get(this.activeKey)
      : undefined;
    if (!active) return;
    const dims = active.fit.proposeDimensions();
    if (
      !dims ||
      !Number.isFinite(dims.cols) ||
      !Number.isFinite(dims.rows) ||
      dims.cols < 1 ||
      dims.rows < 1
    ) {
      return;
    }
    let resized = dims.cols !== this.cols || dims.rows !== this.rows;
    this.cols = dims.cols;
    this.rows = dims.rows;
    for (const e of this.entries.values()) {
      if (e.term.cols !== this.cols || e.term.rows !== this.rows) {
        e.term.resize(this.cols, this.rows);
        resized = true;
      }
    }
    // Match every PTY to the terminals whenever anything changed (a new terminal
    // synced, or the pane resized), so no session's PTY is left at spawn size —
    // including background projects. All PTYs share this one geometry; resize each
    // open project's sessions (one backend call per distinct project handle).
    if (resized) {
      const handles = new Set<number>();
      for (const e of this.entries.values()) handles.add(e.handle);
      for (const h of handles) resizeSession(h, this.cols, this.rows);
    }
  }

  /** Re-focus the active terminal (after a dialog/menu action steals focus). */
  refocus(): void {
    if (this.activeKey) this.entries.get(this.activeKey)?.term.focus();
  }
}

export const terminals = new TerminalManager();
