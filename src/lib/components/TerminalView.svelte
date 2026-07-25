<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { terminals } from "../terminals";

  let { handle, id }: { handle: number; id: number } = $props();
  let el: HTMLDivElement;

  onMount(() => {
    terminals.create(handle, id, el);
  });
  onDestroy(() => {
    terminals.dispose(handle, id);
  });
</script>

<!-- xterm owns this subtree after open(); Svelte never renders into it. -->
<div class="term" bind:this={el}></div>

<style>
  .term {
    position: absolute;
    inset: 0;
  }
</style>
