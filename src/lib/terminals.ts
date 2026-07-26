// One xterm.js Terminal per Claude session, kept alive (hidden, not destroyed)
// when not focused so background Claudes keep rendering. All terminals share one
// geometry — the center pane — exactly as the TUI kept every PTY at one size.
//
// This module is imperative and owns its DOM subtree; Svelte never renders into
// a terminal's container after `open()`.

import { Terminal } from "@xterm/xterm";
import type { ITheme } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { WebglAddon } from "@xterm/addon-webgl";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Channel, attachSession, sendBytes, resizeSession } from "./ipc";

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
  webgl?: WebglAddon;
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

  /** Create the xterm for (handle, id) inside `container` and bind its PTY stream. */
  create(handle: number, id: number, container: HTMLElement): void {
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
    const isNewlineKey = (e: KeyboardEvent): boolean =>
      e.key === "Enter" && (e.shiftKey || e.altKey);
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

    // PTY output arrives base64-chunked over this session's channel.
    const channel = new Channel<string>();
    channel.onmessage = (chunk) => term.write(b64ToBytes(chunk));
    attachSession(handle, id, channel);

    const entry: Entry = { handle, id, term, fit, container };
    this.entries.set(key, entry);
    container.style.visibility = key === this.activeKey ? "visible" : "hidden";
    if (key === this.activeKey) this.attachWebgl(entry);
  }

  dispose(handle: number, id: number): void {
    const key = keyOf(handle, id);
    const e = this.entries.get(key);
    if (!e) return;
    e.webgl?.dispose();
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
      const visible = ekey === this.activeKey;
      e.container.style.visibility = visible ? "visible" : "hidden";
      if (visible) {
        this.attachWebgl(e);
      } else if (e.webgl) {
        // Free the GPU context (browsers cap live WebGL contexts).
        e.webgl.dispose();
        e.webgl = undefined;
      }
    }
    const e = this.activeKey ? this.entries.get(this.activeKey) : undefined;
    this.refit();
    if (e) e.term.focus();
  }

  private attachWebgl(e: Entry): void {
    if (e.webgl) return;
    try {
      const addon = new WebglAddon();
      addon.onContextLoss(() => {
        addon.dispose();
        e.webgl = undefined;
      });
      e.term.loadAddon(addon);
      e.webgl = addon;
    } catch {
      // WebGL unavailable → xterm's default DOM renderer is fine.
    }
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
