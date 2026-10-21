<script lang="ts">
  // The side panel's Board tab: the project's board (`board.ts`,
  // `src-tauri/src/board.rs`) — the user's columns side by side, cards assigned
  // to claudes. Replaced the per-claude "To Do" list.
  //
  // Every edit lands on the board at once, and waits for UPDATE (the footer)
  // to be told to the card's claude — one message per claude, so a round of
  // QA replies goes out together. Cards with unsent changes are marked. Columns are the user's alone: rename by clicking
  // the name, ⋯ for the description (what claudes read), move and delete; `+`
  // at the end adds one. Cards and column headers are dragged with POINTER
  // events — Tauri's native drag-drop swallows HTML5 DnD (`InstanceList.svelte`).
  import { tick } from "svelte";
  import { activeProjectHandle, activeId, sessions } from "../stores";
  import { addrIn, isAgent } from "../agents";
  import { boards, boardAll, editBoard, setBoardOn, sendBoard, imageCount, unreadCount, pendingCards } from "../board";
  import type { Board, BoardCard, BoardColumn, BoardColChange, BoardColRef } from "../ipc";
  import { terminals } from "../terminals";
  import CardOverlay from "./CardOverlay.svelte";

  const h = $derived($activeProjectHandle);
  const id = $derived($activeId);
  const board = $derived(h != null ? $boards.get(h) : undefined);
  const claudes = $derived($sessions.filter((s) => isAgent(s.kind)));
  const at = (n: number) => addrIn($sessions, n);
  const focusedClaude = $derived(id != null && claudes.some((s) => s.id === id) ? id : null);

  let showArchived = $state(false);
  let openN: number | null = $state(null);
  const openCard = $derived(openN != null ? board?.cards.find((c) => c.n === openN) : undefined);
  $effect(() => {
    // The card was deleted (here, on the phone, by another desktop).
    if (openN != null && board && !openCard) openN = null;
  });

  /** Cards the view shows in column `col`, in board order. */
  function cardsIn(b: Board, col: number): BoardCard[] {
    return b.cards.filter(
      (c) => c.col === col && !c.archived && ($boardAll || c.assignee == null || c.assignee === focusedClaude),
    );
  }
  const archived = $derived(board?.cards.filter((c) => c.archived) ?? []);

  const edit = (e: Parameters<typeof editBoard>[1]) => (h != null ? editBoard(h, e) : Promise.resolve(false));

  // ── claudes' column proposals ──────────────────────────────────────────────
  // Nothing changes until Approve; the claude hears the answer at once.
  let rejecting: number | null = $state(null);
  let rejectWhy = $state("");

  const refName = (b: Board, r: BoardColRef | null) =>
    r == null ? null : "id" in r ? (b.columns.find((c) => c.id === r.id)?.name ?? "(a deleted column)") : r.new;

  /** One change, said plainly: a line, an optional second line, and what it replaces. */
  function said(b: Board, ch: BoardColChange): { icon: string; line: string; sub?: string; was?: string } {
    const q = (s: string | null) => `“${s}”`;
    switch (ch.op) {
      case "add":
        return { icon: "＋", line: `Add ${q(ch.name)} ${ch.before ? `before ${q(refName(b, ch.before))}` : "at the end"}`, sub: ch.desc || undefined };
      case "edit": {
        const old = "id" in ch.col ? b.columns.find((c) => "id" in ch.col && c.id === ch.col.id) : undefined;
        const line = ch.name != null ? `Rename ${q(refName(b, ch.col))} → ${q(ch.name)}` : `Describe ${q(refName(b, ch.col))}`;
        return { icon: "✎", line, sub: ch.desc != null ? ch.desc || "(no description)" : undefined, was: ch.desc != null ? old?.desc || undefined : undefined };
      }
      case "move":
        return { icon: "↔", line: `Move ${q(refName(b, ch.col))} ${ch.before ? `before ${q(refName(b, ch.before))}` : "to the end"}` };
      case "delete": {
        const n = "id" in ch.col ? b.cards.filter((c) => "id" in ch.col && c.col === ch.col.id && !c.archived).length : 0;
        const one = n === 1;
        const cards = n ? `its ${n} card${one ? "" : "s"} ${ch.to ? `move${one ? "s" : ""} to ${q(refName(b, ch.to))}` : `${one ? "is" : "are"} archived`}` : "it has no cards";
        return { icon: "✕", line: `Delete ${q(refName(b, ch.col))}`, sub: cards };
      }
    }
  }

  async function reject(id: number) {
    if (await edit({ op: "reject", proposal: id, reason: rejectWhy })) {
      rejecting = null;
      rejectWhy = "";
    }
  }

  // ── adding cards ───────────────────────────────────────────────────────────
  let addingIn: number | null = $state(null);
  let addTitle = $state("");
  let addAssignee: string = $state("");

  async function startAdd(col: number) {
    addingIn = col;
    addTitle = "";
    addAssignee = focusedClaude != null ? String(focusedClaude) : "";
    await tick();
    document.querySelector<HTMLInputElement>(".add-card input")?.focus();
  }

  async function add() {
    if (addingIn == null || !addTitle.trim()) return;
    const ok = await edit({
      op: "add",
      title: addTitle,
      col: addingIn,
      assignee: addAssignee === "" ? null : Number(addAssignee),
    });
    if (ok) addTitle = "";
  }

  function onAddKey(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.isComposing) {
      e.preventDefault();
      void add();
    } else if (e.key === "Escape") {
      e.preventDefault();
      addingIn = null;
      terminals.refocus();
    }
  }

  // ── columns ────────────────────────────────────────────────────────────────
  let renaming: number | null = $state(null);
  let menuFor: number | null = $state(null);
  let describing: number | null = $state(null);
  let descDraft = $state("");
  let newCol: string | null = $state(null);
  let deleting: { col: BoardColumn; count: number; to: string } | null = $state(null);

  function rename(c: BoardColumn, el: HTMLInputElement) {
    const name = el.value.trim();
    renaming = null;
    if (name && name !== c.name) void edit({ op: "col_edit", col: c.id, name });
  }

  function moveCol(b: Board, c: BoardColumn, dir: -1 | 1) {
    const i = b.columns.findIndex((x) => x.id === c.id);
    const j = i + dir;
    if (j < 0 || j >= b.columns.length) return;
    // `before` the column now at j (moving left), or the one after j (moving right).
    const before = dir < 0 ? b.columns[j].id : (b.columns[j + 1]?.id ?? null);
    void edit({ op: "col_move", col: c.id, before });
  }

  function askDelete(b: Board, c: BoardColumn) {
    menuFor = null;
    const count = b.cards.filter((x) => x.col === c.id).length;
    if (!count) {
      void edit({ op: "col_delete", col: c.id, to: null });
      return;
    }
    const other = b.columns.find((x) => x.id !== c.id);
    deleting = { col: c, count, to: other ? String(other.id) : "" };
  }

  async function addColumn() {
    const name = (newCol ?? "").trim();
    if (!name) {
      newCol = null;
      return;
    }
    if (await edit({ op: "col_add", name })) newCol = null;
  }

  // ── dragging cards (both axes) and column headers (x only) ──────────────────
  type Drag = {
    kind: "card" | "col";
    n: number;
    title: string;
    x0: number;
    y0: number;
    x: number;
    y: number;
    live: boolean;
    /** Card drag: target column and the card it goes before (null = end). */
    col: number | null;
    before: number | null;
    el: HTMLElement;
    pointer: number;
  };
  let drag: Drag | null = $state(null);
  let suppressClick = false;
  let colsEl: HTMLElement | undefined = $state();

  function down(e: PointerEvent, kind: "card" | "col", n: number, title: string) {
    if (e.button !== 0) return;
    const t = e.target as HTMLElement;
    if (kind === "col" && t.closest(".dots, .menu, input, textarea")) return;
    // Captured only once it really is a drag: capturing on press would retarget
    // the click, and the column name's rename button would never get it.
    const el = e.currentTarget as HTMLElement;
    drag = { kind, n, title, x0: e.clientX, y0: e.clientY, x: e.clientX, y: e.clientY, live: false, col: null, before: null, el, pointer: e.pointerId };
  }

  function target(x: number, y: number) {
    if (!drag || !colsEl) return;
    const cols = [...colsEl.querySelectorAll<HTMLElement>("[data-col]")];
    const over =
      cols.find((c) => {
        const r = c.getBoundingClientRect();
        return x >= r.left && x <= r.right;
      }) ?? (x < (cols[0]?.getBoundingClientRect().left ?? 0) ? cols[0] : cols[cols.length - 1]);
    if (!over) return;
    const col = Number(over.dataset.col);
    if (drag.kind === "col") {
      // Before the column whose midpoint is right of the pointer.
      const next = cols.find((c) => {
        const r = c.getBoundingClientRect();
        return x < r.left + r.width / 2;
      });
      drag.col = next ? Number(next.dataset.col) : null;
      return;
    }
    const cards = [...over.querySelectorAll<HTMLElement>("[data-card]")].filter((c) => Number(c.dataset.card) !== drag!.n);
    const next = cards.find((c) => {
      const r = c.getBoundingClientRect();
      return y < r.top + r.height / 2;
    });
    drag.col = col;
    drag.before = next ? Number(next.dataset.card) : null;
  }

  function move(e: PointerEvent) {
    if (!drag) return;
    drag.x = e.clientX;
    drag.y = e.clientY;
    if (!drag.live && Math.hypot(e.clientX - drag.x0, e.clientY - drag.y0) > 4) {
      drag.live = true;
      try {
        drag.el.setPointerCapture(drag.pointer);
      } catch {
        /* the pointer is already gone */
      }
    }
    if (drag.live) target(e.clientX, e.clientY);
  }

  function up() {
    const d = drag;
    drag = null;
    if (!d?.live || !board) return;
    suppressClick = true;
    setTimeout(() => (suppressClick = false), 0);
    if (d.kind === "card" && d.col != null) {
      const card = board.cards.find((c) => c.n === d.n);
      if (!card) return;
      // Dropped back where it was: nothing to send.
      const list = cardsIn(board, d.col);
      const i = list.findIndex((c) => c.n === d.n);
      if (card.col === d.col && i >= 0 && (list[i + 1]?.n ?? null) === d.before) return;
      void edit({ op: "move", n: d.n, col: d.col, before: d.before });
    } else if (d.kind === "col" && d.col !== d.n) {
      const i = board.columns.findIndex((c) => c.id === d.n);
      if ((board.columns[i + 1]?.id ?? null) === d.col) return;
      void edit({ op: "col_move", col: d.n, before: d.col });
    }
  }

  function open(c: BoardCard) {
    if (suppressClick) return;
    openN = c.n;
  }

  const assigneeLabel = (a: number | null) => (a == null ? "" : at(a));
  const gone = (a: number | null) => a != null && !claudes.some((s) => s.id === a);
  const sending = $derived(board?.sending.length ?? 0);
  const pendingSet = $derived(pendingCards(board));
  const pendingClaudes = $derived(Object.keys(board?.pending ?? {}).map(Number));
  let updating = $state(false);
  async function update() {
    if (h == null || updating) return;
    updating = true;
    await sendBoard(h);
    updating = false;
    terminals.refocus();
  }

  // No text selection while dragging (the drag would paint a selection across
  // every column it passes).
  $effect(() => {
    const on = !!drag?.live;
    document.body.classList.toggle("no-select", on);
    return () => document.body.classList.remove("no-select");
  });
