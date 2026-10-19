# Linux (x86_64, Ubuntu 24.04 / XFCE)

The Linux port, added 2026-10-12. macOS is still the reference platform: every
Linux difference is behind `#[cfg(target_os = "linux")]` (Rust) or `IS_LINUX`
(`src/lib/platform.ts`), so the Mac build does exactly what it did before.

Back to [CLAUDE.md](../CLAUDE.md).

## Decisions (settled with the user, 2026-10-12)

| Topic | Choice |
| --- | --- |
| ⌘ on Linux | **Super**. The Mac's ⌘ arrives as Super over RDP, so every shortcut is the same key. XFCE's own Super bindings must be off on the machine. |
| Copy / paste | **Super+C / Super+V / Super+A / Super+X.** Ctrl+C is never touched — it stays the terminal's interrupt. No Ctrl+Shift+C/V, no middle-click paste. |
| Menu | **An in-window menu bar** (`MenuBar.svelte`, since 2026-10-15), drawn by the webview in the app's theme. No native/GTK menu. The webview claims every shortcut itself; keyless commands (Check for Updates, Import Docs) are in the menu and the ⌘P palette. |
| Dock badge | **Window title**: `(3) Mulpex`, which the XFCE taskbar shows. |
| Packaging | **AppImage + .deb**, both self-updating, signed on the Mac (`scripts/release-linux.sh`). |

## The menu bar (`MenuBar.svelte`)

Added 2026-10-15, because the user drives the VM over Remote Desktop from the Mac
and **⌘ did not reach the VM as Super there**: with no menu, nothing was
reachable at all, not even Check for Updates.

- A fixed 26 px strip (`--menubar-h`), shown only when `IS_LINUX`. `main.ts` adds
  `html.has-menubar` **before mount**, which pads `#app`, so the first layout
  (and the terminal geometry measured from it) already leaves room. Cards pinned
  near the top add `--menubar-offset`.
- It holds the Mac's menus (Mulpex, File, Edit, Session, Window) with the same ids,
  run through the same `handleMenu`. **A third table to keep in step** with
  `menu.rs` and `linuxKeys.ts`. Mac-only items (About, Services, Hide, Full Disk
  Access, Undo/Redo) are left out. File lists the open projects by name (⌘1–⌘9).
  Mute shows a ✓ for the focused row.
- Labels use ⌘ symbols, since the user works from a Mac.
- Mouse only, like a desktop menu bar: click a title, hover switches menus while
  one is open, Esc / a click outside / losing window focus closes it. Alt belongs
  to the terminal, so there is no Alt navigation.
- Every title and item cancels `mousedown`, so focus never leaves the terminal or
  field: Edit's Copy/Paste/Select All act on it through `linuxKeys.ts::editAction`,
  the Super+C/V/A code.
- Measured on the VM (Xvfb, a real build): every menu opens and hover-switches,
  New Terminal and Mute run, the ✓ follows, Esc closes, Copy/Paste/Select All work
  in a terminal.
- **Known, older than the menu:** pasting a whole Select-All screen into a shell
  terminal froze that xterm's *drawing*, though the shell got every byte (the
  `terminals/<id>.log` transcript and the debug dump both had the later output).
  A 5-line paste and a one-liner were fine. It is the same `terminals.pasteActive`
  path Super+V uses. Not investigated further.

## Keys: the webview is the menu

`menu.rs` is not installed on Linux (`lib.rs`). `src/lib/linuxKeys.ts` is the
whole menu there: a table from `Super(+Shift)+<code>` to the same menu ids, fed
into the same `App.svelte::handleMenu`. **A shortcut added to `menu.rs` must be
added to that table too**, or it silently does nothing on Linux.

- It listens in the **capture** phase: xterm's keydown handler is on its own
  textarea, and a bubbling window listener would run after xterm had already
  encoded the key for the PTY.
- `terminals.ts` also gives every xterm (shells too, on Linux) a custom key
  handler that refuses Super chords, so an *unclaimed* one (Super+J) is not typed
  either.
- **WebKitGTK puts Super on no modifier flag** (measured, WebKitGTK 2.52 on
  X11): with Super held, a `t` keydown has `metaKey` and every
  `getModifierState` false. The only trace is Super's own keydown/keyup
  (`key: "Super"`, `code: "OSLeft"`), so `platform.ts` tracks it as held, and
  clears it on window blur so a keyup lost to the WM cannot leave it stuck.
- Alt does not set `metaKey` there (also measured); Alt still excludes a chord.
- Ctrl+P does **not** open the palette on Linux (it does on the Mac): it is the
  shell's previous-history key. Super+P does.
- Super+Q quits through `quit_app` (`app.exit(0)` → `ExitRequested` → teardown).
- Clipboard: `tauri-plugin-clipboard-manager`, Linux-only in `Cargo.toml` and in
  `capabilities/linux.json` (`"platforms": ["linux"]`). Paste into an xterm is
  `term.paste`, so bracketed paste still applies.

## Processes: `/proc` instead of libproc

`pty.rs`'s tty sweep, child-group kill, shell foreground/cwd and orphaned-listener
reaper all run on Linux, from `/proc/<pid>/stat`, `/cwd` and `/cmdline`. The
parsers (`parse_proc_stat`, `cmdline_text`, `parent_means_orphaned`) are pure and
unit-tested on every platform; only the file reads are Linux-gated.

- `comm` in `stat` can hold spaces and `)`, so the parser anchors on the LAST `)`.
- A shell with no controlling terminal has `tpgid == -1`, not 0.
- **Orphans are not always pid 1's.** Linux reparents to the nearest child
  subreaper, which on a desktop is the user's `systemd --user`. So an orphan is
  `ppid == 1` *or* a parent whose comm is `systemd`. A live listener's parent is
  its `claude`, so it can never match.

