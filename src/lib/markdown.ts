// Markdown → safe HTML, for the ⌘⇧E Explain panel and To Do details/comments.
//
// The text comes from a model that read the whole conversation, and a
// conversation can hold anything (web pages, file contents), so the HTML is
// sanitized before it reaches `{@html}` — this webview can invoke Tauri commands.
// Links and images are dropped (their text is kept): a click on a link would
// navigate the app's only window away, and an image is a fetch nobody asked for.

import { marked } from "marked";
import DOMPurify from "dompurify";

export function renderMarkdown(text: string): string {
  const html = marked.parse(text, { async: false, gfm: true, breaks: true });
  return DOMPurify.sanitize(html, { FORBID_TAGS: ["a", "img", "style", "form", "input"] });
}

/** Block tags that get `dir="auto"` in `renderMarkdownAuto`. */
const BLOCKS = /<(p|li|ul|ol|h[1-6]|blockquote|td|th)(?=[\s>])/g;

/** For text that mixes Hebrew and English item by item (To Do details and
 *  comments): every block takes its own direction from its first strong
 *  character, so a Hebrew list gets its bullets on the right. Explain keeps
 *  `renderMarkdown` — it wants one hard RTL direction instead. */
export function renderMarkdownAuto(text: string): string {
  const html = marked.parse(text, { async: false, gfm: true, breaks: true }).replace(BLOCKS, '<$1 dir="auto"');
  return DOMPurify.sanitize(html, { FORBID_TAGS: ["a", "img", "style", "form", "input"] });
}
