<script lang="ts">
  // One board image full size, over the whole window. ← / → step through the
  // strip it was opened from; Esc or a click outside closes it.
  import { imageUrl } from "../board";

  let { handle, ids, start, onclose }: { handle: number; ids: string[]; start: number; onclose: () => void } =
    $props();

  let i = $state(0);
  $effect(() => {
    i = start;
  });

  function onkey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      onclose();
    } else if (e.key === "ArrowRight" && ids.length > 1) {
      e.preventDefault();
      i = (i + 1) % ids.length;
    } else if (e.key === "ArrowLeft" && ids.length > 1) {
      e.preventDefault();
      i = (i - 1 + ids.length) % ids.length;
    }
  }
</script>

<svelte:window onkeydowncapture={onkey} />

<!-- svelte-ignore a11y_click_events_have_key_events -->
<div class="lb" role="dialog" aria-label="Image" tabindex="-1" onclick={onclose}>
  {#await imageUrl(handle, ids[i]) then url}
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <img src={url} alt="" onclick={(e) => e.stopPropagation()} />
  {/await}
  {#if ids.length > 1}
    <div class="count">{i + 1} / {ids.length}</div>
  {/if}
  <button class="x" title="Close (Esc)" onclick={onclose}>✕</button>
</div>

<style>
  .lb {
    position: fixed;
    inset: 0;
    z-index: 200;
    display: grid;
    place-items: center;
    background: rgba(0, 0, 0, 0.82);
    cursor: zoom-out;
  }
  img {
    max-width: 94vw;
    max-height: 90vh;
    object-fit: contain;
    border-radius: 4px;
    box-shadow: 0 6px 30px rgba(0, 0, 0, 0.5);
    cursor: default;
  }
  .count {
    position: absolute;
    bottom: 1rem;
    color: #ddd;
    font-size: 0.8rem;
  }
  .x {
    position: absolute;
    top: 0.8rem;
    right: 1rem;
    border: none;
    background: none;
    color: #ddd;
    font-size: 1.2rem;
  }
</style>
