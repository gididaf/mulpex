<script lang="ts">
  // ⌘O on a remote host: open a project on THAT machine. There is no native
  // folder dialog onto another disk, so this lists the host's recent projects
  // and lets you walk its folders (`browse_dir`, run there).
  import { browseDir, listRecentProjects, type DirListing } from "../ipc";
  import { hostName } from "../hosts";

  let {
    host,
    onopen,
    onclose,
  }: {
    host: number;
    onopen: (path: string, host: number) => void;
    onclose: () => void;
  } = $props();

  let recents = $state<string[]>([]);
  let dir = $state<DirListing | null>(null);
  let typed = $state("");
  let error = $state("");
  let sheet: HTMLDivElement | undefined = $state();

  async function go(path: string) {
    error = "";
    try {
      dir = await browseDir(path, host);
      typed = dir.path;
    } catch (e) {
      error = String(e);
    }
  }

  $effect(() => {
    listRecentProjects(host)
      .then((r) => (recents = r))
      .catch(() => {});
    void go("");
    sheet?.focus();
  });

  const base = (p: string) => p.split("/").filter(Boolean).pop() ?? p;
  const join = (a: string, b: string) => (a.endsWith("/") ? a + b : `${a}/${b}`);

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      onclose();
    }
  }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="backdrop" onclick={onclose}>
  <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
  <div
    class="sheet"
    role="dialog"
    tabindex="-1"
    aria-label="Open project on {hostName(host)}"
    bind:this={sheet}
    onclick={(e) => e.stopPropagation()}
    onkeydown={onKey}
  >
    <div class="label">Open project on ◆ {hostName(host)}</div>
    {#if recents.length}
      <div class="label sub">Recent</div>
      <div class="list recents">
        {#each recents.slice(0, 8) as r (r)}
          <button class="item" title={r} onclick={() => onopen(r, host)}>
            <span class="name">{base(r)}</span>
            <span class="path">{r}</span>
          </button>
        {/each}
      </div>
    {/if}
    <div class="label sub">Browse</div>
    <div class="row">
      <input
        bind:value={typed}
        spellcheck="false"
        onkeydown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            void go(typed);
          }
        }}
      />
      <button class="primary" disabled={!dir} onclick={() => dir && onopen(dir.path, host)}>Open this folder</button>
    </div>
    {#if dir}
      <div class="list">
        {#if dir.parent}
          <button class="item" onclick={() => dir?.parent && go(dir.parent)}><span class="name">..</span></button>
        {/if}
        {#each dir.dirs as d (d)}
          <button class="item" onclick={() => dir && go(join(dir.path, d))}>
            <span class="name">📁 {d}</span>
          </button>
        {/each}
        {#if dir.dirs.length === 0}
          <div class="empty">No folders here.</div>
        {/if}
      </div>
    {/if}
    {#if error}<div class="error">{error}</div>{/if}
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: grid;
    place-items: center;
    z-index: 50;
  }
  .sheet {
    width: min(34rem, 92vw);
    max-height: 80vh;
    display: flex;
    flex-direction: column;
    padding: 1rem;
    background: var(--bg-elev);
    border: 1px solid var(--border-focus);
    border-radius: 8px;
    outline: none;
  }
  .label {
    color: var(--label);
    font-size: 0.78rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    margin-bottom: 0.5rem;
  }
  .label.sub {
    margin-top: 0.6rem;
    margin-bottom: 0.3rem;
  }
  .row {
    display: flex;
    gap: 0.5rem;
  }
  input {
    flex: 1;
    min-width: 0;
    padding: 0.4rem 0.5rem;
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font-family: ui-monospace, monospace;
    font-size: 0.8rem;
  }
  input:focus {
    outline: none;
    border-color: var(--border-focus);
  }
  .list {
    margin-top: 0.4rem;
    overflow-y: auto;
    min-height: 0;
    flex: 1;
    border: 1px solid var(--border);
    border-radius: 6px;
  }
  .list.recents {
    flex: none;
    max-height: 12rem;
  }
  .item {
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
    width: 100%;
    padding: 0.3rem 0.6rem;
    background: none;
    border: none;
    color: var(--text);
    text-align: left;
    font-size: 0.85rem;
    cursor: pointer;
  }
  .item:hover {
    background: var(--bg);
  }
  .path {
    color: var(--text-faint);
    font-size: 0.75rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .empty {
    padding: 0.5rem 0.6rem;
    color: var(--text-faint);
    font-size: 0.85rem;
  }
  button {
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font: inherit;
    padding: 0.35rem 0.8rem;
    cursor: pointer;
  }
  button.primary {
    border-color: var(--border-focus);
    flex: none;
  }
  .error {
    color: #f85149;
    font-size: 0.85rem;
    margin-top: 0.5rem;
  }
</style>