## Other differences

- `$SHELL` fallback is `/bin/bash` (`claude_bin::DEFAULT_SHELL`), not zsh.
- Remote Control keep-awake: `systemd-inhibit --what=idle:sleep … tail -f /dev/null --pid=<mulpex>`.
  **polkit may refuse it**: from a session logind does not count as active (measured
  over ssh: `Failed to inhibit: Access denied`, and an xrdp session may be the same),
  the inhibitor exits at once and nothing keeps the machine awake. On a VM, which
  never idle-sleeps, that costs nothing; a laptop would need a polkit rule.
- **Notification clicks do nothing on Linux.** The plugin's `onAction` never settles
  there (measured: `initAttention` stopped at it), and awaiting it held up the badge
  and every banner, so `attention.ts` skips it on Linux. Banners still appear.
  The same desktop plugin code runs on macOS — whether the Mac's banners and dock
  badge actually start is unverified (`verification-log.md` lists them as never
  re-driven); macOS was deliberately left unchanged.
- Full Disk Access is macOS-only: `has_full_disk_access` reports true, so the banner never shows.
- `dir_access_error` says "Permission denied … `ls -ld`", not "System Settings ▸ Privacy".
- Fonts: `DejaVu Sans Mono` / `Noto Sans Mono` and `Noto Sans` / `Noto Sans Hebrew`
  are appended to the font stacks, after every Mac font, so the Mac resolves as before.
- Hebrew in a pane comes from DejaVu Sans Mono, a *monospace* Hebrew, so narrow
  letters (י, ו) sit in full-width cells and words look letter-spaced. Order and
  direction are correct (measured on a real claude reply, WebKitGTK 2.52).

## Building

Ubuntu 24.04 packages:

```
build-essential curl file pkg-config libwebkit2gtk-4.1-dev libssl-dev
libayatana-appindicator3-dev librsvg2-dev libxdo-dev libnotify-bin
fonts-noto-core zsh
```

(`zsh` only because `state.rs`'s login-env test drives a real zsh.) A machine with
XFCE also needs its own `<Super>` bindings removed (`Super+P/E/R/Tab`, tiling) —
an XFCE shortcut is grabbed before the webview ever sees the key. Then, as the
desktop user: rustup, Node via nvm, and in the repo `bash scripts/bundle-helper.sh`,
`npm install`, `npm run tauri build -- --bundles deb,appimage`. The helper
sidecar's triple is `x86_64-unknown-linux-gnu`; `bundle-helper.sh` already takes
it from `rustc`. `src-tauri/tauri.linux.conf.json` is merged over
`tauri.conf.json` on Linux only (targets, PNG icons, deb depends).

## What was measured (2026-10-12, Ubuntu 24.04 VM, Xvfb + xdotool, real `claude` 2.1.296)

- `cargo test --workspace` passes on Linux, including the `/proc`-backed process
  tests (foreground job + cwd of a real shell, background-job kill, child-group
  kill, orphaned-listener selection) and the live tests
  `a_spawned_child_receives_its_whole_task_on_its_command_line` and
  `the_store_follows_a_transcript_that_moves_under_a_live_claude`.
- Launched with a bare desktop environment (`env -i … PATH=/usr/bin:/bin`): Super+T
  spawns a working claude (found through the login-shell probe; nvm's `claude`),
  hooks report status and context %, the plugin-monitor hub listener runs, MCP tools
  work (`hub_instances`, `hub_set_name`), a quit + relaunch restores it with `--resume`.
- Super+Shift+T (shell), Super+A / C / V (xterm buffer ↔ CLIPBOARD, bracketed paste),
  Super+P (palette), Super+[ (cycle), Super+Q (quit).
- Super+Q teardown: no `claude`, no listener, a backgrounded `sleep 300 &` in a shell
  killed, scratch root removed.
- Title badge: `Mulpex` → `(1) Mulpex` while a claude asks, back after.
- The AppImage runs, and its helper is served from the AppImage mount
  (`/tmp/.mount_*/usr/bin/mulpex-helper`). **Since the PTY daemon (docs/ptyd.md) both the helper
  and `mulpex-ptyd` are copied out of the mount** (`<home>/bin/`, `<home>/ptyd/<ver>/`) and run from
  there: the mount vanishes when the app quits, and claudes now outlive it. `scripts/test-ptyd.sh`
  passes on the VM under Xvfb (`VISIBLE=1` — a hidden window measures 2x1 cells there).

**Not measured:** anything over real RDP — that Mac ⌘ arrives as Super through
xrdp, notifications on the XFCE desktop, the updater installing an update, and
keep-awake inside an xrdp session.

## Releasing

`scripts/release-linux.sh --host <user@linux-box>`, on the Mac. `release.sh` runs it
itself while the release is still a draft, and publishes only after it succeeds (see
docs/remote-hosts.md, "Updates"). It builds `git archive HEAD` on the Linux host, copies the
`.AppImage` and `.deb` back, signs both with `~/.mulpex/updater.key` (the key
never leaves the Mac), uploads them to the same release, and adds
`linux-x86_64` (AppImage) and `linux-x86_64-deb` to `latest.json`, uploaded last.
**A host driven from a Mac must run the AppImage.** The Mac updates it remotely,
and the `.deb` path needs a root password dialog on the Linux screen.
The deb key matters: on a deb install the updater looks up
`linux-x86_64-deb` *first* and would otherwise fall back to the AppImage entry
and hand an AppImage to dpkg.
