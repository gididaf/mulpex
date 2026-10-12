// Wire form of a dropped path: backslash-escape everything a shell would act
// on. This is what Terminal.app/iTerm insert when you drag a file in, so the
// prompt reads `/Users/me/My\ File.csv` — matching stock Claude Code — rather
// than a quote-wrapped path. Characters ≥ U+0080 are left bare; real
// terminals don't escape unicode and a backslash there just reads as noise.
//
// Control characters take the other branch: they have no printable backslash
// form, and a literal \n or \r on the wire would submit the message halfway
// through the path. ANSI-C `$'…'` keeps the bytes CR/LF-free while still
// round-tripping to the real name.
export function escapePath(path: string): string {
  if (!/[\x00-\x1f\x7f]/.test(path))
    return path.replace(/[^\w@%+:,./=-]/g, (c) =>
      c.charCodeAt(0) < 0x80 ? "\\" + c : c,
    );
  const esc = path
    .replace(/\\/g, "\\\\")
    .replace(/'/g, "\\'")
    .replace(/\n/g, "\\n")
    .replace(/\r/g, "\\r")
    .replace(/\t/g, "\\t")
    .replace(/[\x00-\x1f\x7f]/g, (c) =>
      "\\" + c.charCodeAt(0).toString(8).padStart(3, "0"),
    );
  return `$'${esc}'`;
}

/** Paths as one bracketed paste, the trailing space inside the markers — see
 *  `App.svelte::dropPaths` for why both halves of that matter. */
export function pastePaths(paths: string[]): Uint8Array {
  return new TextEncoder().encode(`\x1b[200~${paths.map(escapePath).join(" ")} \x1b[201~`);
}
