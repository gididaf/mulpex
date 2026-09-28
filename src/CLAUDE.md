# Frontend (Svelte + xterm.js)

Root rules: [../CLAUDE.md](../CLAUDE.md). Two things here are permanent-damage classes, so read the
doc *before* the code:

- **Never reintroduce the WebGL renderer**, and don't touch either RTL rule in `styles.css` —
  `display: inline !important` on `.xterm-rows span` (words) or `unicode-bidi: plaintext` on
  `.xterm-rows > div` (sentences). Any of the three silently breaks Hebrew, and they fix *different*
  layers, so the pane can look half-right. The `plaintext` rule must stay on the row divs; on
  `.xterm-rows` it does nothing. → [../docs/rendering.md](../docs/rendering.md)
- **An xterm must never be built at a size that disagrees with its PTY.** `terminals.setGeometry()`
  runs before any `TerminalView` mounts, and resize is workspace-wide. Debris from a mismatch is
  permanent. → [../docs/rendering.md](../docs/rendering.md)

Everything else about this directory — dropped paths (bracketed paste, and why the trailing space is
*inside* the markers), the sidebar's claudes-then-terminals split and its right-click menu, mute
ordering, drag-to-reorder clamping, the two tab badges, the dock badge and notifications, and why
the hub panel is Messages-only — is in [../docs/frontend.md](../docs/frontend.md).

Several behaviors here were removed *deliberately* and are documented as such: the
drag-a-folder-to-open-a-project gesture, and the hub panel's Waiting/Locks sections. Don't restore
them without asking.

Hidden terminals use `visibility: hidden`, **never** `display: none` — the latter zeroes their size
and breaks `fit()`.

⌘E (`App.svelte::explainInstance`) types `/explain` into the focused claude, but only after
`promptbox.ts` reads its input box as empty off the xterm buffer — a guess at someone else's TUI,
measured and documented in [../docs/explain.md](../docs/explain.md). Keep it refusing (with a toast)
on anything it does not recognize; typing onto a draft or into an open dialog is the failure.
