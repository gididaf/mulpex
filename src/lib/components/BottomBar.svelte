<script lang="ts">
  import { sessions, activeId, notice, unread } from "../stores";

  const activeLabel = $derived(
    $activeId != null ? `claude #${$activeId}` : "no active session",
  );

  /** Every accelerator the app has, session group then project group. */
  const HINTS: Array<{ keys: string; label: string } | "sep"> = [
    { keys: "⌘T", label: "new" },
    { keys: "⌘W", label: "close" },
    { keys: "⌘R", label: "rename" },
    { keys: "⌘[ ⌘]", label: "session" },
    { keys: "⌘M", label: "mute" },
    { keys: "⌘⇧M", label: "messages" },
    "sep",
    { keys: "⌘1–9", label: "project" },
    { keys: "⌘P", label: "switcher" },
    { keys: "⌘⇧[ ⌘⇧]", label: "cycle" },
    { keys: "⌘O", label: "open" },
    { keys: "⌘⇧W", label: "close project" },
  ];
</script>

<footer class="bottom">
  <span class="active">{activeLabel}</span>
  <span class="sep">·</span>
  <span class="count">{$sessions.length} running</span>
  {#if $unread > 0}
    <span class="sep">·</span>
    <span class="pending">{$unread} unread</span>
  {/if}
  <span class="spacer"></span>
  {#if $notice}
    <span class="notice">{$notice}</span>
  {:else}
    <span class="hint">
      {#each HINTS as h}
        {#if h === "sep"}
          <span class="divider"></span>
        {:else}
          <span class="pair"><kbd>{h.keys}</kbd>{h.label}</span>
        {/if}
      {/each}
    </span>
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
  /* The old single faint string was #5c6370 on #16161a — ~3:1, under the 4.5:1
     readable floor. Keys are full-strength text on a chip, labels --text-dim. */
  .hint {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    overflow: hidden;
  }
  .pair {
    display: inline-flex;
    align-items: center;
    gap: 0.28rem;
    color: var(--text-dim);
    flex: 0 0 auto;
  }
  .hint kbd {
    font: inherit;
    color: var(--text);
    background: rgba(255, 255, 255, 0.07);
    border: 1px solid rgba(255, 255, 255, 0.09);
    border-radius: 3px;
    padding: 0 0.25rem;
  }
  .divider {
    flex: 0 0 auto;
    width: 1px;
    align-self: stretch;
    margin: 0.15rem 0;
    background: var(--border);
  }
</style>
