# Frontend (Svelte + xterm.js)

Root rules: [../CLAUDE.md](../CLAUDE.md). Two things here are permanent-damage classes, so read the
doc *before* the code:

- **Never reintroduce the WebGL renderer**, and don't touch the `display: inline !important` rule on
  `.xterm-rows span` in `styles.css`. Either one silently breaks Hebrew (the user works in Hebrew).
  → [../docs/rendering.md](../docs/rendering.md)
- **An xterm must never be built at a size that disagrees with its PTY.** `terminals.setGeometry()`
  runs before any `TerminalView` mounts, and resize is workspace-wide. Debris from a mismatch is
  permanent. → [../docs/rendering.md](../docs/rendering.md)

Everything else about this directory — dropped paths (bracketed paste, and why the trailing space is
*inside* the markers), mute ordering, drag-to-reorder clamping, the two tab badges, the dock badge
and notifications, and why the hub panel is Messages-only — is in
[../docs/frontend.md](../docs/frontend.md).

Several behaviors here were removed *deliberately* and are documented as such: the
drag-a-folder-to-open-a-project gesture, and the hub panel's Waiting/Locks sections. Don't restore
them without asking.

Hidden terminals use `visibility: hidden`, **never** `display: none` — the latter zeroes their size
and breaks `fit()`.
