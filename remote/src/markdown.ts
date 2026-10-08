// Just enough markdown for claude's replies on a phone: fenced code, inline
// code, bold, headings and links. Everything is HTML-escaped FIRST, and the
// only tags produced are the fixed ones below, so text from a transcript can't
// inject markup.

function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

function inline(s: string): string {
  return s
    .replace(/`([^`\n]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*\n]+)\*\*/g, "<b>$1</b>")
    .replace(/\[([^\]\n]+)\]\((https?:\/\/[^\s)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
}

/** One paragraph per block, each its own BiDi paragraph (`dir="auto"`), so a
 *  Hebrew line and an English line each read in their own direction. */
export function render(md: string): string {
  const out: string[] = [];
  const parts = esc(md).split(/^```[^\n]*\n?/m);
  parts.forEach((part, i) => {
    if (i % 2 === 1) {
      out.push(`<pre dir="ltr">${part.replace(/\n$/, "")}</pre>`);
      return;
    }
    for (const block of part.split(/\n{2,}/)) {
      const t = block.replace(/^\n+|\n+$/g, "");
      if (!t) continue;
      const h = t.match(/^#{1,6} (.*)$/);
      if (h && !t.includes("\n")) out.push(`<p dir="auto"><b>${inline(h[1])}</b></p>`);
      else out.push(`<p dir="auto">${inline(t)}</p>`);
    }
  });
  return out.join("");
}
