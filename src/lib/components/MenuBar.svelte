<script lang="ts">
  // Linux only: the Mac's menu bar, drawn in the window. On macOS `menu.rs` is
  // the native menu; Linux has none (docs/linux.md), so this strip lists the
  // same menus with the same ids and runs them through the same `handleMenu`.
  // Keep it in step with `menu.rs` and `linuxKeys.ts` — a command added to one
  // and not here is missing from this menu.
  //
  // Mouse only: Alt belongs to the terminal (Option-as-Meta), so there is no
  // Alt-key navigation. Items cancel `mousedown`, so focus never leaves the
  // terminal or field it was in — Copy / Paste act on that, like Super+C / V.
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { activeId, hostProjects, sessions } from "../stores";

  let {
    onmenu,
    onedit,
  }: {
    onmenu: (id: string) => void;
    onedit: (kind: "copy" | "cut" | "paste" | "select_all") => void;
  } = $props();

  type Item =
    | { sep: true }
    | { label: string; keys?: string; run: () => void; checked?: boolean };
  type Menu = { title: string; items: Item[] };

  const SEP: Item = { sep: true };
  const cmd = (label: string, id: string, keys?: string): Item => ({ label, keys, run: () => onmenu(id) });

  const muted = $derived($sessions.find((s) => s.id === $activeId)?.muted ?? false);

  const menus: Menu[] = $derived([
    {
      title: "Mulpex",
      items: [
        cmd("Check for Updates…", "check_updates"),
        cmd("Live Token…", "live_token"),
        SEP,
        cmd("Checkpoint & Quit…", "checkpoint_quit", "⌘⇧Q"),
        cmd("Quit Mulpex", "quit", "⌘Q"),
      ],
    },
    {
      title: "File",
      items: [
        cmd("Open Project…", "open_project", "⌘O"),
        cmd("Close Project", "close_project", "⌘⇧W"),
        SEP,
        cmd("Next Project", "next_project", "⌘⇧]"),
        cmd("Previous Project", "prev_project", "⌘⇧["),
        cmd("Move Project Left", "move_project_left", "⌘⇧←"),
        cmd("Move Project Right", "move_project_right", "⌘⇧→"),
        // The open projects by name, ⌘1–⌘9 in tab-bar order.
        ...($hostProjects.length ? [SEP] : []),
        ...$hostProjects.slice(0, 9).map((p, i) => cmd(p.name, `project_${i + 1}`, `⌘${i + 1}`)),
        SEP,
        cmd("New Session", "new_session", "⌘T"),
        cmd("New Terminal", "new_terminal", "⌘⇧T"),
        cmd("Close Session", "close_session", "⌘W"),
        SEP,
        cmd("Import Docs…", "import_docs"),
        cmd("Remote Control…", "remote_control", "⌘⇧O"),
        cmd("Connect to Remote Mulpex…", "connect_host"),
        SEP,
        cmd("Next Host", "next_host", "⌘⌥]"),
        cmd("Previous Host", "prev_host", "⌘⌥["),
      ],
    },
    {
      title: "Edit",
      items: [
        { label: "Cut", keys: "⌘X", run: () => onedit("cut") },
        { label: "Copy", keys: "⌘C", run: () => onedit("copy") },
        { label: "Paste", keys: "⌘V", run: () => onedit("paste") },
        { label: "Select All", keys: "⌘A", run: () => onedit("select_all") },
      ],
    },
    {
      title: "Session",
      items: [
        cmd("Rename Session…", "rename", "⌘R"),
        cmd("Restart Session…", "restart", "⌘⇧R"),
        cmd("Save Session…", "save_session", "⌘S"),
        cmd("Load Session…", "load_session", "⌘L"),
        { label: "Mute Session", keys: "⌘M", run: () => onmenu("mute"), checked: muted },
        cmd("Messages", "messages", "⌘⇧M"),
        cmd("Explain Selection", "explain", "⌘⇧E"),
        cmd("Secrets…", "secrets", "⌘K"),
        cmd("Pin Selection", "pin_selection", "⌘⇧P"),
        cmd("Show/Hide Side Panel", "toggle_panel", "⌘⇧L"),
        cmd("Request Compact", "request_compact", "⌘⇧K"),
        cmd("Ask All Claudes…", "ask_all", "⌘⇧B"),
        SEP,
        cmd("Next Session", "next", "⌘]"),
        cmd("Previous Session", "prev", "⌘["),
        cmd("Move Instance Up", "move_instance_up", "⌘⇧↑"),
        cmd("Move Instance Down", "move_instance_down", "⌘⇧↓"),
      ],
    },
    {
      title: "Window",
      items: [
        cmd("Minimize", "minimize"),
        { label: "Maximize", run: () => void getCurrentWindow().toggleMaximize() },
        SEP,
        // Closing the window quits the app (lib.rs), with the usual teardown.
        { label: "Close Window", run: () => void getCurrentWindow().close() },
      ],
    },
  ]);

  /** The open menu's index, or null. */
  let open = $state<number | null>(null);

  function pick(item: Item) {
    open = null;
    if (!("sep" in item)) item.run();
  }

  function onWindowDown(e: MouseEvent) {
    if (open != null && !(e.target as HTMLElement).closest?.(".menubar")) open = null;
  }
  function onWindowKey(e: KeyboardEvent) {
    if (open != null && e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      open = null;
    }
  }
