// Markdown → safe HTML, for the ⌘E Explain panel.
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
