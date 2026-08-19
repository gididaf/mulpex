<script lang="ts">
  import { onMount, tick } from "svelte";
  import { get } from "svelte/store";
  import { listen } from "@tauri-apps/api/event";
  import { open as openFolder } from "@tauri-apps/plugin-dialog";
  import { getCurrentWebview } from "@tauri-apps/api/webview";
  import { getCurrentWindow } from "@tauri-apps/api/window";

  import {
    bootstrap,
    listRecentProjects,
    claudeStatus,
    openProject,
    closeProject,
    switchProject,
    reorderProjects,
    reorderSessions,
    createSession,
    createTerminal,
    closeSession,
    focusSession,
    getHubSnapshot,
    sendBytes,
    setSessionMuted,
    setMuteMenuChecked,
    type BootstrapInfo,
    type ClaudeStatus,
    type HubUpdateEvent,
    type SessionsChangedEvent,
    type SessionExitedEvent,
    type ProjectHandle,
  } from "./lib/ipc";
  import {
    projects,
    activeProjectHandle,
    project,
    sessions,
    activeId,
    showMessages,
    showPalette,
    rename,
    addProject,
    removeProject,
    setActiveProject,
    setActiveSession,
    setSessionsFor,
    setSessionMutedLocal,
    applyHubFor,
    displayOrder,
    flashNotice,
    reorderProjects as reorderProjectsLocal,
    reorderSessions as reorderSessionsLocal,
  } from "./lib/stores";
  import { findByDir } from "./lib/stores";
  import { terminals } from "./lib/terminals";
  import { initAttention } from "./lib/attention";

  import ProjectPicker from "./lib/components/ProjectPicker.svelte";
  import ProjectTabBar from "./lib/components/ProjectTabBar.svelte";
  import TopBar from "./lib/components/TopBar.svelte";
  import UpdateBanner from "./lib/components/UpdateBanner.svelte";
  import { checkForUpdate, startUpdateChecks } from "./lib/updater";
  import BottomBar from "./lib/components/BottomBar.svelte";
  import InstanceList from "./lib/components/InstanceList.svelte";
  import HubPanel from "./lib/components/HubPanel.svelte";
  import TerminalPane from "./lib/components/TerminalPane.svelte";
  import MessageReader from "./lib/components/MessageReader.svelte";
  import CommandPalette from "./lib/components/CommandPalette.svelte";
  import RenameDialog from "./lib/components/RenameDialog.svelte";

  let recents: string[] = $state([]);
  let ready = $state(false);
  let claude: ClaudeStatus | null = $state(null);
  let openError: string | null = $state(null);

  /** Build one project's UI state + xterms, then activate it if asked. */
  async function bootstrapProject(info: BootstrapInfo, makeActive: boolean) {
    addProject({
      handle: info.handle,
      dir: info.project_dir,
      name: info.project_name,
      sessions: info.sessions,
      statuses: new Map(),
      tasks: new Map(),
      hub: null,
      activeSessionId: info.sessions[info.active]?.id ?? null,
    });
    await tick(); // let TerminalView children mount + create their terminals
    const snap = await getHubSnapshot(info.handle);
    if (snap) applyHubFor(info.handle, snap);
    if (makeActive) selectProject(info.handle);
  }

  /** Switch which project is front-most: visible terminal, WebGL, backend active. */
  function selectProject(handle: ProjectHandle) {
    setActiveProject(handle);
    switchProject(handle);
    const p = get(projects).get(handle);
    const aid = p?.activeSessionId ?? null;
    if (aid != null) {
      focusSession(handle, aid);
      terminals.focus(handle, aid);
    } else {
      // Project with no sessions: hide every terminal (no key matches).
      terminals.focus(handle, -1);
    }
  }

  /** Focus a session within the active project. */
  /**
   * Commit a dragged tab order: reorder locally for an immediate repaint, then
   * tell the backend, which persists it to `open.txt` so it survives relaunch.
   *
   * Tab order is also what ⌘1–⌘9 index into (see `handleMenu`), so a drag
   * remaps those shortcuts too — intentionally, since they're documented as
   * "the Nth open project, in tab-bar order".
   */
  function applyProjectOrder(order: ProjectHandle[]) {
    reorderProjectsLocal(order);
    reorderProjects(order);
  }

  /**
   * Commit a dragged sidebar order: reorder locally for an immediate repaint,
   * then tell the backend, which rewrites the session store so the arrangement
   * survives relaunch (and echoes the same order back on the next poll).
   *
   * Sidebar order is also what ⌘[ / ⌘] cycle, so a drag remaps those too — the
   * "what you see is what you cycle" rule the muted-sinking sort already follows.
   */
  function applySessionOrder(ids: number[]) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    reorderSessionsLocal(h, ids);
    reorderSessions(h, ids);
  }

  function selectSession(id: number) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    setActiveSession(h, id);
    focusSession(h, id);
    terminals.focus(h, id);
  }

  /**
   * Mute or unmute one session of the active project (⌘M, or the row's 🔇).
   *
   * Mute never moves focus: the muted session's terminal stays visible and
   * typeable, its row just dims and sinks. Muting is a statement about how loudly
   * the sidebar should talk about an instance, not about whether you're done
   * with it.
   */
  function muteSession(id: number, muted: boolean) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    setSessionMutedLocal(h, id, muted);
    setSessionMuted(h, id, muted); // persists; fire-and-forget
    syncMuteMenu();
  }

  /** Push the focused session's muted state into the menu's check item — the
   *  native menu has no view of which session is active.
   *
   *  Deduped: the trigger below re-runs on every hub poll that changes anything
   *  (statuses churn at 200 ms), and the tick almost never moves with it.
   *
   *  `force` defeats the dedup for the one case where the item's real state and
   *  our last-pushed value have diverged: muda ticks a check item *itself* before
   *  firing the event, so a click that resolves to "no session to mute" leaves a
   *  tick we never wrote and would otherwise never clear. */
  let lastMuteChecked: boolean | null = null;
  function syncMuteMenu(force = false) {
    const cur = get(activeId);
    const s = cur == null ? undefined : get(sessions).find((x) => x.id === cur);
    const checked = s?.muted ?? false;
    if (checked === lastMuteChecked && !force) return;
    lastMuteChecked = checked;
    setMuteMenuChecked(checked);
  }

  // Focus and the flag both move the tick, and both land here as a store change.
  $effect(() => {
    void $activeId;
    void $sessions;
    syncMuteMenu();
  });

  /** Open a project by path, or focus it if already open (picker/+/palette/drop). */
  async function openOrFocusProject(path: string) {
    const existing = findByDir(path);
    if (existing) {
      selectProject(existing.handle);
      return;
    }
    try {
      openError = null;
      const info = await openProject(path);
      await bootstrapProject(info, true);
    } catch (e) {
      // Surfaced in the picker: this is how a missing `claude` used to present
      // itself as the app silently ignoring the click.
      openError = e instanceof Error ? e.message : String(e);
      console.error("open project failed:", e);
    }
  }

  // Wire form of a dropped path: backslash-escape everything a shell would act
  // on. This is what Terminal.app/iTerm insert when you drag a file in, so the
  // prompt reads `/Users/me/My\ File.csv` — matching stock Claude Code — rather
  // than a quote-wrapped path. Characters ≥ U+0080 are left bare; real
  // terminals don't escape unicode and a backslash there just reads as noise.
  //
  // Control characters take the other branch: they have no printable backslash
  // form, and a literal \n or \r on the wire would submit the message halfway
  // through the path. ANSI-C `$'…'` keeps the bytes CR/LF-free while still
  // round-tripping to the real name.
  function escapePath(path: string): string {
    if (!/[\x00-\x1f\x7f]/.test(path))
      return path.replace(/[^\w@%+:,./=-]/g, (c) =>
        c.charCodeAt(0) < 0x80 ? "\\" + c : c,
      );
    const esc = path
      .replace(/\\/g, "\\\\")
      .replace(/'/g, "\\'")
      .replace(/\n/g, "\\n")
      .replace(/\r/g, "\\r")
      .replace(/\t/g, "\\t")
      .replace(/[\x00-\x1f\x7f]/g, (c) =>
        "\\" + c.charCodeAt(0).toString(8).padStart(3, "0"),
      );
    return `$'${esc}'`;
  }

  /**
   * A drop puts the absolute path(s) at the focused session's prompt — files
   * and folders alike, since a folder is a legitimate argument to hand Claude.
   *
   * The paths go over as a single **bracketed paste** (`ESC[200~ … ESC[201~`),
   * not as typed keystrokes, because that is the channel Claude Code inspects
   * for attachments: a pasted image path becomes an `[Image #N]` attachment it
   * can actually see, while a non-image path stays as text. Typed keystrokes
   * get no such treatment — measured, that was the whole reason dropping a
   * screenshot only ever produced a path string. One paste holding every
   * dropped path is the right shape: Claude extracts each image and leaves the
   * rest as text.
   *
   * The trailing space goes *inside* the paste, which looks wrong and isn't:
   * a space typed after `ESC[201~` **wipes the prompt** for any non-image path
   * (measured — the path renders, then Claude erases the line; presumably the
   * keystroke races its async paste handling). Inside the markers both text
   * paths and image attachments survive.
   *
   * With no session to type into (the picker, or a project sitting at zero
   * sessions) a drop falls back to opening the path as a project; the backend
   * rejects non-directories.
   */
  function dropPaths(paths: string[]) {
    if (!paths.length) return;
    const h = get(activeProjectHandle);
    const id = h == null ? null : (get(projects).get(h)?.activeSessionId ?? null);
    if (h == null || id == null) {
      for (const path of paths) openOrFocusProject(path);
      return;
    }
    const body = paths.map(escapePath).join(" ");
    // One write: split across reads, Claude can consume a lone ESC and lose
    // the paste framing (same hazard as the Shift+Enter carve-out).
    sendBytes(h, id, new TextEncoder().encode(`\x1b[200~${body} \x1b[201~`));
    terminals.refocus();
  }

  async function pickAndOpen() {
    const dir = await openFolder({ directory: true, title: "Open Project" });
    if (typeof dir === "string") await openOrFocusProject(dir);
  }

  async function closeProjectHandle(handle: ProjectHandle) {
    await closeProject(handle);
    terminals.disposeProject(handle);
    removeProject(handle); // re-picks the active handle (neighbor / null)
    const next = get(activeProjectHandle);
    if (next != null) selectProject(next);
    else recents = await listRecentProjects(); // none left → picker
  }

  async function newSession() {
    const h = get(activeProjectHandle);
    if (h == null) return;
    let info;
    try {
      info = await createSession(h);
    } catch (e) {
      // Spawning can be refused before it starts — most often because macOS is
      // withholding the project's folder from this app, which the backend
      // checks for by name. Without this the rejection was unhandled and ⌘T
      // simply did nothing at all, which is the failure it exists to explain.
      flashNotice(`Could not start Claude: ${e}`, 8000);
      return;
    }
    const p = get(projects).get(h);
    setSessionsFor(h, [...(p?.sessions ?? []), info]);
    await tick();
    selectSession(info.id);
  }

  /** ⌘⇧T — open a plain shell terminal and focus it. (A terminal an *instance*
   *  opens arrives via `sessions-changed` instead, and never steals focus.) */
  async function newTerminal() {
    const h = get(activeProjectHandle);
    if (h == null) return;
    let info;
    try {
      info = await createTerminal(h);
    } catch (e) {
      // The picker's error line only renders when no project is open, so a
      // failure here (no $SHELL, spawn refused) would otherwise be invisible.
      flashNotice(`Could not open a terminal: ${e}`, 4000);
      return;
    }
    const p = get(projects).get(h);
    setSessionsFor(h, [...(p?.sessions ?? []), info]);
    await tick();
    selectSession(info.id);
  }

  /** Cycle sessions within the active project, in the order the sidebar shows
   *  them — muted ones last. What you see is what you cycle. */
  function cycle(delta: number) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    const list = displayOrder(get(projects).get(h)?.sessions ?? []);
    if (list.length === 0) return;
    const cur = get(activeId);
    const idx = list.findIndex((s) => s.id === cur);
    const next = list[(idx + delta + list.length) % list.length];
    if (next) selectSession(next.id);
  }

  /** Cycle between open projects (⌘⇧[ / ⌘⇧]). */
  function cycleProject(delta: number) {
    const keys = [...get(projects).keys()];
    if (keys.length < 2) return;
    const cur = get(activeProjectHandle);
    const idx = keys.findIndex((h) => h === cur);
    const next = keys[(idx + delta + keys.length) % keys.length];
    if (next != null) selectProject(next);
  }

  async function handleMenu(id: string) {
    const h = get(activeProjectHandle);
    switch (id) {
      case "open_project":
        await pickAndOpen();
        break;
      case "close_project":
        if (h != null) await closeProjectHandle(h);
        break;
      case "next_project":
        cycleProject(1);
        break;
      case "prev_project":
        cycleProject(-1);
        break;
      case "new_session":
        if (h != null) await newSession();
        break;
      case "new_terminal":
        if (h != null) await newTerminal();
        break;
      case "close_session": {
        const cur = get(activeId);
        if (h != null && cur != null) closeSession(h, cur);
        break;
      }
      case "rename": {
        const cur = get(activeId);
        if (h != null && cur != null) {
          const s = get(sessions).find((x) => x.id === cur);
          rename.set({ handle: h, id: cur, value: s?.name ?? "" });
        }
        break;
      }
      case "mute": {
        const cur = get(activeId);
        if (h != null && cur != null) {
          const s = get(sessions).find((x) => x.id === cur);
          muteSession(cur, !s?.muted);
        } else {
          // Nothing focused: the check item already toggled itself on click, so
          // put it back rather than leaving a tick with nothing behind it.
          syncMuteMenu(true);
        }
        break;
      }
      case "messages":
        showMessages.update((v) => !v);
        break;
      case "minimize":
        // Custom item (muda hard-binds the predefined one to ⌘M, which is Mute).
        await getCurrentWindow().minimize();
        break;
      case "check_updates":
        // Manual: reports "up to date" and network errors too, unlike the
        // periodic check which stays silent on both.
        await checkForUpdate(true);
        break;
      case "next":
        cycle(1);
        break;
      case "prev":
        cycle(-1);
        break;
      default:
        // ⌘1–⌘9 → the Nth open project, in tab-bar order (the projects Map's
        // insertion order, which is what ProjectTabBar renders).
        if (id.startsWith("project_")) {
          const n = parseInt(id.slice("project_".length), 10);
          const h2 = [...get(projects).keys()][n - 1];
          if (h2 != null && h2 !== h) selectProject(h2);
        }
    }
  }

  /**
   * Commands chosen in ⌘P run through `handleMenu` — the same string-keyed switch
   * the native menu feeds — so the palette owns no second implementation of any
   * command and cannot drift from the menu.
   *
   * Closing the overlay drops keyboard focus, and most commands don't take it
   * anywhere, so the terminal has to be handed it back. The exceptions are the
   * commands whose whole job is to put focus somewhere else: `rename` and
   * `messages` open their own focus-taking UI, and `open_project` opens a native
   * folder dialog. Refocusing the terminal after those would fight them.
   */
  const PALETTE_KEEPS_FOCUS = new Set(["rename", "messages", "open_project"]);
  async function runPaletteAction(id: string) {
    showPalette.set(false);
    await handleMenu(id);
    if (!PALETTE_KEEPS_FOCUS.has(id)) terminals.refocus();
  }

  function onGlobalKey(e: KeyboardEvent) {
    // ⌘P / Ctrl+P toggles the command palette (projects, sessions, commands). Not a
    // menu accelerator, so it reaches the webview; preventDefault stops the print
    // dialog.
    if ((e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === "p") {
      e.preventDefault();
      if (get(projects).size > 0) showPalette.update((v) => !v);
    }
  }

  onMount(() => {
    const unlisteners: Array<Promise<() => void>> = [];

    unlisteners.push(
      listen<string>("menu", (e) => handleMenu(e.payload)),
      listen<HubUpdateEvent>("hub-update", (e) =>
        applyHubFor(e.payload.handle, e.payload.snapshot),
      ),
      listen<SessionExitedEvent>("session-exited", (e) =>
        terminals.dispose(e.payload.handle, e.payload.id),
      ),
      listen<SessionsChangedEvent>("sessions-changed", async (e) => {
        const { handle, sessions: list } = e.payload;
        setSessionsFor(handle, list);
        const cur = get(projects).get(handle)?.activeSessionId ?? null;
        if (cur == null || !list.some((s) => s.id === cur)) {
          const first = list[0]?.id ?? null;
          setActiveSession(handle, first);
          if (handle === get(activeProjectHandle)) {
            if (first != null) selectSession(first);
            else terminals.focus(handle, -1);
          }
        }
        // New xterms (e.g. hub_spawn children) mount hidden; refit brings them —
        // and their PTYs, spawned at the default size — to the shared geometry,
        // even for a background project.
        await tick();
        terminals.refit();
      }),
      // Drop files/folders onto the window to type their paths at the prompt.
      // Tauri's webview-level drag-drop (`dragDropEnabled`, on by default)
      // swallows the native drop before the DOM sees it, so xterm never gets a
      // `drop` event and this handler is the only place a drop can be honoured.
      getCurrentWebview().onDragDropEvent((ev) => {
        if (ev.payload.type === "drop") dropPaths(ev.payload.paths);
      }),
    );

    const onResize = () => terminals.refit();
    window.addEventListener("resize", onResize);

    // Check at startup, then every 6h. Silent unless something is actually
    // available — a failed check never surfaces on this path.
    const stopUpdateChecks = startUpdateChecks();

    // Dock badge + "a claude needs you" notifications. Clicking a banner routes
    // through the same select path as clicking the sidebar row, so it lands on
    // the pane with the question rather than merely raising the window.
    let stopAttention: (() => void) | null = null;
    initAttention((handle, id) => {
      selectProject(handle);
      setActiveSession(handle, id);
      focusSession(handle, id);
      terminals.focus(handle, id);
    }).then((stop) => {
      stopAttention = stop;
    });

    (async () => {
      const ws = await bootstrap();
      // BEFORE any TerminalView mounts. Restored sessions' PTYs have been running
      // since before the window painted, and `attach_session` flushes everything
      // they printed — so each xterm has to be built at the size those bytes were
      // rendered for. Building at xterm's 80x24 default and resizing afterwards
      // corrupts the pane permanently; see `terminals.ts::create`.
      terminals.setGeometry(ws.cols, ws.rows);
      if (ws.projects.length) {
        for (const info of ws.projects) {
          await bootstrapProject(info, info.handle === ws.active);
        }
      } else {
        recents = await listRecentProjects();
      }
      ready = true;
      // Non-blocking: the probe shells out to the login shell, so let the UI
      // paint first. Only matters for the picker's banner.
      claude = await claudeStatus();
    })();

    return () => {
      window.removeEventListener("resize", onResize);
      stopUpdateChecks();
      stopAttention?.();
      unlisteners.forEach((p) => p.then((f) => f()));
    };
  });
</script>

<svelte:window onkeydown={onGlobalKey} />

{#if ready && $project}
  <div class="shell">
    <ProjectTabBar
      onselect={selectProject}
      onclose={closeProjectHandle}
      onadd={pickAndOpen}
      onreorder={applyProjectOrder}
    />
    <TopBar />
    <aside class="sidebar">
      <InstanceList
        onselect={selectSession}
        onmute={muteSession}
        onreorder={applySessionOrder}
      />
      <HubPanel />
    </aside>
    <main class="pane">
      <TerminalPane />
    </main>
    <BottomBar />
  </div>
  {#if $showPalette}
    <CommandPalette
      onproject={(h) => {
        showPalette.set(false);
        selectProject(h);
      }}
      onsession={(id) => {
        showPalette.set(false);
        selectSession(id);
      }}
      onaction={runPaletteAction}
      onclose={() => showPalette.set(false)}
    />
  {/if}
  {#if $showMessages}
    <MessageReader onclose={() => showMessages.set(false)} />
  {/if}
  {#if $rename}
    <RenameDialog />
  {/if}
{:else if ready}
  <ProjectPicker
    {recents}
    {claude}
    error={openError}
    onpick={pickAndOpen}
    onopen={openOrFocusProject}
  />
{/if}

<!-- Outside both branches AND ungated by `ready`: an update is equally relevant
     with a project open and sitting on the picker, and the card is fixed-position
     so it needs no slot in either layout.

     `ready` deliberately does NOT gate this. It flips only after bootstrap has
     walked every open project and built + attached an xterm for every session,
     serially — so with several projects restoring, the startup check would finish
     early and the banner still sit invisible for as long as that took. Measured:
     with zero projects the banner is up ~6s after launch; with three projects it
     was late enough to look like the launch check had simply not run. The card
     owns nothing that bootstrap provides, so it has no reason to wait for it. -->
<UpdateBanner />

<style>
  .shell {
    display: grid;
    grid-template-columns: var(--sidebar-w) 1fr;
    grid-template-rows: auto auto 1fr auto;
    grid-template-areas:
      "tabs tabs"
      "top top"
      "side pane"
      "bottom bottom";
    height: 100%;
  }
  .sidebar {
    grid-area: side;
    display: grid;
    /* The hub panel is half its old share (55% → 28%): messages are reference
       material, the session list is what you steer with. */
    grid-template-rows: 72% 28%;
    min-height: 0;
    background: var(--bg-sidebar);
    border-right: 1px solid var(--border);
  }
  /* Both rows scroll independently. Without min-height:0 a grid item's auto
     minimum lets it grow past its track instead of overflowing, and the
     `overflow-y: auto` inside InstanceList/HubPanel never engages. */
  .sidebar > :global(*) {
    min-height: 0;
  }
  .pane {
    grid-area: pane;
    min-width: 0;
    min-height: 0;
    background: var(--bg);
  }
</style>
