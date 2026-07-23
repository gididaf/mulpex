<script lang="ts">
  import { hub, showMessages } from "../stores";

  function fileName(p: string): string {
    return p.split("/").filter(Boolean).pop() ?? p;
  }
</script>

<div class="hub">
  <section>
    <div class="label">Locks</div>
    {#if $hub && $hub.locks.length}
      {#each $hub.locks as l (l.path)}
        <div class="line">
          <span class="file" title={l.path}>{fileName(l.path)}</span>
          <span class="holder">#{l.holder}</span>
        </div>
      {/each}
    {:else}
      <div class="empty">none</div>
    {/if}
  </section>

  <section>
    <div class="label">Waiting</div>
    {#if $hub && $hub.waiting.length}
      {#each $hub.waiting as w (w.id)}
        <div class="line">
          <span class="holder">#{w.id}</span>
          <span class="wait">⏳ {w.file}</span>
          <span class="holder">→ #{w.holder}</span>
        </div>
      {/each}
    {:else}
      <div class="empty">none</div>
    {/if}
  </section>

  <section class="grow">
    <button class="label as-button" onclick={() => showMessages.set(true)}>
      Messages{#if $hub && $hub.pending_messages > 0}
        <span class="unread">({$hub.pending_messages} unread)</span>
      {/if}
    </button>
    {#if $hub && $hub.messages.length}
      {#each $hub.messages.slice(0, 8) as m (m.ts + "-" + m.from + "-" + m.to)}
        <div class="msg">
          <span class="route">#{m.from}→{m.to}</span>
          <span class="snippet">{m.body.replace(/\s+/g, " ").slice(0, 60)}</span>
        </div>
      {/each}
    {:else}
      <div class="empty">none</div>
    {/if}
  </section>
</div>

<style>
  .hub {
    display: flex;
    flex-direction: column;
    min-height: 0;
    padding: 0.4rem 0.5rem;
    overflow-y: auto;
    gap: 0.6rem;
  }
  .grow {
    flex: 1;
    min-height: 0;
  }
  .label {
    color: var(--label);
    font-size: 0.72rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    margin-bottom: 0.3rem;
  }
  .as-button {
    background: none;
    border: none;
    padding: 0;
    display: block;
  }
  .as-button:hover {
    filter: brightness(1.2);
  }
  .unread {
    color: var(--dot-needs);
    margin-left: 0.3rem;
  }
  .line,
  .msg {
    display: flex;
    gap: 0.4rem;
    align-items: baseline;
    font-size: 0.76rem;
    margin-bottom: 2px;
  }
  .file,
  .snippet {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .holder,
  .route {
    color: var(--accent);
    flex: none;
  }
  .wait {
    color: var(--text-dim);
  }
  .snippet {
    color: var(--text-dim);
  }
  .empty {
    color: var(--text-faint);
    font-size: 0.76rem;
    font-style: italic;
  }
</style>