</script>

<svelte:window onmousedown={onWindowDown} onkeydowncapture={onWindowKey} onblur={() => (open = null)} />

<nav class="menubar" aria-label="Menu">
  {#each menus as m, i (m.title)}
    <div class="menu">
      <button
        class="title"
        class:open={open === i}
        onmousedown={(e) => {
          e.preventDefault();
          open = open === i ? null : i;
        }}
        onmouseenter={() => {
          if (open != null) open = i;
        }}>{m.title}</button
      >
      {#if open === i}
        <div class="drop" role="menu">
          {#each m.items as item, j (j)}
            {#if "sep" in item}
              <div class="sep"></div>
            {:else}
              <button
                class="item"
                role="menuitem"
                onmousedown={(e) => e.preventDefault()}
                onclick={() => pick(item)}
              >
                <span class="check">{item.checked ? "✓" : ""}</span>
                <span class="label">{item.label}</span>
                <span class="keys">{item.keys ?? ""}</span>
              </button>
            {/if}
          {/each}
        </div>
      {/if}
    </div>
  {/each}
</nav>

<style>
  .menubar {
    position: fixed;
    top: 0;
    left: 0;
    right: 0;
    height: var(--menubar-h);
    z-index: 900;
    display: flex;
    align-items: stretch;
    padding: 0 0.3rem;
    background: var(--bg-sidebar);
    border-bottom: 1px solid var(--border);
    font-size: 0.8rem;
    user-select: none;
  }
  .menu {
    position: relative;
    display: flex;
  }
  .title {
    background: transparent;
    border: 0;
    color: var(--text);
    padding: 0 0.65rem;
    cursor: default;
  }
  .title:hover,
  .title.open {
    background: var(--bg-elev);
  }
  .drop {
    position: absolute;
    top: 100%;
    left: 0;
    min-width: 15rem;
    max-height: calc(100vh - var(--menubar-h) - 8px);
    overflow-y: auto;
    padding: 0.25rem 0;
    background: var(--bg-elev);
    border: 1px solid var(--border);
    border-radius: 0 0 6px 6px;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.5);
  }
  .item {
    display: grid;
    grid-template-columns: 1.1rem 1fr auto;
    align-items: center;
    gap: 0.4rem;
    width: 100%;
    padding: 0.28rem 0.8rem 0.28rem 0.4rem;
    background: transparent;
    border: 0;
    color: var(--text);
    text-align: left;
    cursor: default;
  }
  .item:hover {
    background: var(--accent);
    color: #06232a;
  }
  .item:hover .keys {
    color: #06232a;
  }
  .check {
    text-align: center;
  }
  .label {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 22rem;
  }
  .keys {
    color: var(--text-dim);
    padding-left: 1.5rem;
    white-space: nowrap;
  }
  .sep {
    height: 1px;
    margin: 0.25rem 0;
    background: var(--border);
  }
</style>
