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
  term: Terminal;
  fit: FitAddon;
  container: HTMLElement;
  webgl?: WebglAddon;
}

class TerminalManager {
  private entries = new Map<number, Entry>();
  private activeId: number | null = null;
  private cols = 80;
  private rows = 24;

  has(id: number): boolean {
    return this.entries.has(id);
  }

  /** Create the xterm for `id` inside `container` and bind its PTY stream. */
  create(id: number, container: HTMLElement): void {
    if (this.entries.has(id)) return;

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
      sendBytes(id, encoder.encode(data));
    });
    // The one carve-out: Shift+Enter sends a newline (not Enter's submit \r).
    term.attachCustomKeyEventHandler((e) => {
      if (e.type === "keydown" && e.key === "Enter" && e.shiftKey) {
        sendBytes(id, new Uint8Array([0x0a]));
        return false;
      }
      return true;
    });

    // PTY output arrives base64-chunked over this session's channel.
    const channel = new Channel<string>();
    channel.onmessage = (chunk) => term.write(b64ToBytes(chunk));
    attachSession(id, channel);

    const entry: Entry = { term, fit, container };
    this.entries.set(id, entry);
    container.style.visibility = id === this.activeId ? "visible" : "hidden";
    if (id === this.activeId) this.attachWebgl(entry);
  }

  dispose(id: number): void {
    const e = this.entries.get(id);
    if (!e) return;
    e.webgl?.dispose();
    e.term.dispose();
    this.entries.delete(id);
  }

  /** Show `id`, hide the rest (keeping them alive), fit + refocus. */
  focus(id: number): void {
    this.activeId = id;
    for (const [eid, e] of this.entries) {
      const visible = eid === id;
      e.container.style.visibility = visible ? "visible" : "hidden";
      if (visible) {
        this.attachWebgl(e);
      } else if (e.webgl) {
        // Free the GPU context (browsers cap live WebGL contexts).
        e.webgl.dispose();
        e.webgl = undefined;
      }
    }
    const e = this.entries.get(id);
    if (e) {
      this.refit();
      e.term.focus();
    }
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
    const active =
      this.activeId != null ? this.entries.get(this.activeId) : undefined;
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
    // synced, or the pane resized), so no session's PTY is left at spawn size.
    if (resized) resizeSession(this.cols, this.rows);
  }

  /** Re-focus the active terminal (after a dialog/menu action steals focus). */
  refocus(): void {
    if (this.activeId != null) this.entries.get(this.activeId)?.term.focus();
  }
}

export const terminals = new TerminalManager();
