// Reading claude's input box off the xterm screen, for anything Mulpex types
// into it (⌘K's key tag, Remote Control's messages): typing onto a draft would
// glue the two together, so it must know whether the box is empty first.
//
// There is no declared interface for this — it is claude's TUI, read back — so
// the rule is kept to the two facts measured on claude 2.1.283 (tmux
// `capture-pane -e`, idle / working / after a turn / multi-line):
//
//   ────────────────   ← a rule row directly above
//   ❯ <box content>    ← the prompt row, "❯ " at column 0
//     <more lines>     ← a multi-line draft continues here
//   ────────────────   ← the closing rule
//
// and: everything claude puts in an EMPTY box is drawn dim (SGR 2) — the
// `Try "…"` placeholder, the next-prompt suggestion, `Press up to edit queued
// messages` — while text the user typed is not. So the box is empty exactly
// when every cell after "❯ " up to the closing rule is blank or dim.
//
// The history also contains "❯ " rows (each past prompt is echoed that way),
// which is why the rule row above is required and the scan runs bottom-up.
// A box that can't be found at all — a question or plan dialog replaces it,
// claude is still starting — is "none", and ⌘E does nothing.

import type { IBuffer } from "@xterm/xterm";

export type PromptBox = "empty" | "draft" | "none";

const RULE = "─";
const PROMPT = "❯";

/** A run of "─", or — once a session is named (`/rename`, measured on claude
 *  2.1.296) — a run with the name set into its right end:
 *  `──────── database-migration-analysis ─`. Missing that second form made
 *  every named claude read as "no prompt", so the phone could type into none. */
const RULE_ROW = new RegExp(`^${RULE}{10,}(?: \\S.* ${RULE}+)?$`);

function isRule(text: string): boolean {
  return RULE_ROW.test(text.trim());
}

/** The box's own prompt is "❯" + U+00A0 (NO-BREAK SPACE), not a plain space —
 *  measured; matching on "❯ " finds nothing. Either is accepted. */
function isPromptRow(text: string): boolean {
  return text.startsWith(PROMPT + " ") || text.startsWith(PROMPT + " ");
}

/** The state of claude's input box on the visible screen of `buf`. */
export function readPromptBox(buf: IBuffer, rows: number, cols: number): PromptBox {
  const top = buf.viewportY;
  const lineText = (y: number) => buf.getLine(y)?.translateToString(true) ?? "";

  for (let y = top + rows - 1; y > top; y--) {
    if (!isPromptRow(lineText(y)) || !isRule(lineText(y - 1))) continue;

    const cell = buf.getNullCell();
    for (let row = y; row < top + rows; row++) {
      if (row > y && isRule(lineText(row))) return "empty";
      const line = buf.getLine(row);
      if (!line) break;
      for (let x = row === y ? 2 : 0; x < cols; x++) {
        if (!line.getCell(x, cell)) break;
        const ch = cell.getChars();
        if (ch !== "" && ch.trim() !== "" && !cell.isDim()) return "draft";
      }
    }
    // No closing rule on screen: not a box we understand, so don't type.
    return "none";
  }
  return "none";
}
