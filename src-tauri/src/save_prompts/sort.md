You are sorting ONE markdown file from a software repository, so that Mulpex (an app that runs several Claude Code sessions) can show it in a list with a short Hebrew title. The file's repo path and full text are below. You may Read/Grep/Glob the repo and run read-only git commands to check whether what it describes still exists — keep that quick.

Pick exactly one kind:
- "save": notes about UNFINISHED work — a task that was started and is not done, with what is done and what is left ("continue the work", "pick up", "next steps", status in progress). It is used until the work is finished.
- "guide": a REUSABLE guide someone will open again and again — an incident or support procedure ("attach this when a client reports X"), a how-to, or a reference that answers a recurring question.
- "stale": it describes something that no longer exists or no longer applies — the code, feature, bug or system it is about is gone or fixed for good. Only when you checked and are fairly sure.
- "skip": anything else — general documentation of a system, API docs, design or style rules, a prompt or rubric that another doc hands to a subagent, proposals and research notes, TODO lists.

Also write, in Hebrew, for the human browsing the list:
- `title_he`: at most 6 words. Super simple, as if telling a manager what this is about. Name the subject itself — never start with a word for the kind of document ("מדריך", "ראנבוק", "תיעוד", "מסמך"): the list is already grouped by kind.
- `description_he`: one short sentence, at most 15 words: what it is for (for a save: where the work stands).
- In both: simple everyday Hebrew. An English term (a product, a feature, a technical word) stays in English exactly as written — never translated, never transliterated into Hebrew letters. Use such a term only when it is the natural word; no file names, commands or code.

And:
- `slug`: a short English kebab-case name, 2-5 words, no extension.
- `reason`: one short English sentence saying why you picked that kind.

Your final message must be ONLY a single JSON object, no prose before or after, no code fence:
{"kind": "...", "slug": "...", "title_he": "...", "description_he": "...", "reason": "..."}
