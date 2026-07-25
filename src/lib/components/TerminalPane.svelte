<script lang="ts">
  import { onMount } from "svelte";
  import { projects, activeProject } from "../stores";
  import { terminals } from "../terminals";
  import TerminalView from "./TerminalView.svelte";

  let paneEl: HTMLDivElement;

  // Every session across ALL open projects, so background projects' terminals
  // stay mounted (alive-while-hidden). Only the active project's active session
  // is visible; the rest are visibility:hidden but keep rendering.
  const all = $derived(
    [...$projects.values()].flatMap((p) =>
      p.sessions.map((s) => ({ handle: p.handle, id: s.id })),
    ),
  );
  const activeEmpty = $derived(($activeProject?.sessions.length ?? 0) === 0);

  onMount(() => {
    // Refit whenever the pane's size changes: initial layout, window resize,
    // and anything that shifts the sidebar. All PTYs share this geometry.
    const ro = new ResizeObserver(() => terminals.refit());
    ro.observe(paneEl);
    return () => ro.disconnect();
  });
</script>

<div class="pane-inner" bind:this={paneEl}>
  {#each all as e (e.handle + " " + e.id)}
    <TerminalView handle={e.handle} id={e.id} />
  {/each}
  {#if activeEmpty}
    <div class="empty">No active Claude — press ⌘T to start one</div>
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
