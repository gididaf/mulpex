<script lang="ts">
  import { onMount, tick } from "svelte";
  import { get } from "svelte/store";
  import { listen } from "@tauri-apps/api/event";
  import { open as openFolder } from "@tauri-apps/plugin-dialog";
  import { getCurrentWebview } from "@tauri-apps/api/webview";

  import {
    bootstrap,
    listRecentProjects,
    claudeStatus,
    openProject,
    closeProject,
    switchProject,
    createSession,
    closeSession,
    focusSession,
    getHubSnapshot,
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
    applyHubFor,
  } from "./lib/stores";
  import { findByDir } from "./lib/stores";
  import { terminals } from "./lib/terminals";

  import ProjectPicker from "./lib/components/ProjectPicker.svelte";
  import ProjectTabBar from "./lib/components/ProjectTabBar.svelte";
  import TopBar from "./lib/components/TopBar.svelte";
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
  function selectSession(id: number) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    setActiveSession(h, id);
    focusSession(h, id);
    terminals.focus(h, id);
  }

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
    const info = await createSession(h);
    const p = get(projects).get(h);
    setSessionsFor(h, [...(p?.sessions ?? []), info]);
    await tick();
    selectSession(info.id);
  }

  /** Cycle sessions within the active project. */
  function cycle(delta: number) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    const list = get(projects).get(h)?.sessions ?? [];
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
      case "messages":
        showMessages.update((v) => !v);
        break;
      case "next":
        cycle(1);
        break;
      case "prev":
        cycle(-1);
        break;
      default:
        if (id.startsWith("focus_")) {
          const n = parseInt(id.slice("focus_".length), 10);
          const s = get(sessions)[n - 1];
          if (s) selectSession(s.id);
        }
    }
  }

  function onGlobalKey(e: KeyboardEvent) {
    // ⌘P / Ctrl+P toggles the project quick-switcher. Not a menu accelerator, so it
    // reaches the webview; preventDefault stops the browser print dialog.
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
      // Drag a folder onto the window to open it as a project.
      getCurrentWebview().onDragDropEvent((ev) => {
        if (ev.payload.type === "drop") {
          for (const path of ev.payload.paths) openOrFocusProject(path);
        }
      }),
    );

    const onResize = () => terminals.refit();
    window.addEventListener("resize", onResize);

    (async () => {
      const ws = await bootstrap();
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
    />
    <TopBar />
    <aside class="sidebar">
      <InstanceList onselect={selectSession} />
      <HubPanel />
    </aside>
    <main class="pane">
      <TerminalPane />
    </main>
    <BottomBar />
  </div>
  {#if $showPalette}
    <CommandPalette
      onselect={(h) => {
        showPalette.set(false);
        selectProject(h);
      }}
      onopennew={() => {
        showPalette.set(false);
        pickAndOpen();
      }}
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
    grid-template-rows: 45% 55%;
    min-height: 0;
    background: var(--bg-sidebar);
    border-right: 1px solid var(--border);
  }
  .pane {
    grid-area: pane;
    min-width: 0;
    min-height: 0;
    background: var(--bg);
  }
</style>
