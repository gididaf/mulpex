# Packaging, signing, updates, teardown

Everything about the shipped `.app` rather than the running code: how the helper gets bundled, why
the signing identity is load-bearing, how auto-update works, and what runs on the way out.

Back to [CLAUDE.md](../CLAUDE.md).

## Bundling the helper (signed sidecar)

The dev flow needs nothing (helper sits beside the app in `target/`). For `tauri build`,
`mulpex-helper` ships as a **signed sidecar** so it lands in `Contents/MacOS/` *and is signed with
the bundle* — otherwise Gatekeeper SIGKILLs it and **every hook fails-open silently** (no
coordination, no error). This is **wired**: `bundle.externalBin` is `["binaries/mulpex-helper"]`
in `tauri.conf.json`, and `beforeBuildCommand` runs `scripts/bundle-helper.sh`, which builds the
helper in release and copies it to `src-tauri/binaries/mulpex-helper-<target-triple>` (the
suffix Tauri expects). Tauri strips the suffix, places it at `Contents/MacOS/mulpex-helper`, and
signs it with the app. Bundle `targets` are `["app", "dmg"]`, so `tauri build` produces both
`Mulpex.app` and `Mulpex_<version>_aarch64.dmg`. Verified: the built `.app` has the signed helper
beside the main binary.

## macOS file access (TCC) — the failure with no symptom, and why signing is load-bearing

**Symptom, as the user experiences it: "Claude refuses to open."** A session appears in the sidebar
for about 100 ms, an error flashes in the pane, and both are gone. Every project is affected at
once, so the whole app looks broken. Nothing is logged anywhere.

**Cause.** macOS TCC protects `~/Documents`, `~/Desktop` and `~/Downloads` — where people keep
code. Mulpex spawns `claude` with `cwd` set to the project, and if the app has not been allowed
into that folder the child cannot even resolve its own directory:

```
job-working-directory: error retrieving current directory: getcwd: cannot access parent
directories: Operation not permitted
```

`claude` exits **1 in the same second**; the poll loop reaps the session; the row vanishes. The
denial is recorded **per bundle id and never asked about again** (`auth_value=0` in
`~/Library/Application Support/com.apple.TCC/TCC.db`), so it is permanent until reset.

