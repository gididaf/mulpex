# Terminal rendering: geometry and RTL

Two invariants about how xterm.js is fed. Both are permanent-damage classes — get either wrong and
the pane is wrong for the rest of the session. Read this before touching `terminals.ts`,
`TerminalView.svelte`, `TerminalPane.svelte`, `styles.css`, or any spawn/resize path in
`pty.rs`/`state.rs`.

Back to [CLAUDE.md](../CLAUDE.md).

## One geometry, or the pane is corrupted forever

**An xterm must never be fed bytes that were rendered for a different size.** Not "should not" —
the damage is permanent, and it is why the last few rows of a pane could sit showing stale content
indefinitely.

Claude Code (v2.1.226+) draws on the **alternate screen** and repaints it **differentially**: a
frame is `ESC[H`, then runs of `ESC[<n>B` to *skip* rows it believes are already correct, and
writes only the rows that changed. Measured on a real capture at 204x55 — 146 `ESC[H`, 123
`ESC[48B`, and in 20 KB of repaint exactly one `ESC[2J` and thirteen `ESC[K`. It essentially never
erases. So a row the emulator holds wrong is never rewritten, because claude is not going to write
there again — and for a row it thinks is *blank* that is forever.

Mulpex had two sizes for one session. The backend spawned every PTY at `DEFAULT_COLS`x`DEFAULT_ROWS`
(120x32), while the frontend built the xterm with `new Terminal({…})` — i.e. at **xterm's own 80x24
default** — and `attach_session` then flushed everything the PTY had already printed into it.
`refit()` corrected both afterwards, which is far too late: at launch the PTYs have been running
since before the window painted, so the flush *is* claude's entire startup paint, and `refit`
early-returns (`if (!active) return`) until the first `focus()`, which `bootstrapProject` only
reaches after every `TerminalView` has mounted and attached.

**Too few rows/cols is the destructive direction**, and that is the part worth remembering: content
scrolls off the top or hard-wraps, and what scrolled away can never be repainted. Measured, real
`claude` on a PTY replayed through this exact xterm build (`scratchpad/harness2.py` + `replay2.mjs`):
a stream rendered for 120x32 fed to an **80x24** emulator and then resized keeps its debris to the
end of the session — a leftover box rule struck through `⏺─I'll─look─at─…`, `| Ctx Used: 3.0%` with
its `Model:` gone, the input box's own border missing, welcome-banner fragments stranded in rows
1-5. The **same stream** into an emulator that was 120x32 all along is byte-identical to tmux's
render of it.

The fix is therefore not "resize sooner" but **one geometry both sides always agree on**:

- `Workspace::geometry` is the single (cols, rows) every PTY in every project runs at. Every spawn
  path uses it — restore, ⌘T, `hub_spawn`, ⌘⇧T — so a session created now matches the xterm about
  to be built for it.
- `bootstrap()` reports it (`WorkspaceInfo::cols/rows`) and `App.svelte` applies it with
  `terminals.setGeometry()` **before any `TerminalView` mounts**. `create()` then builds each
  Terminal at `this.cols/this.rows`, never at the 80x24 default.
- Resize is **workspace-wide**, not per project: `resize_terminals(cols, rows)` (was
  `resize_session(handle, …)`, one call per handle). A project the frontend has no xterms for yet
  was otherwise left at the stale size and would spawn its next session there, handing the frontend
  a PTY of a shape its terminal never had.

**A resize while the two agree is fully self-healing** — claude repaints everything on SIGWINCH.
Measured by replaying with the emulator's resize deliberately skewed ±200 and +2000 bytes from the
PTY's: the final screen is identical to the unskewed one in every case. That is what makes the
small async gap inside `refit()` (xterm resizes synchronously, the PTY over IPC) harmless, and it
is why *attach time* is the only moment that has to be exact.

