// Full Markdown → safe HTML for To Do details and comments, which a claude
// writes on purpose to be read (QA steps, tables, code). Same rules as the
// desktop's `src/lib/markdown.ts`: sanitized before `{@html}`, links and
// images dropped (their text is kept). Chat keeps the lighter `markdown.ts`.

import { marked } from "marked";
import DOMPurify from "dompurify";

export function renderRich(text: string): string {
  // Each block takes its own direction (a Hebrew list gets right-side bullets).
  const html = marked
    .parse(text, { async: false, gfm: true, breaks: true })
    .replace(/<(p|li|ul|ol|h[1-6]|blockquote|td|th)(?=[\s>])/g, '<$1 dir="auto"');
  return DOMPurify.sanitize(html, { FORBID_TAGS: ["a", "img", "style", "form", "input"] });
}
