<script lang="ts">
  import { showMessages, unread } from "../stores";
</script>

<!-- Messages is one line now: the label and an unread badge. The feed of the
     last 8 messages that used to fill this panel was dropped on the user's
     ask (2026-11-20) — tapping opens the full reader (⌘⇧M), which is where
     messages are actually read. Locks and Waiting were dropped earlier; a
     blocked session still shows ⏳ on its row in InstanceList, from
     `$hub.waiting`. The badge excludes muted recipients: mute silences the
     nag, not the record. -->
<div class="hub">
  <button class="label as-button" onclick={() => showMessages.set(true)} title="Open messages (⌘⇧M)">
    Messages{#if $unread > 0}<span class="badge">{$unread}</span>{/if}
  </button>
</div>

<style>
  .hub {
    flex: none;
    padding: 0.4rem 0.5rem;
    border-top: 1px solid var(--border);
  }
  .label {
    color: var(--label);
    font-size: 0.72rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }
  .as-button {
    background: none;
    border: none;
    padding: 0;
    display: flex;
    align-items: center;
    gap: 0.4rem;
    cursor: pointer;
  }
  .as-button:hover {
    filter: brightness(1.2);
  }
  .badge {
    background: var(--dot-needs);
    color: #fff;
    border-radius: 999px;
    min-width: 1.1rem;
    padding: 0 0.35rem;
    font-size: 0.66rem;
    line-height: 1.1rem;
    text-align: center;
    letter-spacing: 0;
    font-weight: 600;
  }
</style>
