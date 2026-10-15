<script lang="ts">
  // The side panel's "To Do" tab: what each claude needs from the user, as the
  // claude wrote it with `hub_todo` (`todos.ts`, `src-tauri/src/todos.rs`).
  //
  // Everything the user does here — check, comment, pick a choice, add — is a
  // DRAFT the claude cannot see, marked "pending", until UPDATE sends each
  // claude a message listing exactly what changed. The drafts live on the
  // backend, so the phone and a remote desktop show the same ones.
  //
  // Each item has a comment thread both sides write to (the claude with
  // `hub_todo` action "comment"); the user's reply is one draft per item.
  // Opening a thread marks the claude's comments read — not a draft.
  //
  // Titles are plain text. Details and comments are Markdown, rendered only
  // through `renderMarkdownAuto` (DOMPurify; links and images dropped) — they come
  // from a model and the user, and this webview can invoke Tauri commands.
  import { activeProjectHandle, activeId, sessions } from "../stores";
  import { todos, todoKey, todosAll, editTodo, updateTodos, openCount, pendingCount, unread, byUrgency } from "../todos";
  import type { TodoDraft, TodoItem, TodoKind, TodoList } from "../ipc";
  import { terminals } from "../terminals";
  import { renderMarkdownAuto } from "../markdown";

  const h = $derived($activeProjectHandle);
  const id = $derived($activeId);
  const kind = $derived(id != null ? $sessions.find((s) => s.id === id)?.kind : undefined);

  /** The claudes to show, in sidebar order: the focused one, or all of them. */
  const groups = $derived.by(() => {
    if (h == null) return [] as { id: number; name: string | null; list: TodoList | undefined }[];
    const claudes = $sessions.filter((s) => s.kind === "claude");
    const pick = $todosAll ? claudes : claudes.filter((s) => s.id === id);
    return pick
      .map((s) => ({ id: s.id, name: s.name, list: $todos.get(todoKey(h, s.id)) }))
      .filter((g) => !$todosAll || (g.list && (g.list.items.length || pendingCount(g.list))));
  });

  const sendable = $derived(groups.filter((g) => g.list && pendingCount(g.list) && !g.list.sending));
  const sending = $derived(groups.some((g) => g.list?.sending));
  const pendingTotal = $derived(sendable.reduce((n, g) => n + pendingCount(g.list), 0));

  const KIND_ICON: Record<TodoKind, string> = { action: "▶", decision: "◆", question: "?" };
  const KIND_NAME: Record<TodoKind, string> = { action: "Action", decision: "Decision", question: "Question" };

  /** Expanded details and open threads, by `"<id>:<n>"`. Per view only. */
  let expanded = $state(new Set<string>());
  let threads = $state(new Set<string>());
  const toggle = (set: Set<string>, k: string) => {
    const next = new Set(set);
    if (next.has(k)) next.delete(k);
    else next.add(k);
    return next;
  };

  const draftOf = (l: TodoList | undefined, n: number): TodoDraft | undefined => l?.drafts.edits[String(n)];
  const isDone = (i: TodoItem, d: TodoDraft | undefined) => d?.done ?? i.status === "done";
  const chosenOf = (i: TodoItem, d: TodoDraft | undefined) => d?.chosen ?? i.chosen;
  const replyOf = (d: TodoDraft | undefined) => d?.comment ?? "";
  const orphans = (l: TodoList | undefined) =>
    l ? Object.entries(l.drafts.edits).filter(([, d]) => d.orphan).map(([n, d]) => ({ n: Number(n), d })) : [];

  function edit(cid: number, e: Parameters<typeof editTodo>[2]) {
    if (h != null) editTodo(h, cid, e);
  }

  function saveReply(cid: number, n: number, value: string) {
    edit(cid, { op: "comment", n, text: value });
  }

  function toggleThread(cid: number, i: TodoItem) {
    threads = toggle(threads, `${cid}:${i.n}`);
  }

  // An open thread is being read: whatever the claude adds to it while it is
  // open is read too, so its 💬 never lights up under the user's eyes.
  $effect(() => {
    for (const g of groups) {
      for (const i of g.list?.items ?? []) {
        if (threads.has(`${g.id}:${i.n}`) && unread(i)) edit(g.id, { op: "seen", n: i.n });
      }
    }
  });

  function choose(cid: number, i: TodoItem, d: TodoDraft | undefined, c: string) {
    // Clicking the picked choice again un-picks the draft.
    edit(cid, { op: "choose", n: i.n, choice: chosenOf(i, d) === c && d?.chosen ? null : c });
  }

  // ── add ────────────────────────────────────────────────────────────────────
  let addKind: TodoKind = $state("action");
  let addTitle = $state("");
  let addDetail = $state("");
  let addOpen = $state(false);

  function add() {
    if (id == null || !addTitle.trim() || kind !== "claude") return;
    edit(id, { op: "add", kind: addKind, title: addTitle, detail: addDetail.trim() || null });
    addTitle = "";
    addDetail = "";
  }

  function onAddKey(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      add();
    } else if (e.key === "Escape") {
      e.preventDefault();
      addOpen = false;
      terminals.refocus();
    }
  }

  function onReplyKey(e: KeyboardEvent, cid: number, n: number) {
    const el = e.currentTarget as HTMLTextAreaElement;
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      saveReply(cid, n, el.value);
      threads = toggle(threads, `${cid}:${n}`);
    } else if (e.key === "Escape") {
      e.preventDefault();
      saveReply(cid, n, el.value);
      threads = toggle(threads, `${cid}:${n}`);
    }
  }

  function update() {
    if (h == null) return;
    updateTodos(h, sendable.map((g) => g.id));
    terminals.refocus();
  }
