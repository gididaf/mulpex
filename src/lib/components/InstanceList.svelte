<script lang="ts">
  import { sessions, statuses, tasks, activeId, hub } from "../stores";
  import type { Status } from "../ipc";

  let { onselect }: { onselect: (id: number) => void } = $props();

  const DOT: Record<Status, string> = {
    working: "var(--dot-working)",
    waiting: "var(--dot-ready)",
    needs: "var(--dot-needs)",
  };
  const LABEL: Record<Status, string> = {
    working: "working",
    waiting: "ready",
    needs: "needs you",
  };

  function statusOf(id: number): Status {
    return $statuses.get(id) ?? "waiting";
  }
  function waitOn(id: number): number | null {
    const w = $hub?.waiting.find((x) => x.id === id);
    return w ? w.holder : null;
  }
</script>

<div class="list">
  {#each $sessions as s (s.id)}
    {@const st = statusOf(s.id)}
    <button
      class="row"
      class:active={s.id === $activeId}
      onclick={() => onselect(s.id)}
    >
      <div class="head">
        <span class="dot" style:background={DOT[st]}></span>
        <span class="id">claude #{s.id}</span>
        {#if waitOn(s.id) != null}
          <span class="wait" title="waiting on #{waitOn(s.id)}">⏳</span>
        {/if}
        <span class="st">{LABEL[st]}</span>
      </div>
      {#if s.name}
        <div class="name">{s.name}</div>
      {:else if $tasks.get(s.id)}
        <div class="task">{$tasks.get(s.id)}</div>
      {/if}
    </button>
  {/each}
</div>

<style>
  .list {
    overflow-y: auto;
    padding: 0.35rem;
    border-bottom: 1px solid var(--border);
  }
  .row {
    display: block;
    width: 100%;
    text-align: left;
    padding: 0.4rem 0.5rem;
    margin-bottom: 0.25rem;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 6px;
  }
  .row:hover {
    background: var(--bg-elev);
  }
  .row.active {
    border-color: var(--border-focus);
    background: var(--bg-elev);
  }
  .head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex: none;
  }
  .id {
    font-weight: 600;
  }
  .wait {
    font-size: 0.8rem;
  }
  .st {
    margin-left: auto;
    color: var(--text-faint);
    font-size: 0.72rem;
  }
  .name {
    margin-top: 2px;
    color: var(--text);
    font-weight: 600;
    font-size: 0.8rem;
  }
  .task {
    margin-top: 2px;
    color: var(--text-dim);
    font-size: 0.78rem;
    font-style: italic;
    display: -webkit-box;
    -webkit-line-clamp: 3;
    line-clamp: 3;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
</style>
