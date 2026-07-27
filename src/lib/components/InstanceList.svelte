<script lang="ts">
  import { sessions, statuses, tasks, activeId, hub } from "../stores";
  import type { Status } from "../ipc";

  let {
    onselect,
    onmute,
  }: {
    onselect: (id: number) => void;
    /** Toggle mute for any row, without selecting it first. */
    onmute: (id: number, muted: boolean) => void;
  } = $props();

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

<!-- `$sessions` is already in display order (muted sunk to the bottom, creation
     order preserved within each group) — see stores.ts::displayOrder. -->
<div class="list">
  {#each $sessions as s (s.id)}
    {@const st = statusOf(s.id)}
    <!-- A muted row drops every attention signal — dot, status word, ⏳ — and
         carries 🔇 instead, so "dimmed" reads as "you silenced this" rather than
         "this one died". The mute toggle is a sibling of the select button, not
         nested inside it (a button inside a button is invalid HTML). -->
    <div class="row" class:active={s.id === $activeId} class:muted={s.muted}>
      <button class="body" onclick={() => onselect(s.id)}>
        <div class="head">
          {#if !s.muted}
            <span class="dot" style:background={DOT[st]}></span>
          {/if}
          <span class="id">claude #{s.id}</span>
          {#if !s.muted && waitOn(s.id) != null}
            <span class="wait" title="waiting on #{waitOn(s.id)}">⏳</span>
          {/if}
          {#if !s.muted}
            <span class="st">{LABEL[st]}</span>
          {/if}
        </div>
        {#if s.name}
          <div class="name">{s.name}</div>
        {:else if $tasks.get(s.id)}
          <div class="task">{$tasks.get(s.id)}</div>
        {/if}
      </button>
      <button
        class="mute"
        class:on={s.muted}
        aria-pressed={s.muted}
        title="{s.muted ? 'Unmute' : 'Mute'} claude #{s.id}{s.id === $activeId
          ? ' (⌘M)'
          : ''}"
        onclick={() => onmute(s.id, !s.muted)}
      >
        {s.muted ? "🔇" : "🔊"}
      </button>
    </div>
  {/each}
</div>

<style>
  .list {
    overflow-y: auto;
    padding: 0.35rem;
    border-bottom: 1px solid var(--border);
  }
  .row {
    display: flex;
    align-items: flex-start;
    gap: 0.15rem;
    width: 100%;
    padding: 0.4rem 0.35rem 0.4rem 0.5rem;
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
  .body {
    flex: 1;
    min-width: 0;
    display: block;
    text-align: left;
    background: transparent;
    border: none;
    padding: 0;
  }
  /* Dim the content, not the 🔇 — the marker has to stay legible to explain the
     dimming. */
  .row.muted .body {
    opacity: 0.45;
  }
  .mute {
    flex: none;
    background: none;
    border: none;
    padding: 0.1rem 0.15rem;
    border-radius: 4px;
    font-size: 0.72rem;
    line-height: 1;
    /* Unmuted rows keep the toggle invisible until you look at them: a 🔊 on
       every row is noise, but it has to be reachable without selecting first. */
    opacity: 0;
  }
  .row:hover .mute,
  .mute.on,
  .mute:focus-visible {
    opacity: 1;
  }
  .mute:hover {
    background: var(--border);
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
