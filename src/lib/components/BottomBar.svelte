<script lang="ts">
  import { sessions, activeId, notice, unread } from "../stores";

  const active = $derived(
    $activeId == null ? null : ($sessions.find((x) => x.id === $activeId) ?? null),
  );

  const activeLabel = $derived(
    active == null
      ? "no active session"
      : `${active.kind === "shell" ? "term" : "claude"} #${active.id}`,
  );

  /**
   * The keys that apply to what's focused *right now* — not the whole
   * accelerator table.
   *
   * The bar used to list all twelve, which had two problems. It grew 1:1 with the
   * feature set until it filled the window; and it lied — `⌘M mute` / `⌘⇧M
   * messages` were advertised over a terminal pane, where mute is meaningless
   * (a shell produces none of the signals mute silences) and there is no session
   * to mute. Everything trimmed from here is one keystroke away in ⌘P, which is
   * why the anchor below is not optional.
   */
  const HINTS = $derived.by(() => {
    const out: Array<{ keys: string; label: string }> = [];
    if (active == null) {
      // Zero sessions (or the picker): the only useful keys are the ones that
      // make something to work in.
      out.push(
        { keys: "⌘T", label: "new" },
        { keys: "⌘⇧T", label: "terminal" },
        { keys: "⌘O", label: "open" },
      );
    } else if (active.kind === "shell") {
      out.push(
        { keys: "⌘T", label: "new" },
        { keys: "⌘⇧T", label: "terminal" },
        { keys: "⌘W", label: "close" },
        { keys: "⌘R", label: "rename" },
      );
    } else {
      out.push(
        { keys: "⌘T", label: "new" },
        { keys: "⌘W", label: "close" },
        { keys: "⌘R", label: "rename" },
        { keys: "⌘M", label: active.muted ? "unmute" : "mute" },
      );
    }
    // The reader is worth pointing at only when there's something in it.
    if ($unread > 0) out.push({ keys: "⌘⇧M", label: "messages" });
    return out;
  });
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
    <!-- The anchor sits OUTSIDE .hint on purpose. The strip clips rather than
         wraps, so whatever is last in the flex line is what a narrow window eats
         first — and that used to be the tail of the shortcut list, silently. With
         the shrinkable .hint holding the context keys and `⌘P all` pinned beside
         it, narrowing drops the hints (each of which the palette also offers) and
         always keeps the one key that reaches everything. -->
    <span class="hint">
      {#each HINTS as h}
        <span class="pair"><kbd>{h.keys}</kbd>{h.label}</span>
      {/each}
    </span>
    <span class="divider"></span>
    <span class="pair anchor"><kbd>⌘P</kbd>all</span>
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
    min-width: 0;
    overflow: hidden;
  }
  .pair {
    display: inline-flex;
    align-items: center;
    gap: 0.28rem;
    color: var(--text-dim);
    flex: 0 0 auto;
  }
  .anchor {
    color: var(--text);
  }
  .divider {
    flex: 0 0 auto;
    width: 1px;
    align-self: stretch;
    margin: 0.15rem 0;
    background: var(--border);
  }
  kbd {
    font: inherit;
    color: var(--text);
    background: rgba(255, 255, 255, 0.07);
    border: 1px solid rgba(255, 255, 255, 0.09);
    border-radius: 3px;
    padding: 0 0.25rem;
  }
</style>
