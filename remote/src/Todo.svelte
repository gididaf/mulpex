<script lang="ts">
  // "To Do" on the phone: each claude's list of what it needs from you, as on
  // the desktop's side panel (`src/lib/components/TodoTab.svelte`). Your
  // edits are drafts held by the Mac (`src-tauri/src/todos.rs`) — the desktop
  // shows the same ones — until UPDATE sends each claude what changed. Each
  // item has a comment thread; your reply is one draft per item, and opening a
  // thread marks the claude's comments read (on every device).
  // Titles are plain text; details and comments are Markdown, sanitized in
  // `renderRich` (everything here was written by a model or by you).

  import { renderRich } from "./richmd";

  export type TodoKind = "action" | "decision" | "question";
  type Item = {
    n: number;
    kind: TodoKind;
    title: string;
    detail: string | null;
    choices: string[];
    status: "open" | "done";
    done_by: "claude" | "user" | null;
    author: "claude" | "user";
    comments: { by: "claude" | "user"; text: string; at: number }[];
    seen: number;
    chosen: string | null;
    rev: number;
    for: number | null;
    urgent: boolean;
  };
  type Draft = {
    done: boolean | null;
    comment: string | null;
    chosen: string | null;
    delete: boolean;
    base_rev: number;
    orphan: { kind: TodoKind; title: string } | null;
  };
  export type TodoList = {
    items: Item[];
    drafts: { edits: Record<string, Draft>; added: { tmp: string; kind: TodoKind; title: string; detail: string | null }[] };
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
    lists: Record<string, TodoList>;
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

  const pending = (l: TodoList | undefined) => (l ? Object.keys(l.drafts.edits).length + l.drafts.added.length : 0);
  const open = (l: TodoList | undefined) => l?.items.filter((i) => i.status === "open").length ?? 0;

  const groups = $derived(
    claudes
      .filter((c) => (all ? true : c.id === focus))
      .map((c) => ({ ...c, list: lists[String(c.id)] }))
      .filter((g) => !all || (g.list && (g.list.items.length || pending(g.list)))),
  );
  const sendable = $derived(groups.filter((g) => pending(g.list) && !g.list?.sending));
  const total = $derived(sendable.reduce((n, g) => n + pending(g.list), 0));

  const ICON: Record<TodoKind, string> = { action: "▶", decision: "◆", question: "?" };

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

  const unread = (i: Item) => i.comments.slice(i.seen).some((c) => c.by === "claude");

  /** Open threads, by `"<id>:<n>"`. */
  let threads = $state(new Set<string>());
  function toggleThread(id: number, n: number) {
    const k = `${id}:${n}`;
    const next = new Set(threads);
    if (next.has(k)) next.delete(k);
    else next.add(k);
    threads = next;
  }
  // An open thread is being read, including what arrives while it is open.
  $effect(() => {
    for (const g of groups) {
      for (const i of g.list?.items ?? []) {
        if (threads.has(`${g.id}:${i.n}`) && unread(i)) edit(g.id, { op: "seen", n: i.n });
      }
    }
  });

  function saveReply(id: number, i: Item, d: Draft | undefined, v: string) {
    if (v !== (d?.comment ?? "")) edit(id, { op: "comment", n: i.n, text: v });
  }

  let addTitle = $state("");
  let addKind = $state<TodoKind>("action");
  function add() {
    if (focus == null || !addTitle.trim()) return;
    edit(focus, { op: "add", kind: addKind, title: addTitle });
    addTitle = "";
  }
</script>

<div class="todo">
  <header>
    <button class="back" onclick={onback} aria-label="Back">‹</button>
    <span class="title">To Do</span>
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
      {#each [...(g.list?.items ?? [])].sort((a, b) => Number(b.urgent) - Number(a.urgent)) as i (i.n)}
        {@const d = g.list?.drafts.edits[String(i.n)]}
        {@const done = d?.done ?? i.status === "done"}
        {@const chosen = d?.chosen ?? i.chosen}
        {@const k = `${g.id}:${i.n}`}
        <div class="item" class:done class:gone={d?.delete} class:pending={!!d} class:urgent={i.urgent}>
          <input type="checkbox" checked={done} disabled={d?.delete} onchange={() => edit(g.id, { op: "done", n: i.n, done: !done })} />
          <div class="main">
            <div class="head"><span class="kind">{ICON[i.kind]}</span><span class="t" dir="auto">{i.title}</span>{#if i.urgent}<span class="urgent-tag">URGENT</span>{/if}</div>
            <div class="meta">
              {#if i.for != null}<span class="for">for claude#{i.for}</span>{/if}
              #{i.n}{#if i.author === "user"} · yours{/if}{#if d} · <b>pending</b>{/if}{#if d && i.rev > d.base_rev} · changed since your edit{/if}
            </div>
            {#if i.detail}<details><summary>more</summary><div class="detail md">{@html renderRich(i.detail)}</div></details>{/if}
            {#if i.choices.length}
              <div class="choices">
                {#each i.choices as c (c)}
                  <button class="chip" class:on={chosen === c} dir="auto" disabled={d?.delete}
                    onclick={() => edit(g.id, { op: "choose", n: i.n, choice: chosen === c && d?.chosen ? null : c })}>{c}</button>
                {/each}
              </div>
            {/if}
            {#if threads.has(k)}
              <div class="thread">
                {#each i.comments as c, ci (ci)}
                  <div class="comment" class:by-claude={c.by === "claude"}>
                    <span class="who">{c.by === "claude" ? `claude#${g.id}` : "you"}</span>
                    <div class="ctext md">{@html renderRich(c.text)}</div>
                  </div>
                {/each}
                {#if !d?.delete}
                  <textarea
                    dir="auto"
                    rows="2"
                    placeholder="Reply… sent at UPDATE"
                    value={d?.comment ?? ""}
                    onchange={(e) => saveReply(g.id, i, d, (e.currentTarget as HTMLTextAreaElement).value)}
                  ></textarea>
                {/if}
              </div>
            {:else if d?.comment}
              <div class="note" dir="auto">{d.comment}</div>
            {/if}
            <div class="tools">
              <button class:unread={unread(i)} onclick={() => toggleThread(g.id, i.n)}
                >💬 {i.comments.length ? i.comments.length : "Comment"}{#if unread(i)} · new{/if}</button
              >
              {#if d}<button onclick={() => edit(g.id, { op: "discard", n: i.n })}>↺ Undo</button>{/if}
            </div>
          </div>
        </div>
      {/each}
      {#each Object.entries(g.list?.drafts.edits ?? {}).filter(([, x]) => x.orphan) as [n, o] (n)}
        <div class="item pending gone">
          <div class="main">
            <div class="head"><span class="t" dir="auto">{o.orphan?.title}</span></div>
            <div class="meta">#{n} · closed by claude · your reply is still sent</div>
            {#if o.comment}<div class="note" dir="auto">{o.comment}</div>{/if}
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
  .todo {
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
  .item.urgent {
    border-inline-start-color: var(--red);
  }
  .urgent-tag {
    flex: none;
    margin-inline-start: 0.4rem;
    padding: 0 0.35rem;
    border-radius: 4px;
    background: var(--red);
    color: #fff;
    font-size: 0.65rem;
    font-weight: 700;
    line-height: 1.1rem;
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
  .for {
    padding: 0 0.35rem;
    margin-inline-end: 0.3rem;
    border-radius: 4px;
    background: var(--bg-elev);
    border: 1px solid var(--border);
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
  .thread {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    margin-top: 0.4rem;
  }
  .comment {
    padding-inline-start: 0.5rem;
    border-inline-start: 2px solid var(--border);
    font-size: 0.85rem;
  }
  .comment.by-claude {
    border-inline-start-color: var(--accent);
  }
  .who {
    display: block;
    font-size: 0.72rem;
    color: var(--label);
  }
  .ctext {
    overflow-wrap: anywhere;
  }
  .detail.md {
    white-space: normal;
    unicode-bidi: normal;
  }
  .md :global(p) {
    margin: 0 0 0.5rem;
  }
  .md :global(:is(ul, ol)) {
    margin: 0 0 0.5rem;
    padding-inline-start: 1.3rem;
  }
  .md :global(:is(h1, h2, h3, h4, h5, h6)) {
    margin: 0.6rem 0 0.3rem;
    font-size: 0.95rem;
    color: var(--text);
  }
  .md :global(code) {
    direction: ltr;
    unicode-bidi: isolate;
    padding: 0.05rem 0.25rem;
    border-radius: 3px;
    background: var(--bg-elev);
    font-size: 0.9em;
  }
  .md :global(pre) {
    direction: ltr;
    margin: 0 0 0.5rem;
    overflow-x: auto;
    padding: 0.45rem 0.55rem;
    border-radius: 5px;
    background: var(--bg-elev);
  }
  .md :global(pre code) {
    padding: 0;
    background: none;
  }
  .md :global(blockquote) {
    margin: 0 0 0.5rem;
    padding-inline-start: 0.5rem;
    border-inline-start: 2px solid var(--border);
  }
  .md :global(table) {
    display: block;
    overflow-x: auto;
    margin: 0 0 0.5rem;
    border-collapse: collapse;
  }
  .md :global(:is(td, th)) {
    padding: 0.2rem 0.45rem;
    border: 1px solid var(--border);
  }
  .md > :global(:last-child) {
    margin-bottom: 0;
  }
  .thread textarea {
    width: 100%;
    box-sizing: border-box;
    padding: 0.4rem 0.5rem;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: none;
    color: inherit;
    font: inherit;
    font-size: 0.9rem;
  }
  .tools button.unread {
    font-weight: 700;
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
