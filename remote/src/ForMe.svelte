<script lang="ts">
  // "For me" on the phone: each claude's list of what it needs from you, as on
  // the desktop's side panel (`src/lib/components/ForMeTab.svelte`). Your
  // edits are drafts held by the Mac (`src-tauri/src/asks.rs`) — the desktop
  // shows the same ones — until UPDATE sends each claude what changed.
  // Plain text only: everything here was written by a model or by you.

  export type AskKind = "action" | "decision" | "question";
  type Item = {
    n: number;
    kind: AskKind;
    title: string;
    detail: string | null;
    choices: string[];
    status: "open" | "done";
    done_by: "claude" | "user" | null;
    author: "claude" | "user";
    note: string | null;
    chosen: string | null;
    rev: number;
  };
  type Draft = {
    done: boolean | null;
    note: string | null;
    chosen: string | null;
    delete: boolean;
    base_rev: number;
    orphan: { kind: AskKind; title: string } | null;
  };
  export type AskList = {
    items: Item[];
    drafts: { edits: Record<string, Draft>; added: { tmp: string; kind: AskKind; title: string; detail: string | null }[] };
    sending: boolean;
  };
  type Reply = { ok: boolean; why: string | null };

  let {
    claudes,
    lists,
    focus,
    loading,
    onback,
    onedit,
    onupdate,
  }: {
    /** The project's claudes, in sidebar order. */
    claudes: { id: number; name: string | null }[];
    lists: Record<string, AskList>;
    /** The claude it was opened for; null shows them all. */
    focus: number | null;
    loading: boolean;
    onback: () => void;
    onedit: (id: number, edit: Record<string, unknown>) => Promise<Reply>;
    onupdate: (ids: number[]) => Promise<Reply>;
  } = $props();

  // Opened for one claude, it starts on that one; the toggle is the user's after that.
  // svelte-ignore state_referenced_locally
  let all = $state(focus == null);
  let error = $state<string | null>(null);
  let busy = $state(false);

  const pending = (l: AskList | undefined) => (l ? Object.keys(l.drafts.edits).length + l.drafts.added.length : 0);
  const open = (l: AskList | undefined) => l?.items.filter((i) => i.status === "open").length ?? 0;

  const groups = $derived(
    claudes
      .filter((c) => (all ? true : c.id === focus))
      .map((c) => ({ ...c, list: lists[String(c.id)] }))
      .filter((g) => !all || (g.list && (g.list.items.length || pending(g.list)))),
  );
  const sendable = $derived(groups.filter((g) => pending(g.list) && !g.list?.sending));
  const total = $derived(sendable.reduce((n, g) => n + pending(g.list), 0));

  const ICON: Record<AskKind, string> = { action: "▶", decision: "◆", question: "?" };

  async function edit(id: number, e: Record<string, unknown>) {
    error = null;
    const r = await onedit(id, e);
    if (!r.ok) error = r.why ?? "Couldn't do that";
  }

  async function update() {
    busy = true;
    error = null;
    const r = await onupdate(sendable.map((g) => g.id));
    busy = false;
    if (!r.ok || r.why) error = r.why ?? "Not sent";
  }

  function note(id: number, i: Item, d: Draft | undefined) {
    const cur = d?.note ?? i.note ?? "";
    const v = prompt(`Note for #${i.n}`, cur);
    if (v != null && v !== cur) edit(id, { op: "note", n: i.n, note: v });
  }

  let addTitle = $state("");
  let addKind = $state<AskKind>("action");
  function add() {
    if (focus == null || !addTitle.trim()) return;
    edit(focus, { op: "add", kind: addKind, title: addTitle });
    addTitle = "";
  }
</script>

