# ⌘K Secrets: a password a claude can use but never sees

**The problem.** Anything typed into a claude's prompt — an ssh password, nginx basic-auth — lives
in the transcript forever, so the advice afterwards is always "rotate it". The manual fix was to
write a `.env` under `/tmp` by hand and point claude at it. Claude already sources such a file
without printing it. ⌘K is that fix, built in.

## The flow

1. ⌘K (Session ▸ Secrets…) or right-click a claude row ▸ Secrets…. Claude rows only — a terminal
   or a remote claude has no access to a local file.
2. The dialog (`SecretsDialog.svelte`) takes KEY=VALUE rows. Values are masked, with an eye toggle.
3. Mulpex writes a 0600 file (dir 0700, temp file + rename) and types **only `🔑 KEY, KEY `** into
   the prompt, with no Enter.
4. The path and the rules reach the claude through the **`UserPromptSubmit` hook**, as
   `additionalContext` (`mulpex_core::secrets::context`). The rules say: load it with
   `set -a; . <file>; set +a`, use `$KEY`, and never print, cat or Read a value.

**Why the hook, not the typed line.** The first version typed the whole instruction (path, keys,
how to load it) into the prompt — about 300 characters of noise in every request. The user asked
for something cleaner. The note now rides **every** turn, including `<task-notification>` turns, for
as long as the file exists. That keeps it alive across `/compact`, and it stops by itself once the
file is gone. It is information, not a request, so the `nudges_welcome` gate does not apply (same
reasoning as the peer snapshot).

**Hint only, no blocking.** No hook denies Read or cat on these files. Measured on real use: told
the rules, claude refuses even a direct "print it" and offers `! cat <path>` so the user can look
themselves. The user chose not to add a hard block.

## Two kinds of set

| | One-off (default) | Saved ("Save for reuse") |
| --- | --- | --- |
| File | `<state_dir>/secrets/<id>/secrets-N.env` | `<mulpex home>/secrets/<name>.env` (`~/.mulpex-dev` in debug) |
| Handed to a claude as | the file itself | `<state_dir>/secrets/<id>/<name>.ref`, holding the saved file's path |
| Gone when | the claude is reaped (`Core::forget_session_files`), or the app quits (scratch-dir teardown) | the user deletes it; reaping removes only the ref |
| Scope | that one claude | global; "This project only" adds a `# mulpex-project: <dir>` header, which hides it from other projects |

A ref whose target was deleted is skipped by `secrets::files_of`, so deleting a saved set needs no
sweep through instances. An edit takes effect the next time a claude sources the file.

A one-off file sits in `$TMPDIR` and is written once, so the three-day `$TMPDIR` purge can take it
from a claude left open that long. That is acceptable for a one-off.

## File format (shared by two processes)

The app writes the file and the helper (the hook) reads it, so the layout lives in
`mulpex-core/src/secrets.rs`:

```
# keys: SSH_USER SSH_PASS          ← line 1, always; the hook names keys from here only
# mulpex-project: /path/to/project ← line 2, saved + project-only sets only
SSH_USER='root'
SSH_PASS='it'\''s $ecret'          ← single-quoted, ' written as '\''
```

The keys come from the header and are never scanned from the body: a value can span lines, and a
line inside a value could look like `KEY=`. Edit reads the values back with `secrets::parse`. It
fails on anything Mulpex did not write, rather than half-loading a hand-edited file and saving it
back over itself.

Saving under a name that already exists is refused, so a typo can't overwrite a password. Use Edit.

## Code

- `crates/mulpex-core/src/secrets.rs`: layout, headers, `files_of`, the hook note.
- `src-tauri/src/secrets.rs`: render, parse, write, list, save, attach, update, delete, with tests
  (including a bash `source` round-trip of nasty values).
- `commands.rs` `secrets_*`; `menu.rs` `secrets` (Cmd+K, listed in `is_forwarded`).
- `App.svelte` `openSecrets` / `sendSecretsTag`: refused with a toast on an open question
  (`needs`) or no input box. A draft is allowed; the tag is appended to it.