</script>

<svelte:window
  onpointerdown={(e) => { if (menuFor != null && !(e.target as HTMLElement).closest(".menu, .dots")) menuFor = null; }}
  onpointerup={() => { if (drag && !drag.live) drag = null; }}
/>

<div class="tab">
  {#if h == null}
    <div class="empty">No project open.</div>
  {:else if !board || !board.on}
    <div class="empty">
      The Board is off for this project.
      <p>Turn it on and every claude here can put cards on it, move them through your columns and talk to you on them.</p>
      <button class="btn" onclick={() => setBoardOn(h, true)}>Turn on the Board</button>
    </div>
  {:else}
    <div class="bar">
      <div class="seg" role="radiogroup" aria-label="Show">
        <button class:on={!$boardAll} disabled={focusedClaude == null} title="This claude's cards and unassigned ones" onclick={() => boardAll.set(false)}>{focusedClaude != null ? at(focusedClaude) : "This one"}</button>
        <button class:on={$boardAll} onclick={() => boardAll.set(true)}>All</button>
      </div>
      <button class="link" class:on={showArchived} onclick={() => (showArchived = !showArchived)}>
        {showArchived ? "← Board" : `Archived${archived.length ? ` (${archived.length})` : ""}`}
      </button>
    </div>

    {#if board.proposals?.length}
      <div class="props">
        {#each board.proposals as p (p.id)}
          <div class="prop">
            <div class="phead"><b>{at(p.by)}</b> wants to change the columns <span class="num">P{p.id}</span></div>
            {#if p.why}<div class="pwhy" dir="auto">{p.why}</div>{/if}
            <ul>
              {#each p.changes as ch, i (i)}
                {@const s = said(board, ch)}
                <li class={ch.op}>
                  <span class="picon">{s.icon}</span>
                  <div>
                    <div dir="auto">{s.line}</div>
                    {#if s.sub}<div class="psub" dir="auto">{s.sub}</div>{/if}
                    {#if s.was}<div class="pwas" dir="auto">was: {s.was}</div>{/if}
                  </div>
                </li>
              {/each}
            </ul>
            {#if rejecting === p.id}
              <!-- svelte-ignore a11y_autofocus -->
              <textarea dir="auto" rows="2" bind:value={rejectWhy} autofocus placeholder="Why not? (optional — {at(p.by)} reads it)"
                onkeydown={(e) => { if (e.key === "Escape") rejecting = null; else if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); void reject(p.id); } }}></textarea>
              <div class="row-end">
                <button class="link" onclick={() => (rejecting = null)}>Cancel</button>
                <button class="btn danger" onclick={() => reject(p.id)}>Reject</button>
              </div>
            {:else}
              <div class="row-end">
                <button class="btn quiet" onclick={() => { rejecting = p.id; rejectWhy = ""; }}>Reject…</button>
                <button class="btn" onclick={() => edit({ op: "approve", proposal: p.id })}>Approve</button>
              </div>
            {/if}
          </div>
        {/each}
      </div>
    {/if}

    {#if showArchived}
      <div class="archived">
        {#if !archived.length}
          <div class="empty">Nothing archived.</div>
        {/if}
        {#each archived as c (c.n)}
          <div class="arow">
            <button class="atitle" dir="auto" onclick={() => (openN = c.n)}>#{c.n} {c.title}</button>
            <span class="ameta">{board.columns.find((x) => x.id === c.col)?.name}{c.assignee != null ? ` · ${at(c.assignee)}` : ""}</span>
            <button class="link" onclick={() => edit({ op: "archive", n: c.n, on: false })}>Restore</button>
          </div>
        {/each}
      </div>
    {:else}
      <div
        class="cols"
        bind:this={colsEl}
        onpointermove={move}
        onpointerup={up}
        onpointercancel={() => (drag = null)}
        role="list"
      >
        {#each board.columns as col, ci (col.id)}
          {@const list = cardsIn(board, col.id)}
          <div
            class="col"
            data-col={col.id}
            role="listitem"
            class:drop-before={drag?.live && drag.kind === "col" && drag.col === col.id && drag.n !== col.id}
            class:dragging={drag?.live && drag.kind === "col" && drag.n === col.id}
          >
            <!-- svelte-ignore a11y_no_static_element_interactions -->
            <div class="chead" onpointerdown={(e) => down(e, "col", col.id, col.name)}>
              {#if renaming === col.id}
                <!-- svelte-ignore a11y_autofocus -->
                <input class="rename" dir="auto" value={col.name} autofocus
                  onkeydown={(e) => { if (e.key === "Enter") (e.currentTarget as HTMLInputElement).blur(); else if (e.key === "Escape") renaming = null; }}
                  onblur={(e) => rename(col, e.currentTarget as HTMLInputElement)} />
              {:else}
                <button class="cname" dir="auto" title="Rename" onclick={() => { if (!suppressClick) renaming = col.id; }}>{col.name}</button>
              {/if}
              <span class="ccount">{list.length}</span>
              <button class="dots" title="Column menu" onclick={() => (menuFor = menuFor === col.id ? null : col.id)}>⋯</button>
              {#if menuFor === col.id}
                <div class="menu" role="menu">
                  <button role="menuitem" onclick={() => { menuFor = null; describing = col.id; descDraft = col.desc; }}>Edit description…</button>
                  <button role="menuitem" disabled={ci === 0} onclick={() => { menuFor = null; moveCol(board, col, -1); }}>Move left</button>
                  <button role="menuitem" disabled={ci === board.columns.length - 1} onclick={() => { menuFor = null; moveCol(board, col, 1); }}>Move right</button>
                  <button role="menuitem" disabled={!list.length} onclick={() => { menuFor = null; edit({ op: "archive_column", col: col.id }); }}>Archive all cards</button>
                  <button role="menuitem" class="danger" disabled={board.columns.length <= 1} onclick={() => askDelete(board, col)}>Delete column…</button>
                </div>
              {/if}
            </div>
            {#if describing === col.id}
              <div class="desc-edit">
                <!-- svelte-ignore a11y_autofocus -->
                <textarea dir="auto" rows="3" bind:value={descDraft} autofocus placeholder="What goes here? Every claude reads this, e.g. “Move here when you need me to test, with the QA steps.”"
                  onkeydown={(e) => { if (e.key === "Escape") describing = null; else if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); edit({ op: "col_edit", col: col.id, desc: descDraft }); describing = null; } }}></textarea>
                <div class="row-end">
                  <button class="link" onclick={() => (describing = null)}>Cancel</button>
                  <button class="btn" onclick={() => { edit({ op: "col_edit", col: col.id, desc: descDraft }); describing = null; }}>Save</button>
                </div>
              </div>
            {:else if col.desc}
              <div class="cdesc" dir="auto" title={col.desc}>{col.desc}</div>
            {/if}

            <div class="cards">
              {#each list as c (c.n)}
                <!-- svelte-ignore a11y_no_static_element_interactions -->
                <div
                  class="cardrow"
                  class:drop-before={drag?.live && drag.kind === "card" && drag.col === col.id && drag.before === c.n}
                >
                  <!-- svelte-ignore a11y_click_events_have_key_events -->
                  <div
                    class="card"
                    data-card={c.n}
                    role="button"
                    tabindex="0"
                    class:urgent={c.urgent}
                    class:mine={!$boardAll || c.assignee === focusedClaude}
                    class:dragging={drag?.live && drag.kind === "card" && drag.n === c.n}
                    onpointerdown={(e) => down(e, "card", c.n, c.title)}
                    onclick={() => open(c)}
                    onkeydown={(e) => { if (e.key === "Enter") open(c); }}
                  >
                    <div class="ctitle" dir="auto">{c.title}</div>
                    <div class="cmeta">
                      <span class="num">#{c.n}</span>
                      {#if c.urgent}<span class="urgent-tag">URGENT</span>{/if}
                      {#if c.assignee != null}<span class="who" class:gone={gone(c.assignee)} class:me={c.assignee === focusedClaude}>{assigneeLabel(c.assignee)}</span>{/if}
                      {#if c.choices.length}<span title={c.chosen ? `chose: ${c.chosen}` : "Waiting for your choice"}>{c.chosen ? "◆✓" : "◆"}</span>{/if}
                      {#if c.comments.length}<span title="Comments">💬{c.comments.length}</span>{/if}
                      {#if imageCount(c)}<span title="Images">🖼{imageCount(c)}</span>{/if}
                      {#if pendingSet.has(c.n)}<span class="pend" title="Your changes here wait for UPDATE">pending</span>{/if}
                      {#if c.unread}<span class="udot" title="A claude changed this since you last opened it"></span>{/if}
                    </div>
                  </div>
                </div>
              {/each}
              <div class="cardrow end" class:drop-before={drag?.live && drag.kind === "card" && drag.col === col.id && drag.before == null}></div>
            </div>

            {#if addingIn === col.id}
              <div class="add-card">
                <input dir="auto" placeholder="Card title" bind:value={addTitle} onkeydown={onAddKey} />
                <div class="row-end">
                  <select bind:value={addAssignee} title="Assignee">
                    <option value="">Nobody</option>
                    {#each claudes as s (s.id)}<option value={String(s.id)}>{at(s.id)}</option>{/each}
                  </select>
                  <button class="link" onclick={() => { addingIn = null; terminals.refocus(); }}>✕</button>
                  <button class="btn" disabled={!addTitle.trim()} onclick={add}>Add</button>
                </div>
              </div>
            {:else}
              <button class="add-open" onclick={() => startAdd(col.id)}>+ Add a card</button>
            {/if}
          </div>
        {/each}
        <div class="col newcol" class:drop-before={drag?.live && drag.kind === "col" && drag.col == null}>
          {#if newCol != null}
            <!-- svelte-ignore a11y_autofocus -->
            <input class="rename" dir="auto" placeholder="Column name" bind:value={newCol} autofocus
              onkeydown={(e) => { if (e.key === "Enter") void addColumn(); else if (e.key === "Escape") newCol = null; }}
              onblur={() => void addColumn()} />
          {:else}
            <button class="plus" title="Add a column" onclick={() => (newCol = "")}>+</button>
          {/if}
        </div>
      </div>
    {/if}

    <footer>
      <span class="status">
        {#if pendingSet.size}{pendingSet.size} card{pendingSet.size === 1 ? "" : "s"} not sent{:else if sending}sending to {sending === 1 ? at(board.sending[0]) : `${sending} claudes`}…{:else}up to date{/if}
        {#if unreadCount(board)}<span class="unread"> · {unreadCount(board)} updated</span>{/if}
      </span>
      <button class="update" disabled={!pendingClaudes.length || updating} onclick={update} title="Tell the claudes what you changed">
        {pendingClaudes.length === 1 ? `UPDATE ${at(pendingClaudes[0])}` : pendingClaudes.length > 1 ? `UPDATE · ${pendingClaudes.length} claudes` : "UPDATE"}
      </button>
    </footer>

    {#if openCard}
      <CardOverlay handle={h} {board} card={openCard} {claudes} onclose={() => { openN = null; terminals.refocus(); }} />
    {/if}

    {#if deleting}
      <div class="modal-back">
        <div class="modal" role="dialog" aria-label="Delete column">
          <div class="mtitle" dir="auto">Delete “{deleting.col.name}”?</div>
          <div class="mtext">It has {deleting.count} card{deleting.count === 1 ? "" : "s"}.</div>
          <label class="mrow">
            Move them to
            <select bind:value={deleting.to}>
              {#each board.columns.filter((x) => x.id !== deleting?.col.id) as c (c.id)}<option value={String(c.id)}>{c.name}</option>{/each}
            </select>
          </label>
          <div class="row-end">
            <button class="link" onclick={() => (deleting = null)}>Cancel</button>
            <button class="btn quiet" onclick={() => { const d = deleting!; deleting = null; edit({ op: "col_delete", col: d.col.id, to: null }); }}>Archive them</button>
            <button class="btn" onclick={() => { const d = deleting!; deleting = null; edit({ op: "col_delete", col: d.col.id, to: Number(d.to) }); }}>Move & delete</button>
          </div>
        </div>
      </div>
    {/if}
  {/if}
</div>

{#if drag?.live}
  <div class="drag-ghost" style="left:{drag.x + 8}px;top:{drag.y + 6}px" dir="auto">{drag.title}</div>
{/if}

<style>
  .tab {
    position: relative;
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
  .seg button:disabled {
    opacity: 0.5;
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
  .link:hover,
  .link.on {
    color: var(--text);
  }
  .empty {
    padding: 1.2rem 0.9rem;
    color: var(--text-faint);
    font-size: 0.85rem;
    line-height: 1.5;
  }
  .empty p {
    margin: 0.4rem 0 0.8rem;
    font-size: 0.78rem;
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
  .btn {
    white-space: nowrap;
  }
  .btn.quiet {
    background: none;
    border: 1px solid var(--border);
    color: var(--text-dim);
  }
  .btn:disabled {
    opacity: 0.45;
  }
  .btn.danger {
    background: var(--dot-needs);
  }

  /* ── column proposals ── */
  .props {
    flex: 0 1 auto;
    max-height: 45%;
    overflow-y: auto;
    padding: 0.45rem 0.45rem 0;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }
  .prop {
    border: 1px solid var(--accent);
    border-radius: 6px;
    padding: 0.45rem 0.55rem;
    font-size: 0.78rem;
    background: color-mix(in srgb, var(--accent) 7%, transparent);
  }
  .phead {
    color: var(--text-dim);
  }
  .phead b {
    color: var(--text);
  }
  .phead .num {
    float: right;
    color: var(--text-faint);
    font-size: 0.7rem;
  }
  .pwhy {
    margin-top: 0.2rem;
    color: var(--text-dim);
    font-style: italic;
  }
  .prop ul {
    list-style: none;
    margin: 0.35rem 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }
  .prop li {
    display: flex;
    gap: 0.4rem;
    color: var(--text);
  }
  .picon {
    flex: 0 0 1rem;
    text-align: center;
    color: var(--accent);
  }
  .prop li.delete .picon {
    color: var(--dot-needs);
  }
  .psub {
    color: var(--text-dim);
    white-space: pre-wrap;
  }
  .pwas {
    color: var(--text-faint);
    text-decoration: line-through;
    white-space: pre-wrap;
  }
  .prop textarea {
    width: 100%;
    box-sizing: border-box;
    margin-bottom: 0.3rem;
    font: inherit;
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 0.25rem 0.4rem;
    resize: vertical;
  }

  /* ── columns ── */
  .cols {
    user-select: none;
    -webkit-user-select: none;
    flex: 1;
    min-height: 0;
    display: flex;
    gap: 0.4rem;
    padding: 0.45rem 0.45rem 0.3rem;
    overflow-x: auto;
    overflow-y: hidden;
  }
  .col {
    position: relative;
    flex: 1 0 9.5rem;
    min-width: 9.5rem;
    max-width: 18rem;
    display: flex;
    flex-direction: column;
    min-height: 0;
    border-radius: 6px;
    background: color-mix(in srgb, var(--text) 4%, transparent);
  }
  .col.dragging {
    opacity: 0.5;
  }
  .col.drop-before::before {
    content: "";
    position: absolute;
    left: -0.27rem;
    top: 0;
    bottom: 0;
    width: 2px;
    background: var(--accent);
  }
  .newcol {
    flex: 0 0 1.8rem;
    min-width: 1.8rem;
    align-items: center;
    background: none;
    border: 1px dashed var(--border);
  }
  .newcol:has(input) {
    flex: 0 0 9.5rem;
    padding: 0.35rem;
  }
  .plus {
    width: 100%;
    flex: 1;
    border: none;
    background: none;
    color: var(--text-faint);
    font-size: 1rem;
  }
  .plus:hover {
    color: var(--text);
  }
  .chead {
    position: relative;
    display: flex;
    align-items: center;
    gap: 0.25rem;
    padding: 0.35rem 0.35rem 0.2rem 0.45rem;
    cursor: grab;
    touch-action: none;
  }
  .cname {
    flex: 1;
    min-width: 0;
    border: none;
    background: none;
    padding: 0;
    color: var(--text);
    font-size: 0.75rem;
    font-weight: 700;
    text-align: start;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    unicode-bidi: plaintext;
    cursor: text;
  }
  .rename {
    flex: 1;
    min-width: 0;
    width: 100%;
    padding: 0.1rem 0.3rem;
    border: 1px solid var(--border-focus);
    border-radius: 4px;
    background: var(--bg);
    color: var(--text);
    font: inherit;
    font-size: 0.75rem;
  }
  .ccount {
    font-size: 0.66rem;
    color: var(--text-faint);
  }
  .dots {
    border: none;
    background: none;
    color: var(--text-faint);
    padding: 0 0.2rem;
    border-radius: 3px;
    font-size: 0.85rem;
    line-height: 1;
  }
  .dots:hover {
    background: var(--border);
    color: var(--text);
  }
  .menu {
    position: absolute;
    top: 1.7rem;
    right: 0.2rem;
    z-index: 10;
    display: flex;
    flex-direction: column;
    min-width: 10rem;
    padding: 0.2rem;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg-elev);
    box-shadow: 0 4px 16px rgba(0, 0, 0, 0.35);
  }
  .menu button {
    border: none;
    background: none;
    color: var(--text);
    font-size: 0.75rem;
    text-align: start;
    padding: 0.3rem 0.5rem;
    border-radius: 4px;
  }
  .menu button:hover:not(:disabled) {
    background: var(--border);
  }
  .menu button:disabled {
    color: var(--text-faint);
  }
  .menu .danger:not(:disabled) {
    color: var(--dot-needs, #e5534b);
  }
  .cdesc {
    padding: 0 0.45rem 0.25rem;
    font-size: 0.66rem;
    line-height: 1.3;
    color: var(--text-faint);
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
    unicode-bidi: plaintext;
  }
  .desc-edit {
    padding: 0 0.35rem 0.3rem;
  }
  .cards {
    flex: 1;
    min-height: 2rem;
    overflow-y: auto;
    padding: 0.1rem 0.3rem;
  }
  .cardrow {
    position: relative;
    padding-bottom: 0.3rem;
  }
  .cardrow.end {
    min-height: 1rem;
  }
  .cardrow.drop-before::before {
    content: "";
    position: absolute;
    left: 0;
    right: 0;
    top: -0.2rem;
    height: 2px;
    border-radius: 1px;
    background: var(--accent);
  }
  .card {
    padding: 0.35rem 0.45rem 0.3rem;
    border: 1px solid var(--border);
    border-radius: 5px;
    background: var(--bg-elev);
    cursor: pointer;
    touch-action: none;
    user-select: none;
    opacity: 0.72;
  }
  .card.mine {
    opacity: 1;
  }
  .card:hover {
    border-color: var(--border-focus);
  }
  .card.urgent {
    border-inline-start: 3px solid var(--dot-needs, #e5534b);
  }
  .card.dragging {
    opacity: 0.35;
  }
  .ctitle {
    font-size: 0.78rem;
    line-height: 1.3;
    color: var(--text);
    overflow-wrap: anywhere;
    unicode-bidi: plaintext;
    text-align: start;
  }
  .cmeta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.3rem;
    margin-top: 0.2rem;
    font-size: 0.64rem;
    color: var(--text-faint);
  }
  .who {
    padding: 0 0.25rem;
    border-radius: 3px;
    background: var(--border);
    color: var(--text-dim);
  }
  .who.me {
    background: color-mix(in srgb, var(--accent) 25%, transparent);
    color: var(--text);
  }
  .who.gone {
    text-decoration: line-through;
  }
  .urgent-tag {
    padding: 0 0.25rem;
    border-radius: 3px;
    background: var(--dot-needs, #e5534b);
    color: #fff;
    font-weight: 700;
    font-size: 0.58rem;
  }
  .udot {
    width: 0.45rem;
    height: 0.45rem;
    border-radius: 50%;
    background: var(--accent);
    margin-inline-start: auto;
  }
  .add-open {
    margin: 0.1rem 0.3rem 0.35rem;
    border: none;
    background: none;
    color: var(--text-faint);
    font-size: 0.72rem;
    text-align: start;
    padding: 0.2rem 0.2rem;
    border-radius: 4px;
  }
  .add-open:hover {
    background: var(--border);
    color: var(--text);
  }
  .add-card {
    padding: 0.1rem 0.3rem 0.35rem;
  }
  .add-card input,
  textarea,
  select {
    width: 100%;
    padding: 0.25rem 0.4rem;
    border: 1px solid var(--border);
    border-radius: 5px;
    background: var(--bg);
    color: var(--text);
    font: inherit;
    font-size: 0.75rem;
  }
  .add-card input,
  textarea {
    unicode-bidi: plaintext;
    user-select: text;
    -webkit-user-select: text;
  }
  .add-card select,
  .mrow select {
    width: auto;
    min-width: 0;
    flex: 1;
    padding: 0.1rem 0.2rem;
    font-size: 0.7rem;
  }
  textarea {
    resize: vertical;
  }
  input:focus,
  textarea:focus,
  select:focus {
    outline: none;
    border-color: var(--border-focus);
  }
  .row-end {
    display: flex;
    justify-content: flex-end;
    align-items: center;
    gap: 0.35rem;
    margin-top: 0.3rem;
  }

  /* ── archived ── */
  .archived {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 0.3rem 0;
  }
  .arow {
    display: flex;
    align-items: baseline;
    gap: 0.4rem;
    padding: 0.35rem 0.8rem;
    border-bottom: 1px solid var(--border);
  }
  .atitle {
    flex: 1;
    min-width: 0;
    border: none;
    background: none;
    padding: 0;
    color: var(--text);
    font-size: 0.8rem;
    text-align: start;
    unicode-bidi: plaintext;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ameta {
    font-size: 0.66rem;
    color: var(--text-faint);
    white-space: nowrap;
  }

  footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    padding: 0.3rem 0.7rem;
    border-top: 1px solid var(--border);
    font-size: 0.68rem;
    color: var(--text-faint);
  }
  .unread {
    color: var(--accent);
  }
  .update {
    border: none;
    border-radius: 5px;
    background: var(--accent);
    color: var(--bg);
    font-size: 0.72rem;
    font-weight: 700;
    padding: 0.2rem 0.7rem;
    white-space: nowrap;
  }
  .update:disabled {
    background: var(--border);
    color: var(--text-faint);
  }
  .pend {
    padding: 0 0.25rem;
    border-radius: 3px;
    border: 1px solid var(--accent);
    color: var(--accent);
    font-size: 0.58rem;
  }
  :global(body.no-select),
  :global(body.no-select *) {
    user-select: none !important;
    -webkit-user-select: none !important;
  }

  .modal-back {
    position: absolute;
    inset: 0;
    z-index: 20;
    display: grid;
    place-items: center;
    background: rgba(0, 0, 0, 0.35);
  }
  .modal {
    width: min(24rem, 92%);
    padding: 0.8rem;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-elev);
    box-shadow: 0 6px 24px rgba(0, 0, 0, 0.4);
  }
  .mtitle {
    font-weight: 700;
    font-size: 0.85rem;
  }
  .mtext {
    margin: 0.3rem 0 0.5rem;
    font-size: 0.78rem;
    color: var(--text-dim);
  }
  .modal .row-end {
    flex-wrap: wrap;
    margin-top: 0.7rem;
  }
  .mrow {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.75rem;
    color: var(--text-dim);
  }

  .drag-ghost {
    position: fixed;
    z-index: 100;
    max-width: 14rem;
    padding: 0.3rem 0.5rem;
    border: 1px solid var(--accent);
    border-radius: 5px;
    background: var(--bg-elev);
    color: var(--text);
    font-size: 0.75rem;
    box-shadow: 0 4px 14px rgba(0, 0, 0, 0.4);
    pointer-events: none;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    unicode-bidi: plaintext;
  }
</style>
