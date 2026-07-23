<script lang="ts">
  import { sessions, activeId, hub, notice } from "../stores";

  const activeLabel = $derived(
    $activeId != null ? `claude #${$activeId}` : "no active session",
  );
</script>

<footer class="bottom">
  <span class="active">{activeLabel}</span>
  <span class="sep">·</span>
  <span class="count">{$sessions.length} running</span>
  {#if $hub && $hub.pending_messages > 0}
    <span class="sep">·</span>
    <span class="pending">{$hub.pending_messages} unread</span>
  {/if}
  <span class="spacer"></span>
  {#if $notice}
    <span class="notice">{$notice}</span>
  {:else}
    <span class="hint">⌘T new · ⌘W close · ⌘R rename · ⌘[ ⌘] switch · ⌘M messages</span>
  {/if}
</footer>

<style>
  .bottom {
    grid-area: bottom;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.25rem 0.75rem;
    background: var(--bg-elev);
    border-top: 1px solid var(--border);
    color: var(--text-dim);
    font-size: 0.75rem;
    white-space: nowrap;
    overflow: hidden;
  }
  .active {
    color: var(--accent);
  }
  .pending {
    color: var(--label);
  }
  .spacer {
    flex: 1;
  }
  .notice {
    color: var(--dot-ready);
  }
  .hint {
    color: var(--text-faint);
    overflow: hidden;
    text-overflow: ellipsis;
  }
</style>
