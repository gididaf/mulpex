<script lang="ts">
  import { hub } from "../stores";
  import { terminals } from "../terminals";

  let { onclose }: { onclose: () => void } = $props();

  function close() {
    onclose();
    terminals.refocus();
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") close();
  }

  function when(ts: number): string {
    const d = new Date(ts * 1000);
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }
</script>

<svelte:window onkeydown={onKey} />

<div
  class="backdrop"
  role="button"
  tabindex="-1"
  onclick={close}
  onkeydown={() => {}}
>
  <div class="panel" role="dialog" tabindex="-1" aria-label="Messages" onclick={(e) => e.stopPropagation()} onkeydown={() => {}}>
    <header>
      <span class="title">Messages</span>
      <button class="x" onclick={close} aria-label="Close">✕</button>
    </header>
    <div class="body">
      {#if $hub && $hub.messages.length}
        {#each $hub.messages as m (m.ts + "-" + m.from + "-" + m.to)}
          <article>
            <div class="meta">
              <span class="route">#{m.from} → {m.to}</span>
              <span class="ts">{when(m.ts)}</span>
            </div>
            <div class="text">{m.body}</div>
          </article>
        {/each}
      {:else}
        <div class="empty">No messages yet.</div>
      {/if}
    </div>
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    justify-content: flex-end;
  }
  .panel {
    width: min(34rem, 90vw);
    height: 100%;
    background: var(--bg-elev);
    border-left: 1px solid var(--border);
    display: flex;
    flex-direction: column;
  }
  header {
    display: flex;
    align-items: center;
    padding: 0.6rem 0.8rem;
    border-bottom: 1px solid var(--border);
  }
  .title {
    color: var(--label);
    text-transform: uppercase;
    letter-spacing: 0.08em;
    font-size: 0.8rem;
  }
  .x {
    margin-left: auto;
    background: none;
    border: none;
    color: var(--text-dim);
    font-size: 0.9rem;
  }
  .body {
    overflow-y: auto;
    padding: 0.6rem 0.8rem;
  }
  article {
    padding: 0.5rem 0;
    border-bottom: 1px solid var(--border);
  }
  .meta {
    display: flex;
    justify-content: space-between;
    font-size: 0.74rem;
    margin-bottom: 0.25rem;
  }
  .route {
    color: var(--accent);
  }
  .ts {
    color: var(--text-faint);
  }
  .text {
    white-space: pre-wrap;
    word-break: break-word;
    font-size: 0.82rem;
    color: var(--text);
  }
  .empty {
    color: var(--text-faint);
    font-style: italic;
  }
</style>
