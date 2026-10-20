<script lang="ts">
  // A row of image thumbnails (a card's, a comment's, or ones waiting in the
  // composer). Click one to see it full size. Images are attachments the
  // backend stores by id (`board.rs`), fetched once into blob URLs
  // (`board.ts::imageUrl`) — never Markdown `<img>`, so the sanitizer stays strict.
  import { imageUrl } from "../board";
  import Lightbox from "./Lightbox.svelte";

  let {
    handle,
    ids,
    onremove,
    small = false,
  }: { handle: number; ids: string[]; onremove?: (id: string) => void; small?: boolean } = $props();

  let open: number | null = $state(null);
</script>

{#if ids.length}
  <div class="strip" class:small>
    {#each ids as id, i (id)}
      <div class="thumb">
        {#await imageUrl(handle, id)}
          <div class="ph"></div>
        {:then url}
          <button class="img" title="View" onclick={() => (open = i)}><img src={url} alt="" /></button>
        {:catch}
          <div class="ph bad" title="This image is gone">?</div>
        {/await}
        {#if onremove}
          <button class="rm" title="Remove" onclick={() => onremove(id)}>✕</button>
        {/if}
      </div>
    {/each}
  </div>
{/if}
{#if open != null}
  <Lightbox {handle} {ids} start={open} onclose={() => (open = null)} />
{/if}

<style>
  .strip {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
    margin-top: 0.3rem;
  }
  .thumb {
    position: relative;
    width: 3.6rem;
    height: 3.6rem;
  }
  .small .thumb {
    width: 2.6rem;
    height: 2.6rem;
  }
  .img,
  .ph {
    display: block;
    width: 100%;
    height: 100%;
    padding: 0;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg);
    overflow: hidden;
    cursor: zoom-in;
  }
  .ph {
    cursor: default;
  }
  .ph.bad {
    display: grid;
    place-items: center;
    color: var(--text-faint);
    font-size: 0.8rem;
  }
  img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
  }
  .rm {
    position: absolute;
    top: -0.35rem;
    right: -0.35rem;
    width: 1rem;
    height: 1rem;
    padding: 0;
    border: 1px solid var(--border);
    border-radius: 50%;
    background: var(--bg-elev);
    color: var(--text-dim);
    font-size: 0.55rem;
    line-height: 1;
  }
  .rm:hover {
    color: var(--text);
  }
</style>
