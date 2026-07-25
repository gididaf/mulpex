<script lang="ts">
  import { projects, activeProjectHandle, type ProjectHandle } from "../stores";

  let {
    onselect,
    onclose,
    onadd,
  }: {
    onselect: (h: ProjectHandle) => void;
    onclose: (h: ProjectHandle) => void;
    onadd: () => void;
  } = $props();

  const list = $derived([...$projects.values()]);
</script>

<div class="tabs">
  {#each list as p (p.handle)}
    <div class="tab" class:active={p.handle === $activeProjectHandle}>
      <button class="label" title={p.dir} onclick={() => onselect(p.handle)}>
        <span class="name">{p.name}</span>
        {#if p.hub && p.hub.pending_messages > 0}
          <span class="badge">{p.hub.pending_messages}</span>
        {/if}
      </button>
      <button
        class="x"
        aria-label="Close project"
        title="Close project (⌘⇧W)"
        onclick={(e) => {
          e.stopPropagation();
          onclose(p.handle);
        }}>✕</button
      >
    </div>
  {/each}
  <button class="add" aria-label="Open project" title="Open project (⌘O)" onclick={onadd}>+</button>
</div>

<style>
  .tabs {
    grid-area: tabs;
    display: flex;
    align-items: stretch;
    gap: 0.25rem;
    padding: 0.25rem 0.4rem 0;
    background: var(--bg-sidebar);
    border-bottom: 1px solid var(--border);
    overflow-x: auto;
    white-space: nowrap;
  }
  .tab {
    display: flex;
    align-items: center;
    gap: 0.15rem;
    padding: 0.2rem 0.35rem 0.2rem 0.5rem;
    background: var(--bg-elev);
    border: 1px solid var(--border);
    border-bottom: none;
    border-radius: 6px 6px 0 0;
    max-width: 14rem;
  }
  .tab.active {
    border-color: var(--border-focus);
    background: var(--bg);
  }
  .label {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    background: none;
    border: none;
    padding: 0;
    color: var(--text-dim);
    font-size: 0.8rem;
    max-width: 12rem;
  }
  .tab.active .label {
    color: var(--text);
    font-weight: 600;
  }
  .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .badge {
    flex: none;
    min-width: 1.1rem;
    padding: 0 0.25rem;
    text-align: center;
    background: var(--dot-needs);
    color: #2a0a0d;
    border-radius: 0.7rem;
    font-size: 0.66rem;
    font-weight: 700;
  }
  .x {
    flex: none;
    background: none;
    border: none;
    color: var(--text-faint);
    font-size: 0.7rem;
    line-height: 1;
    padding: 0.15rem;
    border-radius: 4px;
  }
  .x:hover {
    color: var(--text);
    background: var(--border);
  }
  .add {
    flex: none;
    align-self: center;
    background: none;
    border: none;
    color: var(--text-dim);
    font-size: 1rem;
    line-height: 1;
    padding: 0.1rem 0.5rem;
    border-radius: 4px;
  }
  .add:hover {
    color: var(--text);
    background: var(--bg-elev);
  }
</style>
