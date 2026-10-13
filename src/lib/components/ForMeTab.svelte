<script lang="ts">
  // The side panel's "For me" tab: what each claude needs from the user, as the
  // claude wrote it with `hub_user_asks` (`asks.ts`, `src-tauri/src/asks.rs`).
  //
  // Everything the user does here — check, note, pick a choice, dismiss, add —
  // is a DRAFT the claude cannot see, marked "pending", until UPDATE sends each
  // claude a message listing exactly what changed. The drafts live on the
  // backend, so the phone and a remote desktop show the same ones.
  //
  // Plain text only: titles, details and notes come from a model and the user,
  // so nothing here goes through {@html}.
  import { activeProjectHandle, activeId, sessions } from "../stores";
  import { asks, askKey, asksAll, editAsk, updateAsks, openCount, pendingCount } from "../asks";
  import type { AskDraft, AskItem, AskKind, AskList } from "../ipc";
  import { terminals } from "../terminals";

  const h = $derived($activeProjectHandle);
  const id = $derived($activeId);
  const kind = $derived(id != null ? $sessions.find((s) => s.id === id)?.kind : undefined);

  /** The claudes to show, in sidebar order: the focused one, or all of them. */
  const groups = $derived.by(() => {
    if (h == null) return [] as { id: number; name: string | null; list: AskList | undefined }[];
    const claudes = $sessions.filter((s) => s.kind === "claude");
    const pick = $asksAll ? claudes : claudes.filter((s) => s.id === id);
    return pick
      .map((s) => ({ id: s.id, name: s.name, list: $asks.get(askKey(h, s.id)) }))
      .filter((g) => !$asksAll || (g.list && (g.list.items.length || pendingCount(g.list))));
  });

  const sendable = $derived(groups.filter((g) => g.list && pendingCount(g.list) && !g.list.sending));
  const sending = $derived(groups.some((g) => g.list?.sending));
  const pendingTotal = $derived(sendable.reduce((n, g) => n + pendingCount(g.list), 0));
  const doneCount = $derived(
    groups.reduce((n, g) => n + (g.list?.items.filter((i) => i.status === "done" && !g.list?.drafts.edits[i.n]).length ?? 0), 0),
  );

  const KIND_ICON: Record<AskKind, string> = { action: "▶", decision: "◆", question: "?" };
  const KIND_NAME: Record<AskKind, string> = { action: "Action", decision: "Decision", question: "Question" };

  /** Expanded details and open note editors, by `"<id>:<n>"`. Per view only. */
  let expanded = $state(new Set<string>());
  let noting = $state(new Set<string>());
  const toggle = (set: Set<string>, k: string) => {
    const next = new Set(set);
    if (next.has(k)) next.delete(k);
    else next.add(k);
    return next;
  };

  const draftOf = (l: AskList | undefined, n: number): AskDraft | undefined => l?.drafts.edits[String(n)];
  const isDone = (i: AskItem, d: AskDraft | undefined) => d?.done ?? i.status === "done";
  const chosenOf = (i: AskItem, d: AskDraft | undefined) => d?.chosen ?? i.chosen;
  const noteOf = (i: AskItem, d: AskDraft | undefined) => d?.note ?? i.note ?? "";
  const orphans = (l: AskList | undefined) =>
    l ? Object.entries(l.drafts.edits).filter(([, d]) => d.orphan).map(([n, d]) => ({ n: Number(n), d })) : [];

  function edit(cid: number, e: Parameters<typeof editAsk>[2]) {
    if (h != null) editAsk(h, cid, e);
  }

  function saveNote(cid: number, n: number, value: string) {
    edit(cid, { op: "note", n, note: value });
  }

  function choose(cid: number, i: AskItem, d: AskDraft | undefined, c: string) {
    // Clicking the picked choice again un-picks the draft.
    edit(cid, { op: "choose", n: i.n, choice: chosenOf(i, d) === c && d?.chosen ? null : c });
  }

  // ── add ────────────────────────────────────────────────────────────────────
  let addKind: AskKind = $state("action");
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

  function onNoteKey(e: KeyboardEvent, cid: number, n: number) {
    const el = e.currentTarget as HTMLTextAreaElement;
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      saveNote(cid, n, el.value);
      noting = toggle(noting, `${cid}:${n}`);
    } else if (e.key === "Escape") {
      e.preventDefault();
      noting = toggle(noting, `${cid}:${n}`);
    }
  }

  function update() {
    if (h == null) return;
    updateAsks(h, sendable.map((g) => g.id));
    terminals.refocus();
  }

  function clearDone() {
    for (const g of groups) {
      if (g.list?.items.some((i) => i.status === "done")) edit(g.id, { op: "clear_done" });
    }
  }
</script>