Pinned by `a_session_spawns_at_the_geometry_the_frontend_will_build_its_xterm_at`, which reads the
size off a live PTY (a real shell, so it cannot pass vacuously) and was confirmed to fail —
`left: (120, 32) right: (100, 40)` — with the spawn reverted to the fixed default. `TEST_GEOMETRY`
is deliberately not the DEFAULT pair, so the assertion cannot pass by reading the default back.

Diagnosis note, because every cheap step here was misleading: the same byte stream replayed through
xterm.js at the *right* size matches tmux exactly, and a plain replay of a captured session shows
nothing wrong. The bug is invisible unless the replay reproduces the *size history*, so the harness
has to record the PTY's resize offsets alongside the bytes and apply them at the identical point.

## Terminals kept alive while hidden

`TerminalPane` stacks the xterms of **every session across every open project** absolutely (keyed
`(handle,id)`); exactly one — the active project's active session — is `visibility: visible`, all
the rest `visibility: hidden` (**never** `display:none`, which would zero their size and break
`fit()`). Hidden terminals (including whole background *projects*) keep receiving `term.write()`,
so background Claudes keep rendering. Geometry is central — and load-bearing, not just tidy: a
`ResizeObserver` on the pane fits the visible terminal, then applies the same `cols/rows` to every
session + backend PTY (all PTYs share one size, as the TUI did) via a single workspace-wide
`resize_terminals(cols, rows)`, so no project is left at spawn size. A terminal whose PTY is a
different size is corrupted **permanently**, so the sizes are also matched before a terminal is
ever attached — see **One geometry, or the pane is corrupted forever**.

## RTL (Hebrew/Arabic) — two separate fixes, both load-bearing

Terminals use xterm's **DOM renderer**. The WebGL addon was removed for this fix and **must not
come back for speed** — it draws one glyph quad per cell, so column *n* always gets character *n* and
RTL text renders mirrored (Hebrew read backwards). The DOM renderer emits each styled run as a
`<span>` of real text and the **browser's own BiDi engine** reorders it for free. Measured, same
frame through the app's xterm 5.5.0 in headless Chrome: DOM → `שלום זאת בדיקה`, WebGL →
`הקידב תאז םולש` (the reported bug, reproduced). xterm has **no BiDi of its own** (`grep -c
"bidi\|rtl"` on `lib/xterm.js` is 0), so the browser is the only implementation available; a
`unicode-bidi: plaintext` CSS override on the rows changes nothing (already the default behavior),
and `direction: rtl` flips the box-drawing borders and is unusable. Dropping the addon also
deleted the GL-context juggling (attach-on-focus / dispose-on-blur, since browsers cap live
contexts) and cut ~100 kB from the JS bundle.

**That alone only fixed the letters.** Words still ran left-to-right, because xterm's DOM renderer
injects `.xterm-rows span { display: inline-block }` — and an inline-block is an **atomic** inline
box, which the BiDi algorithm treats as one opaque object. It can reorder text *inside* a span but
never *across* spans, and Claude Code colors words individually (`<span>Opus</span><span>
</span><span>5</span>…`), so a Hebrew sentence became one span per word: letters right, words
mirrored-wrong. `src/styles.css` overrides it with `display: inline !important` (`!important` is
required — the injected rule is more specific and lands in `<head>` later). Measured on a 7-span
row: `inline-block` → first word leftmost; `inline` → whole run mirrored, correct. `inline` rather
than `display: contents` on purpose — the span keeps its box, so background colors, the block
cursor and wide-char widths still paint (CJK/emoji x-positions verified byte-identical).

How this was found, for the next RTL bug: a Python `pty.fork()` harness drove a real `claude`,
typed the sentence keystroke-by-keystroke and captured the raw bytes; those bytes were replayed
into the app's own xterm in headless Chrome, and **each character's x-position was measured** with
`Range.getBoundingClientRect` rather than eyeballed — reading Hebrew out of a screenshot is
useless here, because transcribing it silently re-applies BiDi and hides which end is which.

Residual limit: the caret is still column-based, so it can sit visually off inside an RTL run.