<div class="forme">
  <header>
    <button class="back" onclick={onback} aria-label="Back">‹</button>
    <span class="title">For me</span>
    <div class="seg">
      {#if focus != null}<button class:on={!all} onclick={() => (all = false)}>claude#{focus}</button>{/if}
      <button class:on={all} onclick={() => (all = true)}>All</button>
    </div>
  </header>

  <div class="scroll">
    {#if loading}
      <div class="empty">Loading…</div>
    {:else if !groups.length || groups.every((g) => !g.list || (!g.list.items.length && !pending(g.list)))}
      <div class="empty">Nothing open for you{all ? " in this project" : ` from claude#${focus}`}.</div>
    {/if}
    {#each groups as g (g.id)}
      {#if all}
        <div class="group">claude#{g.id}{#if g.name}<span class="gname" dir="auto"> · {g.name}</span>{/if}
          {#if open(g.list)}<span class="badge">{open(g.list)}</span>{/if}
          {#if g.list?.sending}<span class="sending">sending…</span>{/if}
        </div>
      {/if}
      {#each g.list?.items ?? [] as i (i.n)}
        {@const d = g.list?.drafts.edits[String(i.n)]}
        {@const done = d?.done ?? i.status === "done"}
        {@const chosen = d?.chosen ?? i.chosen}
        {@const noteText = d?.note ?? i.note}
        <div class="item" class:done class:gone={d?.delete} class:pending={!!d}>
          <input type="checkbox" checked={done} disabled={d?.delete} onchange={() => edit(g.id, { op: "done", n: i.n, done: !done })} />
          <div class="main">
            <div class="head"><span class="kind">{ICON[i.kind]}</span><span class="t" dir="auto">{i.title}</span></div>
            <div class="meta">
              #{i.n}{#if i.author === "user"} · yours{/if}{#if d} · <b>pending</b>{/if}{#if d && i.rev > d.base_rev} · changed since your edit{/if}
            </div>
            {#if i.detail}<details><summary>more</summary><div class="detail" dir="auto">{i.detail}</div></details>{/if}
            {#if i.choices.length}
              <div class="choices">
                {#each i.choices as c (c)}
                  <button class="chip" class:on={chosen === c} dir="auto" disabled={d?.delete}
                    onclick={() => edit(g.id, { op: "choose", n: i.n, choice: chosen === c && d?.chosen ? null : c })}>{c}</button>
                {/each}
              </div>
            {/if}
            {#if noteText}<div class="note" dir="auto">{noteText}</div>{/if}
            <div class="tools">
              {#if !d?.delete}<button onclick={() => note(g.id, i, d)}>✎ Note</button>{/if}
              {#if d}<button onclick={() => edit(g.id, { op: "discard", n: i.n })}>↺ Undo</button>{/if}
              {#if !d?.delete}<button onclick={() => edit(g.id, { op: "dismiss", n: i.n, on: true })}>✕ Dismiss</button>{/if}
            </div>
          </div>
        </div>
      {/each}
      {#each Object.entries(g.list?.drafts.edits ?? {}).filter(([, x]) => x.orphan) as [n, o] (n)}
        <div class="item pending gone">
          <div class="main">
            <div class="head"><span class="t" dir="auto">{o.orphan?.title}</span></div>
            <div class="meta">#{n} · removed by claude · your note is still sent</div>
            {#if o.note}<div class="note" dir="auto">{o.note}</div>{/if}
            <div class="tools"><button onclick={() => edit(g.id, { op: "discard", n: Number(n) })}>✕ Don't send</button></div>
          </div>
        </div>
      {/each}
      {#each g.list?.drafts.added ?? [] as a (a.tmp)}
        <div class="item pending">
          <div class="main">
            <div class="head"><span class="kind">{ICON[a.kind]}</span><span class="t" dir="auto">{a.title}</span></div>
            <div class="meta">new · yours · <b>pending</b></div>
            <div class="tools"><button onclick={() => edit(g.id, { op: "remove_added", tmp: a.tmp })}>✕ Remove</button></div>
          </div>
        </div>
      {/each}
    {/each}
    {#if !all && focus != null}
      <div class="add">
        <select bind:value={addKind}>
          <option value="action">▶</option>
          <option value="decision">◆</option>
          <option value="question">?</option>
        </select>
        <input dir="auto" placeholder="Ask claude#{focus} to…" bind:value={addTitle} onkeydown={(e) => e.key === "Enter" && add()} />
        <button disabled={!addTitle.trim()} onclick={add}>Add</button>
      </div>
    {/if}
  </div>

  <footer>
    {#if error}<span class="err" dir="auto">{error}</span>{:else}<span class="status">{total ? `${total} pending` : "up to date"}</span>{/if}
    <button class="update" disabled={!sendable.length || busy} onclick={update}>
      UPDATE{#if sendable.length > 1} · {sendable.length} claudes{:else if total} ({total}){/if}
    </button>
  </footer>
</div>

<style>
  .forme {
    display: flex;
    flex-direction: column;
    height: 100dvh;
    padding-top: env(safe-area-inset-top);
    padding-bottom: env(safe-area-inset-bottom);
    box-sizing: border-box;
  }
  header {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.6rem 0.8rem;
    border-bottom: 1px solid var(--border);
  }
  .back {
    border: 0;
    background: none;
    color: var(--accent);
    font-size: 1.6rem;
    line-height: 1;
    padding: 0 0.4rem 0 0;
  }
  .title {
    font-weight: 600;
  }
  .seg {
    margin-inline-start: auto;
    display: flex;
    border: 1px solid var(--border);
    border-radius: 8px;
    overflow: hidden;
  }
  .seg button {
    border: 0;
    background: none;
    color: var(--label);
    font: inherit;
    font-size: 0.8rem;
    padding: 0.3rem 0.6rem;
  }
  .seg button.on {
    background: var(--bg-elev);
    color: var(--text);
  }
  .scroll {
    flex: 1;
    overflow-y: auto;
  }
  .empty {
    padding: 2rem 1rem;
    color: var(--label);
    text-align: center;
  }
  .group {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.8rem 1rem 0.3rem;
    font-size: 0.8rem;
    font-weight: 600;
    color: var(--label);
  }
  .gname {
    font-weight: 400;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .badge {
    padding: 0 0.4rem;
    border-radius: 999px;
    background: var(--accent);
    color: var(--bg);
    font-size: 0.72rem;
  }
  .sending {
    font-weight: 400;
    color: var(--yellow);
  }
  .item {
    display: flex;
    gap: 0.7rem;
    padding: 0.7rem 1rem;
    border-bottom: 1px solid var(--border);
    border-inline-start: 3px solid transparent;
  }
  .item.pending {
    border-inline-start-color: var(--accent);
  }
  .item.done .t,
  .item.gone .t {
    color: var(--label);
    text-decoration: line-through;
  }
  input[type="checkbox"] {
    width: 1.2rem;
    height: 1.2rem;
    margin-top: 0.15rem;
    accent-color: var(--accent);
  }
  .main {
    flex: 1;
    min-width: 0;
  }
  .head {
    display: flex;
    gap: 0.4rem;
  }
  .kind {
    color: var(--label);
    font-size: 0.8rem;
  }
  .t {
    overflow-wrap: anywhere;
    unicode-bidi: plaintext;
  }
  .meta {
    font-size: 0.75rem;
    color: var(--label);
  }
  .meta b {
    color: var(--accent);
    font-weight: 600;
  }
  details summary {
    font-size: 0.8rem;
    color: var(--accent);
  }
  .detail,
  .note {
    margin-top: 0.3rem;
    font-size: 0.85rem;
    color: var(--label);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    unicode-bidi: plaintext;
  }
  .note {
    padding-inline-start: 0.5rem;
    border-inline-start: 2px solid var(--accent);
  }
  .choices {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem;
    margin-top: 0.4rem;
  }
  .chip {
    padding: 0.3rem 0.8rem;
    border: 1px solid var(--border);
    border-radius: 999px;
    background: none;
    color: var(--text);
    font: inherit;
    font-size: 0.85rem;
  }
  .chip.on {
    border-color: var(--accent);
    background: var(--accent);
    color: var(--bg);
  }
  .tools {
    display: flex;
    gap: 0.8rem;
    margin-top: 0.4rem;
  }
  .tools button {
    border: 0;
    background: none;
    color: var(--accent);
    font: inherit;
    font-size: 0.8rem;
    padding: 0;
  }
  .add {
    display: flex;
    gap: 0.4rem;
    padding: 0.8rem 1rem;
  }
  .add input {
    flex: 1;
    min-width: 0;
    padding: 0.4rem 0.6rem;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg);
    color: var(--text);
    font: inherit;
  }
  .add select,
  .add button {
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-elev);
    color: var(--text);
    font: inherit;
  }
  footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    padding: 0.6rem 1rem;
    border-top: 1px solid var(--border);
  }
  .status {
    font-size: 0.8rem;
    color: var(--label);
  }
  .err {
    font-size: 0.8rem;
    color: var(--red);
  }
  .update {
    padding: 0.5rem 1.1rem;
    border: 0;
    border-radius: 8px;
    background: var(--accent);
    color: var(--bg);
    font: inherit;
    font-weight: 700;
  }
  .update:disabled {
    opacity: 0.4;
  }
</style>
