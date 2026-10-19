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
import { SerializeAddon } from "@xterm/addon-serialize";
import { openUrl } from "@tauri-apps/plugin-opener";
import { invoke } from "@tauri-apps/api/core";
import { Channel, attachSession, sendBytes, resizeTerminals } from "./ipc";
import { slotOf, hostClipboardImage } from "./hosts";
import { pastePaths } from "./paths";
import type { SessionKind } from "./ipc";
import { readPromptBox } from "./promptbox";
import type { PromptBox } from "./promptbox";
import { captureSelection, plainText } from "./pins";
import { IS_LINUX, isSuper } from "./platform";
import type { PinRun } from "./pins";

export const THEME: ITheme = {
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

export const FONT_FAMILY =
  'ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Monaco, "Cascadia Code", "DejaVu Sans Mono", "Noto Sans Mono", monospace';

export const FONT_SIZE = 13;
export const LINE_HEIGHT = 1.1;

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
  /** The remote host this pane mirrors (`hosts.ts`), or 0 for a local one. A
   *  mirror's size follows its host's stream, never this window's refit. */
  slot: number;
  /** Local panes: bytes handed to `term.write` since attach — the stream
   *  offset a serialized screen corresponds to (`serialize`). */
  received: number;
  ser: SerializeAddon;
}

/** One frame of a remote pane's stream (`client.rs::on_message`). */
type MirrorFrame =
  | { t: "snap"; d: string; cols: number; rows: number; partial: boolean }
  | { t: "o"; d: string }
  | { t: "r"; cols: number; rows: number }
  | { t: "gone" };

/** How much scrollback a remote host's snapshot of a pane carries. */
const SNAPSHOT_SCROLLBACK = 2000;

/** Composite key: instance ids are per-project, so (handle, id) disambiguates. */
function keyOf(handle: number, id: number): string {
  return `${handle}:${id}`;
}

class TerminalManager {
  private entries = new Map<string, Entry>();
  private activeKey: string | null = null;
  private cols = 80;
  private rows = 24;
  /** A remote desktop drives this Mulpex's geometry (`geometry-changed`):
   *  every local xterm stays at this size whatever the pane measures. */
  private lock: { cols: number; rows: number } | null = null;
  /** The pane has been measured at least once — until then `cols`/`rows` are
   *  the backend's spawn default, not this window's size. */
  private measured = false;
  /** Each remote host's PTY size, for building a mirror before its first frame. */
  private hostGeo = new Map<number, { cols: number; rows: number }>();
  /** Told this window's pane size whenever it changes (`remoteHosts.ts` passes
   *  it on to the hosts whose geometry this Mulpex drives). */
  onGeometry: ((cols: number, rows: number) => void) | null = null;

  /** Adopt the geometry the backend spawned every PTY at, before any terminal is
   *  built. Called once from `bootstrap()`; after that `refit` owns the value and
   *  moves both sides together. Without it the first terminals of a launch are
   *  built at xterm's 80x24 default and fed a stream rendered for another size —
   *  see `create`. */
  setGeometry(cols: number, rows: number): void {
    if (cols >= 1 && rows >= 1) {
      this.cols = cols;
      this.rows = rows;
    }
  }

  has(handle: number, id: number): boolean {
    return this.entries.has(keyOf(handle, id));
  }

  /** This window's pane size, as last measured. */
  geometry(): { cols: number; rows: number } | null {
    return this.measured ? { cols: this.cols, rows: this.rows } : null;
  }

  setHostGeo(slot: number, cols: number, rows: number): void {
    if (cols >= 1 && rows >= 1) this.hostGeo.set(slot, { cols, rows });
  }

  /** Hold every local xterm at a remote desktop's size (`null`: back to the
   *  pane's). Releasing refits at once, which resizes every PTY back. */
  setLock(geo: { cols: number; rows: number } | null): void {
    this.lock = geo;
    if (geo) {
      this.cols = geo.cols;
      this.rows = geo.rows;
      for (const e of this.entries.values()) {
        if (!e.slot && (e.term.cols !== geo.cols || e.term.rows !== geo.rows)) e.term.resize(geo.cols, geo.rows);
      }
    } else {
      this.refit(true);
    }
  }

  /** (handle, id)'s screen and scrollback, serialized — for a remote desktop
   *  starting to mirror it (`remote/desktop.rs`). `offset` is the stream
   *  position it shows: taken before an empty write that xterm processes in
   *  order, so by its callback every byte up to `offset` is on screen. */
  serialize(handle: number, id: number): Promise<{ data: string; offset: number; cols: number; rows: number } | null> {
    const e = this.entries.get(keyOf(handle, id));
    if (!e || e.slot) return Promise.resolve(null);
    const offset = e.received;
    return new Promise((resolve) => {
      e.term.write("", () => {
        try {
          const data = e.ser.serialize({ scrollback: SNAPSHOT_SCROLLBACK });
          resolve({ data, offset, cols: e.term.cols, rows: e.term.rows });
        } catch {
          resolve(null);
        }
      });
    });
  }