Diagnosed 2026-08-05 by measurement, and the sequence is worth repeating because every cheaper
step was misleading: the same `claude` invocation ran fine from a terminal (that inherits the
terminal's *own* TCC grant); reproducing it needed the **launchd** environment a Finder launch
gets (`PATH=/usr/bin:/bin:/usr/sbin:/sbin`, no `TERM`, no `LANG`). Ground truth came from a
transparent shim on `PATH` logging the real argv/env/exit code — all 8 spawns `rc=1`. The control
that proved it was the *folder* and not the app: the same build opened a project in `/private/tmp`
and `claude` stayed alive.

**Three defences, and the third is the one that matters most for other users:**

- **Preflight** (`pty::dir_access_error`, called from `Core::spawn_with`): ⌘T reads the project dir
  first and refuses with the folder name plus the Settings path. Deliberately **not** applied to
  restores — refusing there would reopen the project to an empty sidebar, which is the original
  bug. A restore instead spawns, fails, and becomes a visible failed row.
- **A session that dies within `EARLY_DEATH_GRACE` (10 s) is kept**, not reaped —
  see [sessions.md](sessions.md).
- **Usage strings** (`src-tauri/Info.plist`, auto-merged by the bundler because it sits next to
  `tauri.conf.json`). Without `NSDocumentsFolderUsageDescription` the prompt appears with no reason
  attached, which makes "Don't Allow" the natural click.

**Signing is what makes an Allow stick, and every release before v0.6.0 got this wrong.**
`tauri build` does *not* sign the bundle: the output has no `_CodeSignature`, only the
linker-signed ad-hoc signature Rust puts on every arm64 binary (`flags=0x20002(adhoc,
linker-signed)`, `Sealed Resources=none`), and a **random per-build identifier**
(`mulpex-55de470ef4e5b764`). Verified against the *published* v0.5.0 tarball, not just a local
build. macOS will not persist a TCC grant for a bundle whose signature does not validate, and a
changing identifier makes every update look like a brand-new app — so users were re-asked on every
release until someone clicked Don't Allow, at which point they hit the permanent failure above.
v0.6.0 fixed *half* of this with `bundle.macOS.signingIdentity: "-"`, which ad-hoc signs the `.app`
**before** the `.tar.gz` and `.dmg` are built from it; the identifier then falls back to the stable
`com.mulpex.app` and `codesign --verify` passes. Don't assume a green build means a signed one —
`release.sh` only checks the artifacts *exist*.

**Ad-hoc signing is not enough, and v0.7.0 replaced it with a self-signed certificate.** A grant is
pinned to the bundle's **designated requirement**, and for an ad-hoc signature that requirement is a
bare `cdhash`:

```
# ad-hoc      designated => cdhash H"0f8d43466974ae615256b442ef8535f743dc853e"
# certificate designated => identifier "com.mulpex.app" and certificate root = H"356eabc7…"
```

A cdhash is the hash of *those exact bytes*, so **every rebuild is a different application to TCC**
and silently discards every folder permission the user ever gave — for them and for every other
user, on every release. That is the "Don't Allow" trap above, fired on a schedule. Measured
directly: after an in-place update, `codesign --verify -R='cdhash H"<old>"'` on the new bundle fails
with *"code failed to satisfy specified code requirement(s)"* while the TCC row still reads
`auth_value=2`, i.e. allowed-but-unsatisfiable.

`signingIdentity` is therefore the SHA-1 of a **self-signed code-signing certificate**, and the
requirement is now anchored to the certificate instead of the bytes, so a rebuild inherits the
existing grant. Details that cost time to rediscover:

- The identity lives in the login keychain; the cert + key + `.p12` are backed up at
  `~/.mulpex/signing/` (0600, **not** in the repo). It is now as load-bearing as `updater.key`:
  lose it and every future build mints a new identity, resetting permissions for everyone again.
- **No Apple Developer account, no trust settings and no admin prompt are needed.** `codesign` signs
  happily with an untrusted self-signed identity (`security find-identity -v` reports
  `CSSMERR_TP_NOT_TRUSTED` and lists 0 *valid* identities — that is fine). Trust affects Gatekeeper,
  which is irrelevant here: the app is unnotarized either way and the updater path sets no
  quarantine xattr.
- macOS's `security import` cannot read OpenSSL 3's default PKCS#12 encryption — it fails with
  *"MAC verification failed during PKCS12 import (wrong password?)"*, which reads as a password bug
  and is not one. Export with `-certpbe PBE-SHA1-3DES -keypbe PBE-SHA1-3DES -macalg sha1`.
- **Never leave a second bundle with the same identifier on disk.** Keeping a rollback copy as
  `/tmp/Mulpex-0.6.0-rollback.app` produced an infinite prompt loop: two apps both claiming
  `com.mulpex.app` with different code identities, each Allow invalidating the other's row. `mv`
  also drags the Dock icon to the new path, so the old bundle is what a Dock click then launches.
  Deleting the duplicate is the fix; the prompt's *app name* is what identifies which one is asking.

## The DMG step is a Finder race — `release.sh` exports `CI=true`

`create-dmg` (which the tauri bundler ships as `bundle_dmg.sh`) mounts the image and then runs an
**AppleScript telling Finder to prettify the volume's window** — icon positions, window size, hidden
extension, hidden statusbar. It runs milliseconds after `hdiutil attach`, and if Finder has not yet
registered a window for that volume the property set fails:

```
Finder got an error: Can't set statusbar visible of container window
of disk "dmg.u9NTDy" to false. (-10006)
```

create-dmg treats **any** AppleScript failure as fatal (`Failed running AppleScript` → detach →
`exit 64`). So a release dies *after* the full compile — and tauri reports only
`error running bundle_dmg.sh`, swallowing the script's output entirely, which is why this looked
like a mystery rather than a race. The only mitigation in the script is a fixed `sleep 2`, added for
the sibling error `-1728` ("Can't get disk"), with **no retry**.

**Measured 2026-08-22:** 3 failures in 6 consecutive builds, including a `release.sh` run that
aborted having published nothing. Running the identical command by hand succeeded every time — the
difference is only timing. Orphaned `rw.*.dmg` scratch images dated 2026-08-18 and 08-19 (95 MB of
them) show earlier releases had been rolling the same dice and getting lucky. Getting the real error
out of it took a shim `bash` earlier in `PATH` that ran `bundle_dmg.sh` under `-x`, because tauri
captures the script's stdout and stderr and prints neither.

- **`export CI=true` in `scripts/release.sh`** makes the bundler pass `--skip-jenkins`, and that
  skips the AppleScript entirely. The step that fails no longer runs.
- **`true`/`false` only.** The tauri CLI parses `CI` as its own `--ci` flag, so `CI=1` fails the
  whole build with `invalid value '1' for '--ci'` — a stricter trap than the usual "any non-empty
  value" CI convention.
- **The cost is cosmetic and first-install-only.** The DMG keeps `Mulpex.app`, the `/Applications`
  symlink and the volume icon, but has no `.DS_Store`, so the window opens with a default layout
  instead of the app-beside-Applications arrangement. Updates never touch the DMG — they go through
  `Mulpex.app.tar.gz` (see **Auto-update** below). Verified by mounting the produced image.
- **Each failed run leaked a ~34 MB `rw.*.dmg`** into `target/release/bundle/macos/` and nothing
  ever removed them; `release.sh` now deletes them after the build.

## Auto-update

`tauri-plugin-updater` against the repo's GitHub releases. Checked at launch and every **6 h**
(`updater.ts::CHECK_INTERVAL_MS`), plus **Mulpex ▸ Check for Updates…** on demand; an available
version raises a fixed card (`UpdateBanner.svelte`) with **Update & Restart**.

- **The `.dmg` is not the update channel.** The updater consumes `Mulpex.app.tar.gz` + `.sig`
  (emitted by `bundle.createUpdaterArtifacts`), verifies the minisign signature against the
  `plugins.updater.pubkey` compiled into the app, and swaps the bundle in place. The DMG stays the
  first-install channel only. All four artifacts must land on the **same** GitHub release —
  `latest.json` is fetched from `/releases/latest/download/`, which resolves to the newest
  *published, non-prerelease* release, so a draft ships an update nobody can see.
- **`xattr -dr com.apple.quarantine` does not come back.** `com.apple.quarantine` is written by
  the *downloading* app (a browser, via LaunchServices); the updater fetches over the app's own
  HTTP client, so nothing sets the xattr and the extracted bundle inherits none. Gatekeeper's
  first-launch assessment only fires on quarantined bundles. One manual `xattr` on the first DMG
  install, never again. Ad-hoc signing (`Signature=adhoc`, `TeamIdentifier=not set`) stays fine
  for *this*: there is no cert continuity to break. It is **not** fine for TCC, which is a separate
  concern this bullet used to obscure — see **macOS file access** above. Through v0.5.0 `tauri
  build` produced no bundle signature at all, and the resulting invalid signature plus random
  per-build identifier is why folder permissions never persisted across updates.
- **Restart goes through `AppHandle::request_restart`** (`commands::restart_app`), not
  `plugin-process`'s `relaunch` and **not `AppHandle::restart`**. The restart has to fire
  `ExitRequested`/`Exit` on the way out, because that is what runs teardown — otherwise every
  update orphans process groups and leaks a scratch dir. `relaunch()` doesn't fire them at all,
  and `restart()` only *sometimes* does: its own docs say that called **on the main thread** it
  "cannot guarantee the delivery of those events, so we skip them" and re-execs immediately.
  Whether a command body runs on the main thread is Tauri's scheduling choice — measured today it
  is *not*, and `restart()` did fire the events and did tear down correctly, so this is a bug that
  would have stayed invisible until a runtime upgrade moved the thread. `request_restart` always
  routes through `request_exit(RESTART_EXIT_CODE)`. The leak fix below is a prerequisite for all
  of this, not a nicety.
- **Busy guard.** `updater.ts::busySessionCount()` counts `working` (mid-turn) and `needs`
  (stopped on a question) across **every open project**, not just the visible one; non-zero parks
  the banner in a `confirming` state naming the count. `waiting` sessions don't count — `--resume`
  restores those intact.
- **Automatic checks are silent on failure; manual ones aren't.** A laptop on flaky wifi must not
  accumulate error banners nobody asked for, but a menu-item check that silently did nothing would
  read as broken. Same function, `manual` flag.
- **The banner is NOT gated on `ready`** — don't "tidy" it back inside that block. `ready` flips
  only after `bootstrap()` has walked every open project and built + attached an xterm for every
  session, serially, so with several projects restoring, the launch check finishes early and the
  card would sit invisible for as long as bootstrap took. That is exactly what the first user
  report of "the banner only appears if I click Check for Updates" was: the check had worked
  fine. Measured against the real endpoint, with zero projects the banner is up ~6 s after
  launch. The card is fixed-position and owns nothing bootstrap provides.
- **Shipped in v0.4.7**, which by construction had to be installed by hand — it is the release
  that *adds* the updater, so nothing older could deliver it. v0.4.8 was a deliberate no-op
  release published to exercise the real path end to end.
- **Releasing:** `npm run release` (`scripts/release.sh`) — preflights the key, the
  tauri.conf.json/Cargo.toml version agreement, a clean tree, **an upstream with nothing
  unpushed**, and an unused tag; builds; writes `latest.json`; `gh release create`s all four
  artifacts. The unpushed check exists because `gh release create` is called with **no
  `--target`**, so GitHub creates the tag at the *remote* default branch's HEAD: with local
  commits unpushed, the tag names code the release does not contain — **silently**, since the
  uploaded artifacts are the correct local build, so the release works while its source tag
  points elsewhere. Not hypothetical: **the v0.6.0 tag sits on the v0.5.0 release commit** for
  exactly this reason. `--dry-run` builds and writes the JSON
  without publishing — and because it skips the clean-tree and unused-tag checks, it is also how
  you inspect a release build *before* committing. Use it: `release.sh` checks only that the
  artifacts **exist**, never that the `.app` inside them is signed, which is exactly how the
  unsigned bundles above shipped for five releases. Worth checking after publishing too — re-fetch
  the served tarball, compare its SHA-256 to the local one, and run `codesign --verify --deep
  --strict` on the `.app` inside it.
- **The signing-key gotcha, which costs a full release compile to rediscover:** `tauri signer
  generate` prints `TAURI_SIGNING_PRIVATE_KEY_PATH`, but the v2 bundler reads **only**
  `TAURI_SIGNING_PRIVATE_KEY` (contents or path). With just the `_PATH` form set, the build runs to
  completion, emits the `.tar.gz`, and *then* dies with "A public key has been found, but no
  private key". Both `release.sh` and the `tauri:build` npm script export the key contents. The key
  lives at `~/.mulpex/updater.key` (0600, no password) and is **not** in the repo — lose it and no
  existing install can ever accept another update; the only recovery is a new keypair plus a manual
  DMG reinstall by every user.
- **`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` must be exported too, even though the key has none.** The
  bundler always tries to decrypt, so with the variable unset it prompts — and on a non-interactive
  shell that read fails with `Device not configured (os error 6)`, dressed up as
  *"incorrect updater private key password"*, which reads as a wrong password and is not one.
  Same late-failure shape as the bullet above: the `.app` and `.dmg` are already written by then,
  so it looks like a finished build with a **stale** `Mulpex.app.tar.gz` beside it from a previous
  run. `release.sh` exports it (`:73`); `tauri:build` didn't until this was hit, and now does.

## Teardown fires on TWO RunEvents (the fixed scratch-root leak)

`lib.rs` matches **`RunEvent::ExitRequested | RunEvent::Exit`**, and dropping either arm
re-opens a measured bug. `ExitRequested` is only reachable through `app.exit()` — i.e. the
window-close arm and `AppHandle::restart()`. **⌘Q and an Apple-Event `quit` never touch it:**
they go to Cocoa's `NSApplication terminate:`, which tao turns into `applicationWillTerminate:` →
`AppState::exit()` → `Event::LoopDestroyed`, and tauri-runtime-wry maps *that* to
`RunEvent::Exit`. Matching only `ExitRequested` meant teardown never ran on the two quit paths a
human actually uses.

The old diagnosis in these notes inverted the evidence: it read "every `claude` was dead" as proof
the `killpg` half ran and only `remove_dir_all` failed. **Neither half ran.** The children died
from the PTY hangup when the process exited — which kills the foreground process group only, so
anything an instance had backgrounded was orphaned, exactly the case `killpg` exists for.

Measured before/after on an isolated `HOME` with one empty project (`scratchpad/measure-quit.sh`):
AppleScript quit and ⌘Q both **leaked** the whole `temp/mulpex-<pid>/` tree and both are now
**clean**; window-close was clean throughout; the Quit *menu item* is clean too. `teardown` is
idempotent, so window-close firing both events and running it twice is fine.

**Second layer: `Workspace::sweep_stale_state_roots()`**, run in `setup()` before the new root is
created. No shutdown hook can cover Force Quit, `kill -9`, a crash or a power loss, so each launch
also collects `temp/mulpex-<pid>` dirs whose pid is no longer alive (`libc::kill(pid, 0)`, with
`EPERM` counting as alive). It errs toward *keeping*: a recycled pid just defers that dir to a
later launch, whereas deleting a live Mulpex's root would break its running hub. This is what
finally collected the 12 dirs the old bug had accumulated on this machine.

