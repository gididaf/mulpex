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

The Explainer column (`ExplainerPanel.svelte`, the `explains` map in `stores.ts`) is documented in
[../docs/explainer.md](../docs/explainer.md) — including why its text is hard `dir="rtl"` and never
`dir="auto"` (entries often *start* with an English identifier, which flips auto to LTR). It is
**open by default** and fills itself after every turn; ⌘⇧E is a plain show/hide and nothing in the
frontend ever discards an entry except a row exiting. The feed is capped at 10 per instance in
`stores.ts` (`MAX_EXPLAIN_ENTRIES`, mirroring the backend) — keep the two caps equal, or a reload
shows a different history than the live feed did. The panel's feed rules (render-time reversal,
sticky scroll, `opacity: 0.18` history, `:last-of-type` not `:last-child`) only make sense together;
don't remove one piecemeal.
