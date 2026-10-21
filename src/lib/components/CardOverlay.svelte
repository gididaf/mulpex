<script lang="ts">
  // One card, full: it covers the Board tab (back / Esc returns). Title, column,
  // assignee, detail, choices, the comment thread and a composer that takes
  // pasted, dropped and picked images. Every change lands on the board at once
  // and waits for UPDATE (the Board footer) to reach the card's claude.
  //
  // Titles are plain text. Details and comments are Markdown, rendered only
  // through `renderMarkdownAuto` (DOMPurify; links and images dropped) — they come
  // from a model and the user, and this webview can invoke Tauri commands.
  import { onDestroy, onMount, tick } from "svelte";
  import { confirm } from "@tauri-apps/plugin-dialog";
  import type { Board, BoardCard, SessionInfo } from "../ipc";
  import { editBoard, uploadImage, importImages, pickImages, setDropTarget, MAX_IMAGES, IMAGE_TYPES } from "../board";
  import { flashToast } from "../stores";
  import { addr, addrIn } from "../agents";
  import { renderMarkdownAuto } from "../markdown";
  import ImageStrip from "./ImageStrip.svelte";

  let {
    handle,
    board,
    card,
    claudes,
    onclose,
  }: { handle: number; board: Board; card: BoardCard; claudes: SessionInfo[]; onclose: () => void } = $props();

  // ── what's unsent survives closing the card (per card, per app run) ─────────
  const key = $derived(`${handle}:${card.n}`);
  let text = $state("");
  let pending: string[] = $state([]);
  let busy = $state(0);
  onMount(() => {
    const d = drafts.get(key);
    if (d) {
      text = d.text;
      pending = d.images;
    }
  });
  onDestroy(() => {
    if (text.trim() || pending.length) drafts.set(key, { text, images: pending });
    else drafts.delete(key);
    setDropTarget(null);
  });

  // Opening a card reads it; while it is open, whatever a claude adds is read too.
  $effect(() => {
    if (card.unread) void editBoard(handle, { op: "seen", n: card.n });
  });

  // ── title / detail ──────────────────────────────────────────────────────────
  let editingDetail = $state(false);
  let detailDraft = $state("");

  function saveTitle(el: HTMLInputElement) {
    const t = el.value.trim();
    if (t && t !== card.title) void editBoard(handle, { op: "edit", n: card.n, title: t });
    else el.value = card.title;
  }

  async function startDetail() {
    detailDraft = card.detail ?? "";
    editingDetail = true;
    await tick();
    detailEl?.focus();
  }
  let detailEl: HTMLTextAreaElement | undefined = $state();

  async function saveDetail() {
    if (detailDraft.trim() !== (card.detail ?? "").trim()) {
      if (!(await editBoard(handle, { op: "edit", n: card.n, detail: detailDraft }))) return;
    }
    editingDetail = false;
  }

  // ── images ──────────────────────────────────────────────────────────────────
  async function attach(get: () => Promise<string[]>, toCard = false) {
    busy++;
    try {
      const ids = await get();
      if (!ids.length) return;
      if (toCard) {
        const all = [...card.images, ...ids];
        if (all.length > MAX_IMAGES) throw new Error(`A card can hold at most ${MAX_IMAGES} images`);
        await editBoard(handle, { op: "edit", n: card.n, images: all });
      } else {
        if (pending.length + ids.length > MAX_IMAGES) throw new Error(`At most ${MAX_IMAGES} images per comment`);
        pending = [...pending, ...ids];
      }
    } catch (e) {
      flashToast(e instanceof Error ? e.message : String(e));
    } finally {
      busy--;
    }
  }

  function onpaste(e: ClipboardEvent) {
    const files = [...(e.clipboardData?.files ?? [])].filter((f) => IMAGE_TYPES.includes(f.type));
    if (!files.length) return;
    e.preventDefault();
    void attach(() => Promise.all(files.map((f) => uploadImage(handle, f))));
  }

  // A file dropped on the window lands in this composer while the card is open.
  let composerEl: HTMLElement | undefined = $state();
  $effect(() => {
    if (composerEl) setDropTarget({ el: composerEl, accept: (paths) => void attach(() => importImages(handle, paths)) });
  });

  // ── comments ────────────────────────────────────────────────────────────────
  async function send() {
    if ((!text.trim() && !pending.length) || busy) return;
    const ok = await editBoard(handle, { op: "comment", n: card.n, text, images: [...pending] });
    if (ok) {
      text = "";
      pending = [];
      drafts.delete(key);
    }
  }

  function onComposerKey(e: KeyboardEvent) {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      void send();
    }
  }

  function onkey(e: KeyboardEvent) {
    if (e.key !== "Escape" || e.defaultPrevented) return;
    const t = e.target as HTMLElement;
    // Esc in a field leaves the field first; a second Esc closes the card.
    if (t.matches?.("textarea, input, select")) {
      if (editingDetail && t === detailEl) editingDetail = false;
      t.blur();
    } else {
      onclose();
    }
    e.preventDefault();
  }

  const colName = (id: number) => board.columns.find((c) => c.id === id)?.name ?? "?";
  const who = (c: { by: string; who: number | null }) => (c.by === "user" ? "you" : c.who != null ? addrIn(claudes, c.who) : "claude");
  const assigneeGone = $derived(card.assignee != null && !claudes.some((s) => s.id === card.assignee));

  // ── editing an unsent comment of the user's ─────────────────────────────────
  let editing: { id: string; text: string; images: string[] } | null = $state(null);
  async function saveEdit() {
    if (!editing) return;
    const e = editing;
    if (!e.text.trim() && !e.images.length) {
      if (await editBoard(handle, { op: "comment_delete", n: card.n, id: e.id })) editing = null;
      return;
    }
    if (await editBoard(handle, { op: "comment_edit", n: card.n, id: e.id, text: e.text, images: [...e.images] })) editing = null;
  }
  // Sent by UPDATE (here or on the phone) while being edited: the edit is moot.
  $effect(() => {
    if (editing && !board.unsent.includes(editing.id)) editing = null;
  });

  function choose(c: string) {
    void editBoard(handle, { op: "choose", n: card.n, choice: card.chosen === c ? null : c });
  }

  async function del() {
    if (await confirm(`Delete #${card.n} "${card.title}" for good? Its comments and images go too.`, { title: "Delete card", kind: "warning" })) {
      if (await editBoard(handle, { op: "delete", n: card.n })) onclose();
    }
  }
