// Linux: the webview owns every shortcut, because there is no native menu (the
// in-window MenuBar.svelte is drawn by the webview too, and runs the same ids).
//
// On macOS `src-tauri/src/menu.rs` declares each ⌘ accelerator and AppKit
// consumes the key before xterm sees it. On Linux no menu is installed (a GTK
// strip in the window was declined), so this table is that menu: the same ids,
// fed into the same `handleMenu`, with Super standing in for ⌘. Keep the two in
// step — a shortcut added to menu.rs and not here simply does nothing on Linux.
//
// Copy / paste / select-all were the predefined Edit-menu items on the Mac.
// Here they are Super+C / V / A; plain Ctrl+C is never touched, so it stays the
// terminal's interrupt.

import { readText, writeText } from "@tauri-apps/plugin-clipboard-manager";
import { isSuper } from "./platform";
import { terminals } from "./terminals";

/** `${alt ? "A-" : ""}${shift ? "S-" : ""}${e.code}` → menu id. Super is implied. */
const CHORDS: Record<string, string> = {
  KeyO: "open_project",
  "S-KeyW": "close_project",
  "S-BracketRight": "next_project",
  "S-BracketLeft": "prev_project",
  "S-ArrowLeft": "move_project_left",
  "S-ArrowRight": "move_project_right",
  KeyT: "new_session",
  "S-KeyT": "new_terminal",
  KeyW: "close_session",
  "S-KeyO": "remote_control",
  KeyR: "rename",
  "S-KeyR": "restart",
  KeyS: "save_session",
  KeyL: "load_session",
  KeyM: "mute",
  "S-KeyM": "messages",
  "S-KeyE": "explain",
  KeyK: "secrets",
  "S-KeyP": "pin_selection",
  "S-KeyL": "toggle_panel",
  "S-KeyK": "request_compact",
  "S-KeyB": "ask_all",
  BracketRight: "next",
  BracketLeft: "prev",
  "S-ArrowUp": "move_instance_up",
  "S-ArrowDown": "move_instance_down",
  "S-KeyQ": "checkpoint_quit",
  KeyQ: "quit",
  "A-BracketRight": "next_host",
  "A-BracketLeft": "prev_host",
  "A-KeyT": "new_codex_session",
  "A-KeyS": "continue_other_agent",
};
for (let n = 1; n <= 9; n++) CHORDS[`Digit${n}`] = `project_${n}`;

/** Chords that are a text-selection gesture inside a real text field (the
 *  rename box, the palette query), so they are left to it there. */
const SELECTION_CHORDS = new Set(["S-ArrowLeft", "S-ArrowRight", "S-ArrowUp", "S-ArrowDown"]);

/** The menu id a key event maps to, or null. Pure, for the table's sake. */
export function chordId(e: Pick<KeyboardEvent, "code" | "shiftKey" | "ctrlKey" | "altKey">): string | null {
  if (e.ctrlKey) return null;
  // Alt is part of the chord: without it, Super+Alt+[ read as Super+[.
  return CHORDS[`${e.altKey ? "A-" : ""}${e.shiftKey ? "S-" : ""}${e.code}`] ?? null;
}

/** An editable field that is not xterm's own hidden textarea. */
function textField(el: EventTarget | null): HTMLInputElement | HTMLTextAreaElement | HTMLElement | null {
  const t = el as HTMLElement | null;
  if (!t?.tagName) return null;
  if (t.classList.contains("xterm-helper-textarea")) return null;
  if (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable) return t;
  return null;
}

function fieldSelection(f: HTMLElement): string {
  if (f instanceof HTMLInputElement || f instanceof HTMLTextAreaElement) {
    const { selectionStart: a, selectionEnd: b, value } = f;
    return a != null && b != null ? value.slice(a, b) : "";
  }
  return window.getSelection()?.toString() ?? "";
}

async function copy(target: EventTarget | null, cut: boolean): Promise<void> {
  const field = textField(target);
  const text = field
    ? fieldSelection(field)
    : terminals.activeSelection() ?? window.getSelection()?.toString() ?? "";
  if (!text) return;
  await writeText(text).catch(() => {});
  if (cut && field) document.execCommand("delete");
}

async function paste(target: EventTarget | null): Promise<void> {
  const text = await readText().catch(() => "");
  if (!text) return;
  const field = textField(target);
  if (field) {
    field.focus();
    // Goes through the field's own input events, so Svelte bindings update.
    document.execCommand("insertText", false, text);
  } else {
    terminals.pasteActive(text);
  }
}

/** The Edit menu's items (`MenuBar.svelte`), acting on what has focus — the
 *  same code Super+C / X / V / A run. */
export function editAction(kind: "copy" | "cut" | "paste" | "select_all"): void {
  const target = document.activeElement;
  if (kind === "copy" || kind === "cut") void copy(target, kind === "cut");
  else if (kind === "paste") void paste(target);
  else if (textField(target)) document.execCommand("selectAll");
  else terminals.selectAllActive();
}

/**
 * Install the capture-phase listener. Capture, not bubble: xterm's keydown
 * listener sits on its textarea, which a bubbling window listener would only
 * reach after xterm had already encoded the key for the PTY.
 */
export function installLinuxKeys(
  handleMenu: (id: string) => void,
  togglePalette: () => void,
): () => void {
  const onKey = (e: KeyboardEvent) => {
    if (!isSuper(e)) return;
    const claim = () => {
      e.preventDefault();
      e.stopPropagation();
    };
    if (!e.shiftKey && !e.ctrlKey && !e.altKey) {
      switch (e.code) {
        case "KeyC":
          claim();
          void copy(e.target, false);
          return;
        case "KeyX":
          claim();
          void copy(e.target, true);
          return;
        case "KeyV":
          claim();
          void paste(e.target);
          return;
        case "KeyA":
          claim();
          if (textField(e.target)) document.execCommand("selectAll");
          else terminals.selectAllActive();
          return;
        case "KeyP":
          claim();
          togglePalette();
          return;
      }
    }
    const key = `${e.shiftKey ? "S-" : ""}${e.code}`;
    const id = chordId(e);
    if (!id) return;
    if (SELECTION_CHORDS.has(key) && textField(e.target)) return;
    claim();
    handleMenu(id);
  };
  window.addEventListener("keydown", onKey, true);
  return () => window.removeEventListener("keydown", onKey, true);
}
