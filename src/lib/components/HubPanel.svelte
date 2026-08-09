<script lang="ts">
  import { hub, showMessages, unread } from "../stores";
</script>

<!-- Messages is the whole panel. Locks and Waiting used to render above it,
     anomaly-only; both were dropped because neither is something the user
     steers with. Contention is still surfaced where the eye already is — a ⏳
     on the blocked session's row in InstanceList, which reads `$hub.waiting`
     directly, so the snapshot still carries locks/waiting for that. -->
<div class="hub">
  <section class="grow">
    <button class="label as-button" onclick={() => showMessages.set(true)}>
      <!-- The unread *count* excludes muted recipients, but the message list
           below does not: mute silences the nag, not the record. -->
      Messages{#if $unread > 0}
        <span class="unread">({$unread} unread)</span>
      {/if}
    </button>
    <!-- Both ends are already addresses (`claude#2`, `all`, `central-one#3`), so
         they render verbatim — a route must stay unambiguous now that a sender
         can live in another project. The index is in the key because
         (ts, from, to) is not unique: two messages on the same route inside one
         second are possible, and Svelte 5 throws on a duplicate key. -->
    {#if $hub && $hub.messages.length}
      {#each $hub.messages.slice(0, 8) as m, i (m.ts + "-" + m.from + "-" + m.to + "-" + i)}
        <div class="msg">
          <span class="route">{m.from}→{m.to}</span>
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
  .msg {
    display: flex;
    gap: 0.4rem;
    align-items: baseline;
    font-size: 0.76rem;
    margin-bottom: 2px;
  }
  .snippet {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text-dim);
  }
  .route {
    color: var(--accent);
    flex: none;
  }
  .empty {
    color: var(--text-faint);
    font-size: 0.76rem;
    font-style: italic;
  }
</style>