<div class="tab">
  <div class="bar">
    <div class="seg" role="radiogroup" aria-label="Show">
      <button class:on={!$asksAll} onclick={() => asksAll.set(false)}>{id != null ? `${kind === "shell" ? "term" : "claude"}#${id}` : "This one"}</button>
      <button class:on={$asksAll} onclick={() => asksAll.set(true)}>All</button>
    </div>
    {#if doneCount}
      <button class="link" title="Remove items that are done" onclick={clearDone}>Clear done</button>
    {/if}
  </div>

  <div class="list">
    {#if !$asksAll && kind === "shell"}
      <div class="empty">For me works on claudes only.</div>
    {:else if !$asksAll && id == null}
      <div class="empty">No instance selected.</div>
    {:else if !groups.length || groups.every((g) => !g.list || (!g.list.items.length && !pendingCount(g.list)))}
      <div class="empty">
        {#if $asksAll}
          Nothing open for you in this project.
        {:else}
          Nothing open for you from claude#{id}.<br />When it needs something from you, it lands here.
        {/if}
      </div>
    {/if}

    {#each groups as g (g.id)}
      {#if $asksAll}
        <div class="group">
          claude#{g.id}{#if g.name}<span class="gname"> · {g.name}</span>{/if}
          {#if openCount(g.list)}<span class="gcount">{openCount(g.list)}</span>{/if}
          {#if g.list?.sending}<span class="sending">sending…</span>{/if}
        </div>
      {/if}
      {#each g.list?.items ?? [] as i (i.n)}
        {@const d = draftOf(g.list, i.n)}
        {@const k = `${g.id}:${i.n}`}
        {@const done = isDone(i, d)}
        <div class="item" class:done class:gone={d?.delete} class:pending={!!d}>
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
            </div>
            <div class="meta">
              <span>#{i.n}</span>
              {#if i.author === "user"}<span>· yours</span>{/if}
              {#if i.status === "done" && i.done_by}<span>· done by {i.done_by === "user" ? "you" : "claude"}</span>{/if}
              {#if d}<span class="pend">· pending</span>{/if}
              {#if d && i.rev > d.base_rev}<span class="warn" title="The claude changed this item after you started editing it">· changed since your edit</span>{/if}
              {#if i.detail}
                <button class="link" onclick={() => (expanded = toggle(expanded, k))}>{expanded.has(k) ? "less" : "more"}</button>
              {/if}
            </div>
            {#if i.detail && expanded.has(k)}
              <div class="detail" dir="auto">{i.detail}</div>
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
            {#if noting.has(k)}
              <!-- svelte-ignore a11y_autofocus -->
              <textarea
                dir="auto"
                rows="2"
                placeholder="Your answer or note… (⌘↩ to keep)"
                value={noteOf(i, d)}
                autofocus
                onchange={(e) => saveNote(g.id, i.n, (e.currentTarget as HTMLTextAreaElement).value)}
                onkeydown={(e) => onNoteKey(e, g.id, i.n)}
              ></textarea>
            {:else if noteOf(i, d)}
              <button class="note-view" dir="auto" title="Edit note" onclick={() => (noting = toggle(noting, k))}>{noteOf(i, d)}</button>
            {/if}
          </div>
          <div class="tools">
            {#if !noting.has(k) && !d?.delete}
              <button title="Add a note / answer" onclick={() => (noting = toggle(noting, k))}>✎</button>
            {/if}
            {#if d}
              <button title="Discard your pending edit" onclick={() => edit(g.id, { op: "discard", n: i.n })}>↺</button>
            {/if}
            {#if !d?.delete}
              <button title="Dismiss — remove it at UPDATE" onclick={() => edit(g.id, { op: "dismiss", n: i.n, on: true })}>✕</button>
            {/if}
          </div>
        </div>
      {/each}
      {#each orphans(g.list) as o (o.n)}
        <div class="item gone-by-claude pending">
          <span class="kind k-{o.d.orphan?.kind}">{o.d.orphan ? KIND_ICON[o.d.orphan.kind] : ""}</span>
          <div class="main">
            <div class="head"><span class="title" dir="auto">{o.d.orphan?.title}</span></div>
            <div class="meta"><span>#{o.n} · removed by claude · your note is still sent</span></div>
            {#if o.d.note}<div class="note-view" dir="auto">{o.d.note}</div>{/if}
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
            {#if a.detail}<div class="detail" dir="auto">{a.detail}</div>{/if}
          </div>
          <div class="tools">
            <button title="Remove" onclick={() => edit(g.id, { op: "remove_added", tmp: a.tmp })}>✕</button>
          </div>
        </div>
      {/each}
    {/each}

    {#if !$asksAll && kind === "claude" && id != null}
      {#if addOpen}
        <div class="add">
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
  .meta {
    display: flex;
    flex-wrap: wrap;
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
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    unicode-bidi: plaintext;
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
    margin: 0.5rem 0.8rem;
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
  .add {
    margin: 0.5rem 0.8rem;
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
