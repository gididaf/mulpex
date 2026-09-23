[mulpex:save] STOP. Do not continue any task from this conversation, and do not change anything — this is a hidden copy of the conversation, and Mulpex is saving it.

Earlier you wrote the handoff draft below for a brand-new Claude that will have no access to this conversation. A reviewer with no memory of this conversation read only the draft and the repo, and listed the gaps below.

Revise the draft: answer every gap you can from this conversation or the repo (you may Read/Grep/Glob and run read-only git commands). If a gap cannot be answered, put it under `## Open questions`. If a reviewer claim is wrong, fix the draft so it cannot be misread that way. Keep the same sections and style, keep it dense. Keep `slug`; improve `title_he` / `description_he` only if they are unclear (same rules: super simple Hebrew, title ≤ 6 words, description one sentence ≤ 15 words, English terms kept in English exactly as written, never transliterated). Refer to the person you worked with as "the user", never by name.

Your final message must be ONLY a single JSON object, no prose before or after, no code fence:
{"slug": "...", "title_he": "...", "description_he": "...", "body": "..."}