</script>

<div class="tab">
  <div class="bar">
    <div class="seg" role="radiogroup" aria-label="Show">
      <button class:on={!$todosAll} onclick={() => todosAll.set(false)}>{id != null ? `${kind === "shell" ? "term" : "claude"}#${id}` : "This one"}</button>
      <button class:on={$todosAll} onclick={() => todosAll.set(true)}>All</button>
    </div>
  </div>

  <div class="list">
    {#if !$todosAll && kind === "shell"}
      <div class="empty">To Do works on claudes only.</div>
    {:else if !$todosAll && id == null}
      <div class="empty">No instance selected.</div>
    {:else if !groups.length || groups.every((g) => !g.list || (!g.list.items.length && !pendingCount(g.list)))}
      <div class="empty">
        {#if $todosAll}
          Nothing open for you in this project.
        {:else}
          {#if !$sessions.find((s) => s.id === id)?.todos_on}
            The To Do list is off for claude#{id}.<br />Right-click its row → To Do list → On, and it
            puts what it needs from you here.
          {:else}
            Nothing open for you from claude#{id}.<br />When it needs something from you, it lands here.
          {/if}
        {/if}
      </div>
    {/if}

    {#each groups as g (g.id)}
      {#if $todosAll}
        <div class="group">
          claude#{g.id}{#if g.name}<span class="gname"> · {g.name}</span>{/if}
          {#if openCount(g.list)}<span class="gcount">{openCount(g.list)}</span>{/if}
          {#if g.list?.sending}<span class="sending">sending…</span>{/if}
        </div>
      {/if}
      {#each byUrgency(g.list?.items ?? []) as i (i.n)}
        {@const d = draftOf(g.list, i.n)}
        {@const k = `${g.id}:${i.n}`}
        {@const done = isDone(i, d)}
        <div class="item" class:done class:gone={d?.delete} class:pending={!!d} class:urgent={i.urgent}>
          <input
            type="checkbox"
            checked={done}
            disabled={d?.delete}
            title={done ? "Mark open" : "Mark done"}
            onchange={() => edit(g.id, { op: "done", n: i.n, done: !done })}
          />
          <div class="main">
            <div class="head">
              <span class="kind k-{i.kind}" title={KIND_NAME[i.kind]}>{KIND_ICON[i.kind]}</span>
              <span class="title" dir="auto">{i.title}</span>
              {#if i.urgent}<span class="urgent-tag" title="The claude needs your attention on this as soon as possible">URGENT</span>{/if}
            </div>
            {#if i.for != null}<span class="for" title="A worker's ask, collected by claude#{g.id}">for claude#{i.for}</span>{/if}
            <div class="meta">
              <span>#{i.n}</span>
              {#if i.author === "user"}<span>· yours</span>{/if}
              {#if d}<span class="pend">· pending</span>{/if}
              {#if d && i.rev > d.base_rev}<span class="warn" title="The claude changed this item after you started editing it">· changed since your edit</span>{/if}
              {#if i.detail}
                <button class="link" onclick={() => (expanded = toggle(expanded, k))}>{expanded.has(k) ? "less" : "more"}</button>
              {/if}
            </div>
            {#if i.detail && expanded.has(k)}
              <!-- Sanitized in renderMarkdownAuto. -->
              <div class="detail md">{@html renderMarkdownAuto(i.detail)}</div>
            {/if}
            {#if i.choices.length}
              <div class="choices">
                {#each i.choices as c (c)}
                  <button class="chip" class:on={chosenOf(i, d) === c} dir="auto" disabled={d?.delete} onclick={() => choose(g.id, i, d, c)}>{c}</button>
                {/each}
              </div>
            {:else if i.chosen}
              <div class="note-view" dir="auto">chose: {i.chosen}</div>
            {/if}
            <!-- Like a post's action bar: the thread opens below the item. -->
            <div class="social">
              <button
                class="talk"
                class:open={threads.has(k)}
                class:unread={unread(i)}
                title={unread(i) ? "New reply from the claude" : threads.has(k) ? "Close the thread" : "Comments"}
                onclick={() => toggleThread(g.id, i)}
              ><span class="ico">💬</span><span class="lbl"
                  >{i.comments.length ? `${i.comments.length} comment${i.comments.length === 1 ? "" : "s"}` : "Comment"}</span
                >{#if unread(i)}<span class="udot"></span><span class="new">new</span>{/if}</button>
            </div>
            {#if threads.has(k)}
              <div class="thread">
                {#each i.comments as c, ci (ci)}
                  <div class="comment" class:by-claude={c.by === "claude"}>
                    <span class="who">{c.by === "claude" ? `claude#${g.id}` : "you"}</span>
                    <!-- Sanitized in renderMarkdownAuto. -->
                    <div class="text md">{@html renderMarkdownAuto(c.text)}</div>
                  </div>
                {/each}
                {#if !d?.delete}
                  <!-- svelte-ignore a11y_autofocus -->
                  <textarea
                    dir="auto"
                    rows="2"
                    placeholder="Reply… sent at UPDATE (⌘↩ to keep)"
                    value={replyOf(d)}
                    autofocus
                    onchange={(e) => saveReply(g.id, i.n, (e.currentTarget as HTMLTextAreaElement).value)}
                    onkeydown={(e) => onReplyKey(e, g.id, i.n)}
                  ></textarea>
                {/if}
              </div>
            {:else if replyOf(d)}
              <button class="note-view" dir="auto" title="Edit your reply" onclick={() => toggleThread(g.id, i)}>{replyOf(d)}</button>
            {/if}
          </div>
          <div class="tools">
            {#if d}
              <button title="Discard your pending edit" onclick={() => edit(g.id, { op: "discard", n: i.n })}>↺</button>
            {/if}
          </div>
        </div>
      {/each}
      {#each orphans(g.list) as o (o.n)}
        <div class="item gone-by-claude pending">
          <span class="kind k-{o.d.orphan?.kind}">{o.d.orphan ? KIND_ICON[o.d.orphan.kind] : ""}</span>
          <div class="main">
            <div class="head"><span class="title" dir="auto">{o.d.orphan?.title}</span></div>
            <div class="meta"><span>#{o.n} · closed by claude · your reply is still sent</span></div>
            {#if o.d.comment}<div class="note-view" dir="auto">{o.d.comment}</div>{/if}
            {#if o.d.chosen}<div class="note-view" dir="auto">chose: {o.d.chosen}</div>{/if}
          </div>
          <div class="tools">
            <button title="Don't send it" onclick={() => edit(g.id, { op: "discard", n: o.n })}>✕</button>
          </div>
        </div>
      {/each}
      {#each g.list?.drafts.added ?? [] as a (a.tmp)}
        <div class="item pending mine">
          <span class="kind k-{a.kind}" title={KIND_NAME[a.kind]}>{KIND_ICON[a.kind]}</span>
          <div class="main">
            <div class="head"><span class="title" dir="auto">{a.title}</span></div>
            <div class="meta"><span>new · yours · pending</span></div>
            <!-- Sanitized in renderMarkdownAuto. -->
            {#if a.detail}<div class="detail md">{@html renderMarkdownAuto(a.detail)}</div>{/if}
          </div>
          <div class="tools">
            <button title="Remove" onclick={() => edit(g.id, { op: "remove_added", tmp: a.tmp })}>✕</button>
          </div>
        </div>
      {/each}
    {/each}

    {#if !$todosAll && kind === "claude" && id != null}
      {#if addOpen}
        <div class="add">
          <div class="add-head">New request for claude#{id}</div>
          <div class="add-row">
            <select bind:value={addKind} title="Kind">
              <option value="action">▶ Action</option>
              <option value="decision">◆ Decision</option>
              <option value="question">? Question</option>
            </select>
            <!-- svelte-ignore a11y_autofocus -->
            <input dir="auto" placeholder="Ask claude#{id} to…" bind:value={addTitle} onkeydown={onAddKey} autofocus />
          </div>
          <textarea dir="auto" rows="2" placeholder="Details (optional)" bind:value={addDetail}></textarea>
          <div class="add-row end">
            <button class="link" onclick={() => { addOpen = false; terminals.refocus(); }}>Cancel</button>
            <button class="btn" disabled={!addTitle.trim()} onclick={add}>Add</button>
          </div>
        </div>
      {:else}
        <button class="add-open" onclick={() => (addOpen = true)}>+ Add a request for claude#{id}</button>
      {/if}
    {/if}
  </div>

  <footer>
    <span class="status">
      {#if sending}sending…{:else if pendingTotal}{pendingTotal} pending{:else}up to date{/if}
    </span>
    <button class="update" disabled={!sendable.length} onclick={update} title="Send your changes to the claude">
      UPDATE{#if sendable.length > 1} · {sendable.length} claudes{:else if pendingTotal} ({pendingTotal}){/if}
    </button>
  </footer>
</div>

<style>
  .tab {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
  }
  .bar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    min-height: 1.9rem;
    padding: 0.25rem 0.6rem;
    border-bottom: 1px solid var(--border);
  }
  .seg {
    display: flex;
    border: 1px solid var(--border);
    border-radius: 5px;
    overflow: hidden;
  }
  .seg button {
    border: none;
    background: none;
    color: var(--text-faint);
    font-size: 0.72rem;
    padding: 0.15rem 0.55rem;
  }
  .seg button.on {
    background: var(--border);
    color: var(--text);
  }
  .link {
    border: none;
    background: none;
    color: var(--text-dim);
    font-size: 0.72rem;
    padding: 0 0.25rem;
    border-radius: 4px;
    cursor: pointer;
  }
  .link:hover {
    color: var(--text);
  }
  .list {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding-bottom: 0.5rem;
  }
  .empty {
    padding: 1.2rem 0.9rem;
    color: var(--text-faint);
    font-size: 0.85rem;
    line-height: 1.5;
  }
  .group {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.55rem 0.8rem 0.2rem;
    font-size: 0.72rem;
    font-weight: 600;
    color: var(--text-dim);
  }
  .gname {
    font-weight: 400;
    color: var(--text-faint);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .gcount {
    padding: 0 0.3rem;
    border-radius: 0.6rem;
    background: var(--accent);
    color: var(--bg);
    font-size: 0.66rem;
    line-height: 1rem;
  }
  .sending {
    font-weight: 400;
    color: var(--dot-working);
  }
  .item {
    display: flex;
    gap: 0.5rem;
    align-items: flex-start;
    padding: 0.45rem 0.6rem 0.45rem 0.8rem;
    border-left: 2px solid transparent;
  }
  .item + .item {
    border-top: 1px solid var(--border);
  }
  .item.pending {
    border-left-color: var(--accent);
    background: color-mix(in srgb, var(--accent) 6%, transparent);
  }
  .item.urgent {
    border-left-color: var(--dot-needs, #e5534b);
  }
  .urgent-tag {
    flex: none;
    padding: 0 0.3rem;
    border-radius: 3px;
    background: var(--dot-needs, #e5534b);
    color: #fff;
    font-size: 0.6rem;
    font-weight: 700;
    letter-spacing: 0.03em;
    line-height: 1rem;
  }
  .item.done .title,
  .item.gone .title {
    color: var(--text-faint);
    text-decoration: line-through;
  }
  .item.gone-by-claude .title {
    color: var(--text-faint);
  }
  input[type="checkbox"] {
    margin-top: 0.2rem;
    accent-color: var(--accent);
    flex: none;
  }
  .main {
    flex: 1;
    min-width: 0;
  }
  .head {
    display: flex;
    gap: 0.4rem;
    align-items: baseline;
  }
  .kind {
    flex: none;
    font-size: 0.7rem;
    width: 0.9rem;
    text-align: center;
    color: var(--text-faint);
  }
  .k-decision {
    color: var(--accent);
  }
  .k-question {
    color: var(--dot-working);
    font-weight: 700;
  }
  .title {
    font-size: 0.86rem;
    line-height: 1.4;
    overflow-wrap: anywhere;
    unicode-bidi: plaintext;
  }
  .for {
    display: inline-block;
    margin: 0.1rem 0 0 1.3rem;
    padding: 0 0.35rem;
    border-radius: 3px;
    background: var(--border);
    color: var(--text-dim);
    font-size: 0.66rem;
  }
  .meta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.25rem;
    margin-top: 0.1rem;
    font-size: 0.68rem;
    color: var(--text-faint);
  }
  .pend {
    color: var(--accent);
  }
  .warn {
    color: var(--warn, #e0a34a);
  }
  .detail {
    margin-top: 0.3rem;
    padding: 0.3rem 0.5rem;
    border-radius: 4px;
    background: var(--bg);
    color: var(--text-dim);
    font-size: 0.78rem;
    line-height: 1.45;
    overflow-wrap: anywhere;
  }
  .choices {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
    margin-top: 0.35rem;
  }
  .chip {
    border: 1px solid var(--border);
    border-radius: 0.8rem;
    background: none;
    color: var(--text-dim);
    font-size: 0.75rem;
    padding: 0.12rem 0.6rem;
    unicode-bidi: plaintext;
  }
  .chip:hover:not(:disabled) {
    border-color: var(--border-focus);
    color: var(--text);
  }
  .chip.on {
    border-color: var(--accent);
    background: color-mix(in srgb, var(--accent) 20%, transparent);
    color: var(--text);
  }
  .note-view {
    display: block;
    width: 100%;
    margin-top: 0.3rem;
    padding: 0.25rem 0.45rem;
    border: none;
    border-inline-start: 2px solid var(--accent);
    border-radius: 0;
    background: none;
    color: var(--text-dim);
    font: inherit;
    font-size: 0.78rem;
    text-align: start;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    unicode-bidi: plaintext;
  }
  button.note-view {
    cursor: text;
  }
  /* The comments bar under each item, like a post's action bar. */
  .social {
    margin-top: 0.4rem;
    padding-top: 0.3rem;
    border-top: 1px solid color-mix(in srgb, var(--border) 70%, transparent);
  }
  .talk {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.15rem 0.5rem;
    border: none;
    border-radius: 5px;
    background: none;
    color: var(--text-dim);
    font-size: 0.8rem;
    font-weight: 600;
    line-height: 1.3rem;
    cursor: pointer;
  }
  .talk:hover,
  .talk.open {
    background: color-mix(in srgb, var(--text) 9%, transparent);
    color: var(--text);
  }
  .talk .ico {
    font-size: 0.9rem;
    line-height: 1;
  }
  .talk.unread {
    color: var(--accent);
  }
  .talk .new {
    font-size: 0.7rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .udot {
    width: 0.45rem;
    height: 0.45rem;
    border-radius: 50%;
    background: var(--accent);
  }
  .thread {
    margin-top: 0.35rem;
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }
  .comment {
    padding: 0.25rem 0.45rem;
    border-inline-start: 2px solid var(--border);
    font-size: 0.78rem;
    line-height: 1.45;
  }
  .comment.by-claude {
    border-inline-start-color: var(--dot-working);
  }
  .comment .who {
    display: block;
    font-size: 0.66rem;
    color: var(--text-faint);
  }
  .comment .text {
    color: var(--text-dim);
    overflow-wrap: anywhere;
  }
  /* Markdown in details and comments. Each block finds its own direction
     (Hebrew and English items sit side by side); code is always LTR. */
  .md :global(p) {
    margin: 0 0 0.45rem;
  }
  .md :global(:is(ul, ol)) {
    margin: 0 0 0.45rem;
    padding-inline-start: 1.2rem;
  }
  .md :global(li) {
    margin-bottom: 0.15rem;
  }
  .md :global(:is(h1, h2, h3, h4, h5, h6)) {
    margin: 0.55rem 0 0.3rem;
    color: var(--text);
    font-size: 0.86rem;
  }
  .md :global(strong) {
    color: var(--text);
  }
  .md :global(code) {
    direction: ltr;
    unicode-bidi: isolate;
    padding: 0.05rem 0.25rem;
    border-radius: 3px;
    background: color-mix(in srgb, var(--text) 10%, transparent);
    font-size: 0.9em;
  }
  .md :global(pre) {
    direction: ltr;
    margin: 0 0 0.45rem;
    overflow-x: auto;
    padding: 0.4rem 0.5rem;
    border-radius: 4px;
    background: color-mix(in srgb, var(--text) 7%, transparent);
  }
  .md :global(pre code) {
    padding: 0;
    background: none;
  }
  .md :global(blockquote) {
    margin: 0 0 0.45rem;
    padding-inline-start: 0.5rem;
    border-inline-start: 2px solid var(--border);
  }
  .md :global(table) {
    margin: 0 0 0.45rem;
    border-collapse: collapse;
    display: block;
    overflow-x: auto;
  }
  .md :global(:is(td, th)) {
    padding: 0.15rem 0.45rem;
    border: 1px solid var(--border);
  }
  .md :global(hr) {
    border: none;
    border-top: 1px solid var(--border);
  }
  .md > :global(:last-child) {
    margin-bottom: 0;
  }
  textarea,
  input:not([type]),
  select {
    width: 100%;
    margin-top: 0.3rem;
    padding: 0.3rem 0.45rem;
    border: 1px solid var(--border);
    border-radius: 5px;
    background: var(--bg);
    color: var(--text);
    font: inherit;
    font-size: 0.8rem;
    resize: vertical;
  }
  textarea:focus,
  input:focus,
  select:focus {
    outline: none;
    border-color: var(--border-focus);
  }
  .tools {
    display: flex;
    flex: none;
    gap: 0.05rem;
    opacity: 0.55;
  }
  .item:hover .tools {
    opacity: 1;
  }
  .tools button {
    border: none;
    background: none;
    color: var(--text-faint);
    font-size: 0.8rem;
    padding: 0 0.3rem;
    border-radius: 4px;
  }
  .tools button:hover {
    color: var(--text);
    background: var(--border);
  }
  .add-open {
    display: block;
    margin: 0.8rem 0.8rem 0.5rem;
    border: 1px dashed var(--border);
    border-radius: 5px;
    background: none;
    color: var(--text-faint);
    font-size: 0.78rem;
    padding: 0.35rem 0.6rem;
    width: calc(100% - 1.6rem);
    text-align: start;
  }
  .add-open:hover {
    color: var(--text-dim);
    border-color: var(--border-focus);
  }
  /* Its own card below a divider, so the form never reads as part of the
     last item. */
  .add {
    margin: 0.8rem 0.8rem 0.5rem;
    padding: 0.6rem 0.65rem 0.55rem;
    border: 1px solid var(--border);
    border-radius: 7px;
    background: color-mix(in srgb, var(--text) 4%, transparent);
  }
  .add-head {
    font-size: 0.7rem;
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--text-dim);
  }
  .add-row {
    display: flex;
    gap: 0.35rem;
    align-items: center;
  }
  .add-row select {
    width: auto;
    flex: none;
  }
  .add-row.end {
    justify-content: flex-end;
    margin-top: 0.3rem;
  }
  .btn,
  .update {
    border: 1px solid var(--accent);
    border-radius: 5px;
    background: none;
    color: var(--text);
    font-size: 0.8rem;
    padding: 0.25rem 0.8rem;
  }
  .btn:disabled,
  .update:disabled {
    opacity: 0.4;
    border-color: var(--border);
  }
  .update:not(:disabled) {
    background: var(--accent);
    color: var(--bg);
    font-weight: 700;
  }
  footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    padding: 0.5rem 0.7rem;
    border-top: 1px solid var(--border);
  }
  .status {
    font-size: 0.72rem;
    color: var(--text-faint);
  }
</style>
