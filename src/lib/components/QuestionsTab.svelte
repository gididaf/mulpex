<script lang="ts">
  // The side panel's Questions tab: every open async question in the active
  // project, grouped by claude, the focused claude first.
  import { activeProjectHandle, activeId, sessions } from "../stores";
  import { questions } from "../questions";
  import { addr } from "../agents";
  import QuestionCard from "./QuestionCard.svelte";
  import type { QCard } from "../ipc";

  const h = $derived($activeProjectHandle);
  const list = $derived(h != null ? ($questions.get(h) ?? []) : []);

  const groups = $derived.by(() => {
    const m = new Map<number, QCard[]>();
    for (const c of list) m.set(c.instance, [...(m.get(c.instance) ?? []), c]);
    const ids = [...m.keys()].sort((a, b) => (a === $activeId ? -1 : b === $activeId ? 1 : a - b));
    return ids.map((id) => ({ id, cards: m.get(id)! }));
  });

  function label(id: number): string {
    const s = $sessions.find((x) => x.id === id);
    const a = addr(s?.kind, id);
    return s?.name ? `${a} · ${s.name}` : a;
  }
</script>

<div class="wrap">
  {#if h == null || groups.length === 0}
    <div class="empty">
      No open questions. A claude that asks with <code>metadata.source: "async"</code> keeps working, and its question waits here.
    </div>
  {:else}
    {#each groups as g (g.id)}
      <div class="group">
        <div class="who" class:focused={g.id === $activeId} dir="auto">{label(g.id)}</div>
        {#each g.cards as c (c.qid)}
          <QuestionCard handle={h!} card={c} />
        {/each}
      </div>
    {/each}
  {/if}
</div>

<style>
  .wrap {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 0.5rem;
    display: flex;
    flex-direction: column;
    gap: 0.8rem;
  }
  .empty {
    color: var(--text-faint);
    font-size: 0.8rem;
    padding: 1rem 0.5rem;
    line-height: 1.4;
  }
  .group {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }
  .who {
    font-size: 0.75rem;
    font-weight: 700;
    color: var(--text-dim);
  }
  .who.focused {
    color: var(--text);
  }
</style>
