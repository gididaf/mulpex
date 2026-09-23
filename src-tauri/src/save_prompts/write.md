[mulpex:save] STOP. Do not continue any task from this conversation, and do not change anything — this is a hidden copy of the conversation, and Mulpex is saving it.

Write a handoff document for this work. The reader is a brand-new Claude with NO access to this conversation: the conversation will be deleted. The document is the only thing that survives, so everything needed to continue must be in it.

First, verify the real state instead of trusting memory. You may Read/Grep/Glob files and run `git status`, `git diff`, `git log`, `git show`, `git branch` (read-only; nothing else is allowed). Check what is actually committed, what is uncommitted, and what exists on disk.

The body (English, markdown, for a Claude reader) must contain these sections, and skip a section only if it is truly empty:
- `## Goal` — what we are trying to achieve and why, in 2-4 sentences.
- `## Status` — one line: done / in progress / blocked, and on what.
- `## Context` — facts the reader needs that are not obvious from the code: systems, IDs, environments, people, constraints, what the user asked for in their own words when it matters.
- `## Done` — what is finished, with file paths, commits (short hash + subject), and whether it is committed, uncommitted, deployed.
- `## Left` — an ordered list of concrete next steps. Each step specific enough to start immediately.
- `## Decisions` — decisions made and WHY, including approaches tried and rejected, so the reader does not redo them.
- `## Traps` — gotchas, things that broke, things that look right but are wrong.
- `## How to verify` — commands or checks that prove the work is correct.
- `## Open questions` — things only the user can answer.

Leave out chat history narration and anything no longer relevant. Be complete but dense. Other people on the team will read this too: refer to the person you worked with as "the user", never by name.

Also write, in Hebrew, for the human who will browse a list of saved sessions:
- `title_he`: at most 6 words. Super simple, as if telling a manager what this is about.
- `description_he`: one short sentence, at most 15 words: what the work is and where it stands (e.g. "כמעט גמור, נשאר לבדוק בשרת").
- In both: simple everyday Hebrew. An English term (a product, a feature, a technical word) stays in English exactly as written — never translated, never transliterated into Hebrew letters. Use such a term only when it is the natural word; no file names, commands or code.

And `slug`: a short English kebab-case file name, 2-5 words, no extension.

Your final message must be ONLY a single JSON object, no prose before or after, no code fence:
{"slug": "...", "title_he": "...", "description_he": "...", "body": "..."}
