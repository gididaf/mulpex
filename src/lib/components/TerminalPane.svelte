<script lang="ts">
  import { onMount } from "svelte";
  import { sessions } from "../stores";
  import { terminals } from "../terminals";
  import TerminalView from "./TerminalView.svelte";

  let paneEl: HTMLDivElement;

  onMount(() => {
    // Refit whenever the pane's size changes: initial layout, window resize,
    // and anything that shifts the sidebar. All PTYs share this geometry.
    const ro = new ResizeObserver(() => terminals.refit());
    ro.observe(paneEl);
    return () => ro.disconnect();
  });
</script>

<div class="pane-inner" bind:this={paneEl}>
  {#if $sessions.length === 0}
    <div class="empty">No active Claude — press ⌘T to start one</div>
  {:else}
    {#each $sessions as s (s.id)}
      <TerminalView id={s.id} />
    {/each}
  {/if}
</div>

<style>
  .pane-inner {
    position: relative;
    height: 100%;
    width: 100%;
    overflow: hidden;
  }
  .empty {
    display: grid;
    place-items: center;
    height: 100%;
    color: var(--text-faint);
    font-size: 0.9rem;
  }
</style>