</script>

<script module lang="ts">
  /** Unsent comment text and images, by `"<handle>:<n>"`. */
  const drafts = new Map<string, { text: string; images: string[] }>();
</script>

<svelte:window onkeydown={onkey} />

<div class="card" role="dialog" aria-label="Card #{card.n}">
  <div class="top">
    <button class="back" title="Back to the board (Esc)" onclick={onclose}>← Board</button>
    <span class="num">#{card.n}</span>
    {#if card.urgent}<span class="urgent-tag" title="A claude needs your attention on this as soon as possible">URGENT</span>{/if}
    <span class="grow"></span>
    {#if card.archived}
      <button class="btn-s" onclick={() => editBoard(handle, { op: "archive", n: card.n, on: false })}>Restore</button>
    {:else}
      <button class="btn-s" title="Hide it from the board and from every claude" onclick={async () => { if (await editBoard(handle, { op: "archive", n: card.n, on: true })) onclose(); }}>Archive</button>
    {/if}
    <button class="btn-s danger" title="Delete for good" onclick={del}>Delete</button>
  </div>

  <div class="scroll">
    <input class="title" dir="auto" value={card.title} onkeydown={(e) => { if (e.key === "Enter") (e.currentTarget as HTMLInputElement).blur(); }} onblur={(e) => saveTitle(e.currentTarget as HTMLInputElement)} />

    <div class="fields">
      <label>
        <span>Column</span>
        <select value={card.col} onchange={(e) => editBoard(handle, { op: "move", n: card.n, col: Number((e.currentTarget as HTMLSelectElement).value), before: null })}>
          {#each board.columns as c (c.id)}<option value={c.id}>{c.name}</option>{/each}
        </select>
      </label>
      <label>
        <span>Assignee</span>
        <select value={card.assignee ?? ""} onchange={(e) => { const v = (e.currentTarget as HTMLSelectElement).value; editBoard(handle, { op: "assign", n: card.n, to: v === "" ? null : Number(v) }); }}>
          <option value="">Nobody</option>
          {#each claudes as s (s.id)}<option value={s.id}>{addr(s.kind, s.id)}{s.name ? ` · ${s.name}` : ""}</option>{/each}
          {#if assigneeGone}<option value={card.assignee}>#{card.assignee} (closed)</option>{/if}
        </select>
      </label>
    </div>
    <div class="meta">
      {card.author === "user" ? "Added by you" : `Added by ${card.by != null ? addrIn(claudes, card.by) : "?"}`} · in {colName(card.col)}{card.archived ? " · archived" : ""}
    </div>

    <section>
      <div class="sec-head">
        <span>Detail</span>
        {#if !editingDetail}<button class="link" onclick={startDetail}>{card.detail ? "Edit" : "Add"}</button>{/if}
        <button class="link" title="Attach images to the card" disabled={busy > 0} onclick={() => attach(() => pickImages(handle), true)}>📎</button>
      </div>
      {#if editingDetail}
        <textarea bind:this={detailEl} dir="auto" rows="6" bind:value={detailDraft} placeholder="Markdown"
          onkeydown={(e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); void saveDetail(); } }}></textarea>
        <div class="row-end">
          <button class="link" onclick={() => (editingDetail = false)}>Cancel</button>
          <button class="btn" onclick={saveDetail}>Save</button>
        </div>
      {:else if card.detail}
        <!-- Sanitized in renderMarkdownAuto. -->
        <div class="detail md">{@html renderMarkdownAuto(card.detail)}</div>
      {:else}
        <div class="none">No detail.</div>
      {/if}
      <ImageStrip {handle} ids={card.images} onremove={(id) => editBoard(handle, { op: "edit", n: card.n, images: card.images.filter((i) => i !== id) })} />
    </section>

    {#if card.choices.length}
      <section>
        <div class="sec-head"><span>Choose</span></div>
        <div class="choices">
          {#each card.choices as c (c)}
            <button class="chip" class:on={card.chosen === c} dir="auto" onclick={() => choose(c)}>{c}</button>
          {/each}
        </div>
      </section>
    {:else if card.chosen}
      <div class="meta" dir="auto">chose: {card.chosen}</div>
    {/if}

    <section>
      <div class="sec-head"><span>Comments{card.comments.length ? ` · ${card.comments.length}` : ""}</span></div>
      {#each card.comments as c, ci (c.id || ci)}
        {@const mine = c.by === "user" && !!c.id && board.unsent.includes(c.id)}
        <div class="comment" class:by-claude={c.by === "claude"} class:unsent={mine}>
          <span class="who">
            {who(c)}{#if mine}<span class="unsent-tag" title="Not sent yet — you can still change it">not sent</span>{/if}
            {#if mine && editing?.id !== c.id}
              <span class="ctools">
                <button class="link" onclick={() => (editing = { id: c.id, text: c.text, images: [...c.images] })}>Edit</button>
                <button class="link" onclick={() => editBoard(handle, { op: "comment_delete", n: card.n, id: c.id })}>Delete</button>
              </span>
            {/if}
          </span>
          {#if editing?.id === c.id}
            <!-- svelte-ignore a11y_autofocus -->
            <textarea dir="auto" rows="3" bind:value={editing.text} autofocus
              onkeydown={(e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); void saveEdit(); } }}></textarea>
            <ImageStrip {handle} ids={editing.images} small onremove={(id) => editing && (editing.images = editing.images.filter((i) => i !== id))} />
            <div class="row-end">
              <button class="link" onclick={() => (editing = null)}>Cancel</button>
              <button class="btn" onclick={saveEdit}>Save</button>
            </div>
          {:else}
            <!-- Sanitized in renderMarkdownAuto. -->
            {#if c.text}<div class="text md">{@html renderMarkdownAuto(c.text)}</div>{/if}
            <ImageStrip {handle} ids={c.images} small />
          {/if}
        </div>
      {/each}
    </section>
  </div>

  <div class="composer" bind:this={composerEl}>
    <textarea
      dir="auto"
      rows="2"
      placeholder={card.assignee != null ? `Comment to ${addrIn(claudes, card.assignee)}… paste or drop images (⌘↩ adds; UPDATE tells it)` : "Comment… (no assignee: nobody is told)"}
      bind:value={text}
      onpaste={onpaste}
      onkeydown={onComposerKey}
    ></textarea>
    <ImageStrip {handle} ids={pending} small onremove={(id) => (pending = pending.filter((p) => p !== id))} />
    <div class="row-end">
      {#if busy}<span class="busy">attaching…</span>{/if}
      <button class="link" title="Attach images" disabled={busy > 0} onclick={() => attach(() => pickImages(handle))}>📎 Image</button>
      <button class="btn" disabled={busy > 0 || (!text.trim() && !pending.length)} onclick={send}>Comment</button>
    </div>
  </div>
</div>

<style>
  .card {
    position: absolute;
    inset: 0;
    z-index: 5;
    display: flex;
    flex-direction: column;
    background: var(--bg-elev);
  }
  .top {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.3rem 0.6rem;
    border-bottom: 1px solid var(--border);
    min-height: 1.9rem;
  }
  .back {
    border: none;
    background: none;
    color: var(--text-dim);
    font-size: 0.78rem;
    padding: 0.1rem 0.3rem;
    border-radius: 4px;
  }
  .back:hover {
    background: var(--border);
    color: var(--text);
  }
  .num {
    color: var(--text-faint);
    font-size: 0.75rem;
  }
  .grow {
    flex: 1;
  }
  .urgent-tag {
    padding: 0 0.3rem;
    border-radius: 3px;
    background: var(--dot-needs, #e5534b);
    color: #fff;
    font-size: 0.6rem;
    font-weight: 700;
    letter-spacing: 0.03em;
    line-height: 1rem;
  }
  .btn-s {
    border: 1px solid var(--border);
    background: none;
    color: var(--text-dim);
    font-size: 0.7rem;
    padding: 0.1rem 0.45rem;
    border-radius: 4px;
  }
  .btn-s:hover {
    color: var(--text);
    border-color: var(--border-focus);
  }
  .btn-s.danger:hover {
    color: var(--dot-needs, #e5534b);
    border-color: var(--dot-needs, #e5534b);
  }
  .scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 0.5rem 0.8rem 0.8rem;
  }
  .title {
    width: 100%;
    border: 1px solid transparent;
    border-radius: 5px;
    background: none;
    color: var(--text);
    font-size: 0.95rem;
    font-weight: 600;
    padding: 0.2rem 0.3rem;
    margin-left: -0.3rem;
    unicode-bidi: plaintext;
  }
  .title:hover,
  .title:focus {
    border-color: var(--border);
    background: var(--bg);
    outline: none;
  }
  .fields {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.4rem;
  }
  .fields label {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    font-size: 0.66rem;
    color: var(--text-faint);
  }
  .meta {
    margin-top: 0.35rem;
    font-size: 0.68rem;
    color: var(--text-faint);
  }
  section {
    margin-top: 0.8rem;
  }
  .sec-head {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    font-size: 0.7rem;
    font-weight: 600;
    color: var(--text-dim);
    text-transform: uppercase;
    letter-spacing: 0.03em;
    margin-bottom: 0.25rem;
  }
  .sec-head span {
    flex: 1;
  }
  .none {
    font-size: 0.75rem;
    color: var(--text-faint);
  }
  .detail,
  .comment .text {
    font-size: 0.78rem;
    line-height: 1.45;
    color: var(--text-dim);
    overflow-wrap: anywhere;
  }
  .choices {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
  }
  .chip {
    border: 1px solid var(--border);
    background: none;
    color: var(--text-dim);
    font-size: 0.75rem;
    padding: 0.15rem 0.55rem;
    border-radius: 0.8rem;
    unicode-bidi: plaintext;
  }
  .chip.on {
    border-color: var(--accent);
    background: color-mix(in srgb, var(--accent) 18%, transparent);
    color: var(--text);
  }
  .comment {
    padding: 0.35rem 0.5rem;
    margin-bottom: 0.35rem;
    border-radius: 5px;
    border-inline-start: 2px solid var(--border);
    background: color-mix(in srgb, var(--text) 4%, transparent);
  }
  .comment.unsent {
    border-inline-start-color: var(--accent);
  }
  .unsent-tag {
    margin-inline-start: 0.35rem;
    padding: 0 0.25rem;
    border-radius: 3px;
    border: 1px solid var(--accent);
    color: var(--accent);
    font-size: 0.58rem;
  }
  .ctools {
    float: inline-end;
  }
  .ctools .link {
    font-size: 0.66rem;
  }
  .comment.by-claude {
    border-inline-start-color: var(--dot-working);
  }
  .comment .who {
    display: block;
    font-size: 0.66rem;
    color: var(--text-faint);
  }
  .composer {
    border-top: 1px solid var(--border);
    padding: 0.4rem 0.6rem 0.5rem;
  }
  .row-end {
    display: flex;
    justify-content: flex-end;
    align-items: center;
    gap: 0.4rem;
    margin-top: 0.3rem;
  }
  .busy {
    font-size: 0.7rem;
    color: var(--dot-working);
    margin-right: auto;
  }
  .link {
    border: none;
    background: none;
    color: var(--text-dim);
    font-size: 0.72rem;
    padding: 0 0.25rem;
    border-radius: 4px;
    cursor: pointer;
    text-transform: none;
    letter-spacing: 0;
  }
  .link:hover {
    color: var(--text);
  }
  .btn {
    border: none;
    border-radius: 5px;
    background: var(--accent);
    color: var(--bg);
    font-size: 0.75rem;
    font-weight: 600;
    padding: 0.2rem 0.7rem;
  }
  .btn:disabled {
    opacity: 0.45;
  }
  textarea,
  select {
    width: 100%;
    padding: 0.3rem 0.45rem;
    border: 1px solid var(--border);
    border-radius: 5px;
    background: var(--bg);
    color: var(--text);
    font: inherit;
    font-size: 0.78rem;
  }
  select {
    padding: 0.15rem 0.3rem;
    font-size: 0.75rem;
  }
  textarea {
    resize: vertical;
    unicode-bidi: plaintext;
  }
  textarea:focus,
  select:focus {
    outline: none;
    border-color: var(--border-focus);
  }
  /* Markdown in details and comments. Each block finds its own direction
     (Hebrew and English cards sit side by side); code is always LTR. */
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
</style>
