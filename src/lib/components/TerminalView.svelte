<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { terminals } from "../terminals";

  let { id }: { id: number } = $props();
  let el: HTMLDivElement;

  onMount(() => {
    terminals.create(id, el);
  });
  onDestroy(() => {
    terminals.dispose(id);
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
