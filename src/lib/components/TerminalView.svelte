<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { terminals } from "../terminals";
  import type { SessionKind } from "../ipc";

  let {
    handle,
    id,
    kind,
    exited,
  }: {
    handle: number;
    id: number;
    kind: SessionKind;
    exited: boolean;
  } = $props();
  let el: HTMLDivElement;

  onMount(() => {
    terminals.create(handle, id, el, kind);
  });
  onDestroy(() => {
    terminals.dispose(handle, id);
  });

  // A terminal's shell can exit long after its xterm was created.
  $effect(() => {
    terminals.setExited(handle, id, exited);
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
