// `npm test`: promptbox.ts against real screens, replayed through the real
// xterm build. The codex fixtures are raw PTY streams recorded from codex-cli
// 0.149.1 in tmux (`pipe-pane`, 100x30, `--no-alt-screen`, 2026-10-21);
// `session.json` holds the byte offset at which each state was on screen.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import xterm from "@xterm/xterm";
import { readPromptBox } from "./promptbox.ts";

const { Terminal } = xterm;
const dir = new URL("./fixtures/codex/", import.meta.url);
const COLS = 100;
const ROWS = 30;

/** The screen after `bytes`, as an xterm buffer. */
function screen(bytes) {
  const term = new Terminal({ cols: COLS, rows: ROWS, allowProposedApi: true, scrollback: 1000 });
  return new Promise((resolve) => term.write(bytes, () => resolve(term)));
}

async function read(bytes, agent) {
  const term = await screen(bytes);
  return readPromptBox(term.buffer.active, term.rows, term.cols, agent);
}

const session = readFileSync(new URL("session.bin", dir));
const at = JSON.parse(readFileSync(new URL("session.json", dir), "utf8"));

const EXPECT = {
  empty: "empty",
  draft: "draft", // Hebrew and English typed into the box
  multi: "draft", // ESC+CR, then a second line
  empty_after_delete: "empty",
  slash: "draft", // "/" with the command popup open below the box
  busy: "empty", // the placeholder stays while a turn runs; status gates typing
  after_turn: "empty",
  draft_after_turn: "draft",
  empty_after_turn: "empty",
};

for (const [state, want] of Object.entries(EXPECT)) {
  test(`codex: ${state} reads ${want}`, async () => {
    assert.equal(await read(session.subarray(0, at[state]), "codex"), want);
  });
}

test("codex: the directory-trust picker is no box", async () => {
  assert.equal(await read(readFileSync(new URL("trust.bin", dir)), "codex"), "none");
});

test("codex: a past prompt in the history (bold + dim ›) is not the box", async () => {
  assert.equal(await read("\x1b[1;2m› \x1b[0mold prompt\r\n• answer\r\n", "codex"), "none");
});

test("codex: a box with neither placeholder nor text is refused", async () => {
  assert.equal(await read("\x1b[1m›\x1b[0m \r\n  gpt-5 · /x\r\n", "codex"), "none");
});

test("each reader refuses the other agent's screen", async () => {
  assert.equal(await read(session.subarray(0, at.empty), "claude"), "none");
  const claude = "─".repeat(40) + "\r\n❯ \x1b[2mTry \"fix it\"\x1b[0m\r\n" + "─".repeat(40) + "\r\n";
  assert.equal(await read(claude, "claude"), "empty");
  assert.equal(await read(claude, "codex"), "none");
});