  /** Every pane mirroring host `slot`. */
  mirrorsOf(slot: number): { handle: number; id: number }[] {
    return [...this.entries.values()].filter((e) => e.slot === slot).map((e) => ({ handle: e.handle, id: e.id }));
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
    const slot = slotOf(handle);
    // A mirror starts at its host's size; its first frame (`snap`) sets it
    // exactly, before a single byte is written.
    const geo = slot
      ? (this.hostGeo.get(slot) ?? { cols: this.cols, rows: this.rows })
      : { cols: this.cols, rows: this.rows };

    // `cols`/`rows`, NOT xterm's 80x24 default. This terminal is about to be
    // attached, and `attach_session` flushes everything the PTY has produced so
    // far — for a session restored at launch that is claude's entire startup
    // paint. Rendering those bytes at a size the PTY never had is not a transient
    // glitch that the next frame repairs: Claude Code draws on the ALT SCREEN and
    // repaints it DIFFERENTIALLY (it skips unchanged rows with CUD rather than
    // redrawing them), so any row the emulator gets wrong stays wrong until
    // claude happens to write to that row again — which for a blank row is never.
    // Too FEW rows/cols is the destructive direction: the content scrolls off the
    // top or wraps, and what scrolled away can never be repainted.
    // Measured (scratchpad harness, real claude on a PTY, replayed through this
    // exact xterm build): a stream rendered for 120x32 fed to an 80x24 emulator
    // and then resized leaves permanent debris — leftover box rule through
    // "⏺─I'll─look─at─…", "| Ctx Used: 3.0%" with its "Model:" gone, the input
    // box's own border missing. The same stream into an emulator that was 120x32
    // all along is byte-identical to tmux's render. A resize while the two agree
    // is fully self-healing (claude repaints everything on SIGWINCH), which is
    // why keeping the sizes equal AT ATTACH TIME is the whole fix.
    const term = new Terminal({
      cols: geo.cols,
      rows: geo.rows,
      scrollback: 10000,
      macOptionIsMeta: true,
      allowProposedApi: true,
      fontFamily: FONT_FAMILY,
      fontSize: FONT_SIZE,
      lineHeight: LINE_HEIGHT,
      theme: THEME,
      cursorBlink: true,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    const ser = new SerializeAddon();
    term.loadAddon(ser);
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
    //
    // On Linux every kind gets a handler as well: a Super chord is a Mulpex
    // shortcut there (lib/linuxKeys.ts), never input, so xterm must not encode it.
    // Ctrl+V in a remote claude: Claude Code would read the clipboard of the
    // machine IT runs on. The image on this Mac's clipboard goes there first
    // and its path is pasted; with no image, the Ctrl+V goes through as is.
    const isImagePaste = (e: KeyboardEvent): boolean =>
      slot > 0 && kind === "claude" && e.ctrlKey && !e.metaKey && !e.altKey && e.code === "KeyV";
    if (kind === "claude" || IS_LINUX) {
      term.attachCustomKeyEventHandler((e) => {
        if (IS_LINUX && isSuper(e)) return false;
        if (kind !== "claude") return true;
        if (isImagePaste(e)) {
          if (e.type === "keydown") {
            e.preventDefault();
            hostClipboardImage(slot)
              .then((path) => sendBytes(handle, id, path ? pastePaths([path]) : new Uint8Array([0x16])))
              .catch(() => sendBytes(handle, id, new Uint8Array([0x16])));
          }
          return false;
        }
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

    const entry: Entry = { handle, id, term, fit, container, slot, received: 0, ser };
    if (slot) {
      // A remote host's pane: its screen, then every byte and resize after it,
      // in the order they happened there (`remote/desktop.rs`).
      const channel = new Channel<MirrorFrame>();
      // A resize reflows the scrollback, and xterm can leave the view
      // scrolled up afterwards; a mirror that was following the bottom keeps
      // following it. Queued behind the writes, so it runs in stream order.
      const resize = (cols: number, rows: number) =>
        term.write("", () => {
          if (term.cols === cols && term.rows === rows) return;
          const buf = term.buffer.active;
          const atBottom = buf.viewportY >= buf.baseY;
          term.resize(cols, rows);
          if (atBottom) term.scrollToBottom();
        });
      channel.onmessage = (f) => {
        if (f.t === "snap") {
          term.reset();
          if (term.cols !== f.cols || term.rows !== f.rows) term.resize(f.cols, f.rows);
          term.write(b64ToBytes(f.d), () => term.scrollToBottom());
        } else if (f.t === "o") {
          term.write(b64ToBytes(f.d));
        } else if (f.t === "r") {
          resize(f.cols, f.rows);
        }
      };
      void invoke("host_attach", { host: slot, projectHandle: handle, id, channel });
    } else {
      attachSession(handle, id, this.localChannel(entry));
    }
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

  /** Rebind (handle, id) to the process that replaced it (⌘⇧R restart): wipe the
   *  pane and hand the backend a fresh `Channel`, since the old session — and the
   *  channel it was streaming into — is gone.
   *
   *  The xterm itself is deliberately kept rather than rebuilt. It is already at
   *  the one shared geometry, and the new PTY spawned at that same geometry, so
   *  reusing it is the only path that cannot put an emulator and a claude at
   *  different sizes (see `create`). `reset()` is what makes the wipe safe: the
   *  dead claude was drawing on the alt screen and left the pane mid-frame, and
   *  the resumed one repaints from scratch — so keeping those bytes would leave
   *  debris under a screen nothing will ever redraw.
   *
   *  Output the new child produces before this runs is not lost: the backend
   *  buffers a session's bytes until something attaches. */
  reattach(handle: number, id: number): void {
    const e = this.entries.get(keyOf(handle, id));
    if (!e || e.slot) return;
    e.term.reset();
    e.received = 0;
    attachSession(handle, id, this.localChannel(e));
  }

  /** A local pane's stream. PTY output arrives base64-chunked; a message that
   *  starts with `{` (which base64 never does) is a control frame from
   *  `pty.rs::compose`, rebuilding a pane the PTY daemon kept through an app
   *  restart: `snap` resets the xterm to a size — painting the screen the
   *  previous app serialized, if any — and says which stream offset it now
   *  shows; `r` resizes it at exactly that point in the stream, the way the
   *  PTY did (docs/rendering.md: bytes drawn for one size must never reach an
   *  xterm of another). `received` stays the stream offset throughout, so the
   *  next restart's snapshot is cut at the right byte. */
  private localChannel(e: Entry): Channel<string> {
    const term = e.term;
    const channel = new Channel<string>();
    channel.onmessage = (chunk) => {
      if (chunk.charCodeAt(0) === 123) {
        const f = JSON.parse(chunk) as { t: string; cols: number; rows: number; offset?: number; s?: string };
        if (f.t === "snap") {
          term.reset();
          if (term.cols !== f.cols || term.rows !== f.rows) term.resize(f.cols, f.rows);
          if (f.s) term.write(f.s);
          term.write("", () => term.scrollToBottom());
          e.received = f.offset ?? 0;
        } else if (f.t === "r") {
          term.write("", () => {
            if (term.cols !== f.cols || term.rows !== f.rows) term.resize(f.cols, f.rows);
          });
        }
        return;
      }
      const bytes = b64ToBytes(chunk);
      e.received += bytes.length;
      term.write(bytes);
    };
    return channel;
  }

  /** Every local pane serialized, for a restart that keeps the sessions
   *  running (`ptyd-snapshot`). Remote mirrors belong to their hosts. */
  async snapshotAll(): Promise<{ projectHandle: number; id: number; data: string; offset: number; cols: number; rows: number }[]> {
    const out = [];
    for (const e of [...this.entries.values()]) {
      if (e.slot) continue;
      const s = await this.serialize(e.handle, e.id);
      if (s) out.push({ projectHandle: e.handle, id: e.id, ...s });
    }
    return out;
  }

  dispose(handle: number, id: number): void {
    const key = keyOf(handle, id);
    const e = this.entries.get(key);
    if (!e) return;
    if (e.slot) void invoke("host_detach", { host: e.slot, projectHandle: handle, id }).catch(() => {});
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
  refit(force = false): void {
    if (this.lock) {
      // A remote desktop drives the size: local panes stay at it, and nothing
      // here may resize a PTY.
      const { cols, rows } = this.lock;
      for (const e of this.entries.values()) {
        if (!e.slot && (e.term.cols !== cols || e.term.rows !== rows)) e.term.resize(cols, rows);
      }
      return;
    }
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
    const first = !this.measured;
    this.measured = true;
    const changed = dims.cols !== this.cols || dims.rows !== this.rows || first;
    let resized = changed || force;
    this.cols = dims.cols;
    this.rows = dims.rows;
    for (const e of this.entries.values()) {
      // A mirror is sized by its host's stream (`create`), never from here.
      if (e.slot) continue;
      if (e.term.cols !== this.cols || e.term.rows !== this.rows) {
        e.term.resize(this.cols, this.rows);
        resized = true;
      }
    }
    // Match every PTY to the terminals whenever anything changed (a new terminal
    // synced, or the pane resized), so no session's PTY is left at spawn size —
    // including background projects. All PTYs share this one geometry; resize each
    // open project's sessions (one backend call per distinct project handle).
    if (resized) resizeTerminals(this.cols, this.rows);
    if (changed || force) this.onGeometry?.(this.cols, this.rows);
  }

  /** Whether (handle, id)'s claude input box is empty, holds a draft, or can't
   *  be found on screen. See `promptbox.ts`. */
  promptBox(handle: number, id: number): PromptBox {
    const e = this.entries.get(keyOf(handle, id));
    if (!e) return "none";
    return readPromptBox(e.term.buffer.active, e.term.rows, e.term.cols);
  }

  /** (handle, id)'s visible screen as plain text, one line per row. */
  screenText(handle: number, id: number): string {
    const e = this.entries.get(keyOf(handle, id));
    if (!e) return "";
    const buf = e.term.buffer.active;
    const lines: string[] = [];
    for (let y = buf.viewportY; y < buf.viewportY + e.term.rows; y++) {
      lines.push(buf.getLine(y)?.translateToString(true) ?? "");
    }
    return lines.join("\n");
  }

  /** Whether (handle, id)'s program asked for application cursor keys
   *  (DECCKM), which changes the bytes an arrow key must send: `ESC O A`
   *  instead of `ESC [ A`. Remote Control's key bar sends what xterm would. */
  appCursorKeys(handle: number, id: number): boolean {
    return this.entries.get(keyOf(handle, id))?.term.modes.applicationCursorKeysMode ?? false;
  }

  /** (handle, id)'s current selection, colors and all, for a pin (⌘⇧P). The
   *  selection is cleared once taken, so the pane shows it went somewhere. */
  pinSelection(handle: number, id: number): PinRun[][] | null {
    const e = this.entries.get(keyOf(handle, id));
    if (!e) return null;
    const lines = captureSelection(e.term, THEME);
    if (lines) e.term.clearSelection();
    return lines;
  }

  hasSelection(handle: number, id: number): boolean {
    return this.entries.get(keyOf(handle, id))?.term.hasSelection() ?? false;
  }

  /** (handle, id)'s selection as plain text, for ⌘⇧E — read the same way a pin
   *  is (soft wraps joined, blank edges trimmed), then cleared. */
  selectionText(handle: number, id: number): string | null {
    const e = this.entries.get(keyOf(handle, id));
    if (!e) return null;
    const lines = captureSelection(e.term, THEME);
    if (!lines) return null;
    e.term.clearSelection();
    const text = plainText(lines);
    return text.trim() ? text : null;
  }

  /** The focused terminal's selection as plain text, left selected — Linux
   *  Super+C, where no native Edit menu does it. */
  activeSelection(): string | null {
    const e = this.activeKey ? this.entries.get(this.activeKey) : undefined;
    if (!e || !e.term.hasSelection()) return null;
    return e.term.getSelection();
  }

  /** Paste into the focused terminal as a real paste (bracketed when the app
   *  asked for it) — Linux Super+V. */
  pasteActive(text: string): void {
    const e = this.activeKey ? this.entries.get(this.activeKey) : undefined;
    e?.term.paste(text);
  }

  /** Select the focused terminal's whole buffer — Linux Super+A. */
  selectAllActive(): void {
    const e = this.activeKey ? this.entries.get(this.activeKey) : undefined;
    e?.term.selectAll();
  }

  /** Every pane's buffer as text, with its size and scroll position — the
   *  headless-test dump (`App.svelte`, `debug_dump`). */
  dumpAll(): { key: string; text: string }[] {
    return [...this.entries.values()].map((e) => {
      const b = e.term.buffer.active;
      const lines: string[] = [];
      for (let i = 0; i < b.length; i++) lines.push(b.getLine(i)?.translateToString(true) ?? "");
      const head = `cols=${e.term.cols} rows=${e.term.rows} viewportY=${b.viewportY} baseY=${b.baseY} slot=${e.slot} received=${e.received}`;
      return { key: `${e.handle}-${e.id}`, text: `${head}\n${lines.join("\n")}` };
    });
  }

  /** Headless testing: type into the focused pane exactly as a keyboard
   *  would — through xterm, so its `onData` handler sends it. */
  inputActive(text: string): void {
    const e = this.activeKey ? this.entries.get(this.activeKey) : undefined;
    e?.term.input(text, true);
  }

  /** Re-focus the active terminal (after a dialog/menu action steals focus). */
  refocus(): void {
    if (this.activeKey) this.entries.get(this.activeKey)?.term.focus();
  }
}

export const terminals = new TerminalManager();
