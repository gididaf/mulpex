<script lang="ts">
  import { onMount, tick } from "svelte";
  import { get } from "svelte/store";
  import { listen } from "@tauri-apps/api/event";
  import { open as openFolder } from "@tauri-apps/plugin-dialog";

  import {
    currentProject,
    listRecentProjects,
    openProject,
    createSession,
    closeSession,
    focusSession,
    getHubSnapshot,
    type BootstrapInfo,
    type HubSnapshot,
    type SessionInfo,
  } from "./lib/ipc";
  import {
    project,
    sessions,
    statuses,
    tasks,
    hub,
    activeId,
    showMessages,
    rename,
  } from "./lib/stores";
  import { terminals } from "./lib/terminals";

  import ProjectPicker from "./lib/components/ProjectPicker.svelte";
  import TopBar from "./lib/components/TopBar.svelte";
  import BottomBar from "./lib/components/BottomBar.svelte";
  import InstanceList from "./lib/components/InstanceList.svelte";
  import HubPanel from "./lib/components/HubPanel.svelte";
  import TerminalPane from "./lib/components/TerminalPane.svelte";
  import MessageReader from "./lib/components/MessageReader.svelte";
  import RenameDialog from "./lib/components/RenameDialog.svelte";

  let recents: string[] = $state([]);
  let ready = $state(false);

  function applyHub(snap: HubSnapshot) {
    hub.set(snap);
    statuses.set(new Map(snap.statuses.map((e) => [e.id, e.status])));
    tasks.set(new Map(snap.tasks.map((e) => [e.id, e.task])));
  }

  async function bootstrap(info: BootstrapInfo) {
    project.set(info);
    sessions.set(info.sessions);
    activeId.set(info.sessions[info.active]?.id ?? null);
    await tick(); // let TerminalView children mount + create their terminals
    const aid = get(activeId);
    if (aid != null) terminals.focus(aid);
    const snap = await getHubSnapshot();
    if (snap) applyHub(snap);
  }

  /** Focus a session: reactive state, backend order, and the xterm itself. */
  function selectSession(id: number) {
    activeId.set(id);
    focusSession(id);
    terminals.focus(id);
  }

  async function pickAndOpen() {
    const dir = await openFolder({ directory: true, title: "Open Project" });
    if (typeof dir === "string") {
      const info = await openProject(dir);
      await bootstrap(info);
    }
  }

  async function openRecent(path: string) {
    const info = await openProject(path);
    await bootstrap(info);
  }

  async function newSession() {
    const info = await createSession();
    sessions.update((l) => [...l, info]);
    await tick();
    selectSession(info.id);
  }

  function cycle(delta: number) {
    const list = get(sessions);
    if (list.length === 0) return;
    const cur = get(activeId);
    const idx = list.findIndex((s) => s.id === cur);
    const next = list[(idx + delta + list.length) % list.length];
    if (next) selectSession(next.id);
  }

  async function handleMenu(id: string) {
    switch (id) {
      case "open_project":
        // One project per window: only meaningful before one is open.
        if (!get(project)) await pickAndOpen();
        break;
      case "new_session":
        if (get(project)) await newSession();
        break;
      case "close_session": {
        const cur = get(activeId);
        if (cur != null) closeSession(cur);
        break;
      }
      case "rename": {
        const cur = get(activeId);
        if (cur != null) {
          const s = get(sessions).find((x) => x.id === cur);
          rename.set({ id: cur, value: s?.name ?? "" });
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

  onMount(() => {
    const unlisteners: Array<Promise<() => void>> = [];

    unlisteners.push(
      listen<string>("menu", (e) => handleMenu(e.payload)),
      listen<HubSnapshot>("hub-update", (e) => applyHub(e.payload)),
      listen<number>("session-exited", (e) => terminals.dispose(e.payload)),
      listen<SessionInfo[]>("sessions-changed", (e) => {
        sessions.set(e.payload);
        const cur = get(activeId);
        if (!e.payload.some((s) => s.id === cur)) {
          const first = e.payload[0]?.id ?? null;
          activeId.set(first);
          if (first != null) selectSession(first);
        }
      }),
    );

    const onResize = () => terminals.refit();
    window.addEventListener("resize", onResize);

    (async () => {
      const existing = await currentProject();
      if (existing) {
        await bootstrap(existing);
      } else {
        recents = await listRecentProjects();
      }
      ready = true;
    })();

    return () => {
      window.removeEventListener("resize", onResize);
      unlisteners.forEach((p) => p.then((f) => f()));
    };
  });
</script>

{#if ready && $project}
  <div class="shell">
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
  {#if $showMessages}
    <MessageReader onclose={() => showMessages.set(false)} />
  {/if}
  {#if $rename}
    <RenameDialog />
  {/if}
{:else if ready}
  <ProjectPicker
    {recents}
    onpick={pickAndOpen}
    onopen={openRecent}
  />
{/if}

<style>
  .shell {
    display: grid;
    grid-template-columns: var(--sidebar-w) 1fr;
    grid-template-rows: auto 1fr auto;
    grid-template-areas:
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
