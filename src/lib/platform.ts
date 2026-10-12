// Which desktop this webview runs on, and the one modifier that differs.
//
// macOS is the reference platform: its native menu owns every ⌘ shortcut before
// the webview sees a key. Linux has no such menu (see src-tauri/src/lib.rs), so
// there the webview claims Super chords itself — Super is ⌘ (the Mac's ⌘ arrives
// as Super over RDP), and Ctrl stays the terminal's own (Ctrl+C interrupts).

/** WebKitGTK's user agent carries "Linux"; WKWebView's never does. */
export const IS_LINUX = /Linux/.test(navigator.userAgent);

/**
 * WebKitGTK reports Super on NO modifier flag. Measured (WebKitGTK 2.52, X11):
 * with Super held, a `t` keydown arrives with `metaKey`, `getModifierState("OS")`,
 * `("Super")` and `("Meta")` all false — the only trace of Super is its own
 * keydown/keyup, `key: "Super"`, `code: "OSLeft"`. So it is tracked here.
 *
 * Alt does not leak into `metaKey` (also measured), but Alt still excludes a
 * chord below, cheaply: no Mulpex shortcut uses Super+Alt.
 */
let superHeld = false;

const SUPER_CODES = new Set(["OSLeft", "OSRight", "MetaLeft", "MetaRight"]);
const isSuperKey = (e: KeyboardEvent) =>
  SUPER_CODES.has(e.code) || e.key === "Super" || e.key === "Meta" || e.key === "OS";

if (IS_LINUX) {
  // Capture phase, registered at module load — before any other key listener
  // (this module is imported by terminals.ts, which loads first).
  window.addEventListener("keydown", (e) => {
    if (isSuperKey(e)) superHeld = true;
  }, true);
  window.addEventListener("keyup", (e) => {
    if (isSuperKey(e)) superHeld = false;
  }, true);
  // A keyup that lands elsewhere (Super+L locking the screen, a WM grabbing the
  // chord) must not leave Super stuck down — every later `t` would be ⌘T.
  window.addEventListener("blur", () => (superHeld = false));
}

/** Super (the Windows/⌘ key) is held for this event, on Linux. The Super key's
 *  own keydown is not itself a chord. */
export function isSuper(e: KeyboardEvent): boolean {
  if (!IS_LINUX || e.altKey || isSuperKey(e)) return false;
  return superHeld || e.metaKey || e.getModifierState("OS") || e.getModifierState("Super");
}
