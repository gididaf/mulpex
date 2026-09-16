<script lang="ts">
  import { onMount, tick, untrack } from "svelte";
  import { get } from "svelte/store";
  import { listen } from "@tauri-apps/api/event";
  import { open as openFolder, confirm } from "@tauri-apps/plugin-dialog";
  import { getCurrentWebview } from "@tauri-apps/api/webview";
  import { getCurrentWindow } from "@tauri-apps/api/window";

  import {
    bootstrap,
    listRecentProjects,
    claudeStatus,
    openProject,
    closeProject,
    switchProject,
    reorderProjects,
    reorderSessions,
    createSession,
    createTerminal,
    closeSession,
    restartSession,
    focusSession,
    getHubSnapshot,
    explainNow,
    clearExplain,
    sendBytes,
    setSessionMuted,
    setMuteMenuChecked,
    type BootstrapInfo,
    type ClaudeStatus,
    type HubUpdateEvent,
    type ExplainUpdateEvent,
    type ExplainPendingEvent,
    type SessionsChangedEvent,
    type SessionExitedEvent,
    type ProjectHandle,
  } from "./lib/ipc";
  import type { SessionInfo } from "./lib/ipc";
  import {
    projects,
    activeProjectHandle,
    project,
    sessions,
    activeId,
    statuses,
    showMessages,
    showExplainer,
    explainDeciding,
    showPalette,
    rename,
    addProject,
    removeProject,
    setActiveProject,
    setActiveSession,
    setSessionsFor,
    setSessionMutedLocal,
    applyHubFor,
    applyExplainFor,
    setExplainPendingFor,
    removeExplainsFor,
    displayOrder,
    clampToGroup,
    dragOrder,
    claudeInAnotherProject,
    flashNotice,
    reorderProjects as reorderProjectsLocal,
    reorderSessions as reorderSessionsLocal,
  } from "./lib/stores";
  import { findByDir } from "./lib/stores";
  import { terminals } from "./lib/terminals";
  import { initAttention } from "./lib/attention";

  import ProjectPicker from "./lib/components/ProjectPicker.svelte";
  import ProjectTabBar from "./lib/components/ProjectTabBar.svelte";
  import TopBar from "./lib/components/TopBar.svelte";
  import UpdateBanner from "./lib/components/UpdateBanner.svelte";
  import { checkForUpdate, startUpdateChecks } from "./lib/updater";
  import BottomBar from "./lib/components/BottomBar.svelte";
  import InstanceList from "./lib/components/InstanceList.svelte";
  import HubPanel from "./lib/components/HubPanel.svelte";
  import TerminalPane from "./lib/components/TerminalPane.svelte";
  import ExplainerPanel from "./lib/components/ExplainerPanel.svelte";
  import MessageReader from "./lib/components/MessageReader.svelte";
  import CommandPalette from "./lib/components/CommandPalette.svelte";
  import RenameDialog from "./lib/components/RenameDialog.svelte";
  import ContextMenu from "./lib/components/ContextMenu.svelte";
  import type { CtxItem } from "./lib/components/ContextMenu.svelte";

  let recents: string[] = $state([]);
  let ready = $state(false);
  let claude: ClaudeStatus | null = $state(null);
  let openError: string | null = $state(null);

  /** Build one project's UI state + xterms, then activate it if asked. */
  async function bootstrapProject(info: BootstrapInfo, makeActive: boolean) {
    addProject({
      handle: info.handle,
      dir: info.project_dir,
      name: info.project_name,
      sessions: info.sessions,
      statuses: new Map(),
      tasks: new Map(),
      explains: new Map(),
      explainPending: new Set(),
      hub: null,
      activeSessionId: info.sessions[info.active]?.id ?? null,
    });
    await tick(); // let TerminalView children mount + create their terminals
    const snap = await getHubSnapshot(info.handle);
    if (snap) applyHubFor(info.handle, snap);
    // Nothing to fetch for the Explainer: an explanation only exists between a
    // ⌘⇧E and the next prompt, and the panel starts closed.
    if (makeActive) selectProject(info.handle);
  }

  /** Switch which project is front-most: visible terminal, WebGL, backend active. */
  function selectProject(handle: ProjectHandle) {
    setActiveProject(handle);
    switchProject(handle);
    const p = get(projects).get(handle);
    const aid = p?.activeSessionId ?? null;
    if (aid != null) {
      focusSession(handle, aid);
      terminals.focus(handle, aid);
    } else {
      // Project with no sessions: hide every terminal (no key matches).
      terminals.focus(handle, -1);
    }
  }

  /** Focus a session within the active project. */
  /**
   * Commit a dragged tab order: reorder locally for an immediate repaint, then
   * tell the backend, which persists it to `open.txt` so it survives relaunch.
   *
   * Tab order is also what ⌘1–⌘9 index into (see `handleMenu`), so a drag
   * remaps those shortcuts too — intentionally, since they're documented as
   * "the Nth open project, in tab-bar order".
   */
  function applyProjectOrder(order: ProjectHandle[]) {
    reorderProjectsLocal(order);
    reorderProjects(order);
  }

  /**
   * Commit a dragged sidebar order: reorder locally for an immediate repaint,
   * then tell the backend, which rewrites the session store so the arrangement
   * survives relaunch (and echoes the same order back on the next poll).
   *
   * Sidebar order is also what ⌘[ / ⌘] cycle, so a drag remaps those too — the
   * "what you see is what you cycle" rule the muted-sinking sort already follows.
   */
  function applySessionOrder(ids: number[]) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    reorderSessionsLocal(h, ids);
    reorderSessions(h, ids);
  }

  // ---- sidebar context menu (right-click) ----
  //
  // The menu acts on the row you clicked, NOT on the focused one — right-clicking
  // a background instance to rename or close it must not yank the center pane
  // away from whatever you were reading. That is also why the ⌘R/⌘M/⌘W hints are
  // conditional below: those keys act on the *focused* row, so printing them
  // beside an item aimed at a different one would advertise a key that does
  // something else.

  let ctx = $state<{ x: number; y: number; items: CtxItem[] } | null>(null);

  /** Copy without a plugin: `navigator.clipboard` where the webview allows it,
   *  the execCommand path otherwise. WKWebView has historically refused the
   *  async API off a secure-context check, and a silently empty clipboard is
   *  exactly the kind of failure that gets blamed on the user's fingers. */
  async function copyText(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      return;
    } catch {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      try {
        document.execCommand("copy");
      } finally {
        ta.remove();
      }
    }
  }

  /**
   * What Copy address puts on the clipboard for `s`.
   *
   * With no claude in any other project, `claude#3` / `term#5` — the hub's own
   * local form, pasteable straight into a hub_send or a prompt.
   *
   * Once another project has a claude, a *bare* `claude#3` is the dangerous
   * string: it is a valid LOCAL address in every project, so a claude reading it
   * elsewhere sends to its own instance 3 and the message is silently delivered
   * to the wrong one. `<project>#<n>` cannot do that — and it is not a
   * cross-project-only form: `mcp.rs::send_foreign` resolves a qualifier naming
   * the sender's own project back to a local send, so one string is exact from
   * everywhere, and is the same one `hub_instances` reports.
   *
   * Terminals invert that. There is no cross-project address for one at all, and
   * `registry::parse_address` refuses every `term#…` outright (pointing at
   * hub_terminal_send), so nothing here can mis-deliver and the project is free
   * to be a readable parenthetical rather than an address that doesn't exist.
   */
  function instanceAddress(s: SessionInfo): string {
    const h = get(activeProjectHandle);
    const shell = s.kind === "shell";
    const bare = `${shell ? "term" : "claude"}#${s.id}`;
    if (h == null || !claudeInAnotherProject(h)) return bare;
    const name = get(projects).get(h)?.name;
    if (!name) return bare;
    return shell ? `${bare} (in ${name})` : `${name}#${s.id}`;
  }

  function openRowMenu(e: MouseEvent, s: SessionInfo) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    // Only the focused row's menu carries key hints (see above).
    const key = (k: string) => (get(activeId) === s.id ? k : undefined);
    const shell = s.kind === "shell";
    const items: CtxItem[] = [
      {
        label: "Rename…",
        hint: key("⌘R"),
        run: () => rename.set({ handle: h, id: s.id, value: s.name ?? "" }),
      },
      // Terminals can't be muted — the backend drops the flag for a shell — so
      // the item is absent rather than present-and-dead.
      ...(shell
        ? []
        : [
            {
              label: s.muted ? "Unmute" : "Mute",
              hint: key("⌘M"),
              run: () => muteSession(s.id, !s.muted),
            },
          ]),
      {
        // The address the hub itself uses, qualified with the project as soon as
        // another project has a claude in it — see `instanceAddress`.
        label: "Copy address",
        run: () => copyText(instanceAddress(s)),
      },
      { sep: true },
      // Terminals have no conversation to resume, so the item is absent rather
      // than present-and-refused (same reasoning as Mute above).
      ...(shell
        ? []
        : [
            {
              label: "Restart…",
              hint: key("⌘⇧R"),
              run: () => void restartInstance(h, s.id),
            },
          ]),
      { label: "Close", hint: key("⌘W"), danger: true, run: () => closeSession(h, s.id) },
    ];
    ctx = { x: e.clientX, y: e.clientY, items };
  }

  /** The gap below the last row. These two act on the active project rather than
   *  on any row, so their hints are unconditional. */
  function openEmptyMenu(e: MouseEvent) {
    if (get(activeProjectHandle) == null) return;
    ctx = {
      x: e.clientX,
      y: e.clientY,
      items: [
        { label: "New Session", hint: "⌘T", run: () => void newSession() },
        { label: "New Terminal", hint: "⌘⇧T", run: () => void newTerminal() },
      ],
    };
  }

  function closeContextMenu(chosen?: CtxItem) {
    ctx = null;
    chosen?.run?.();
    // Rename opens a dialog that wants the keyboard; everything else should hand
    // it straight back to the terminal, which the menu took focus from.
    if (!get(rename)) terminals.refocus();
  }

  function selectSession(id: number) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    setActiveSession(h, id);
    focusSession(h, id);
    terminals.focus(h, id);
  }

  /**
   * ⌘⇧E. Open the Explainer **and ask it**, in one keystroke — the panel has no
   * standing content of its own, so opening it without asking would show an
   * empty column.
   *
   * "Ask" is usually free: the backend compares the instance's transcript
   * against the one its answer was made from and reuses it when nothing moved,
   * so opening and closing the panel repeatedly costs one summarizer call, not
   * one per press. Only a turn that actually moved on produces a new answer —
   * and only then is the old one dropped, which is what `explainDeciding` is
   * for: the panel holds its content for the one frame the verdict takes rather
   * than flashing an explanation it is about to discard.
   *
   * Terminals open the panel to its "terminals aren't explained" line and cost
   * no Sonnet call — `explain_now` refuses them backend-side too.
   */
  async function toggleExplainer() {
    if (get(showExplainer)) {
      closeExplainer();
      return;
    }
    showExplainer.set(true);
    const h = get(activeProjectHandle);
    const cur = get(activeId);
    if (h == null || cur == null) return;
    explainDeciding.set(true);
    try {
      if ((await explainNow(h, cur)) === "running") removeExplainsFor(h, cur);
    } catch {
      // The panel falls back to its "nothing to explain yet" line.
    } finally {
      explainDeciding.set(false);
    }
  }

  /** Put the panel away, **keeping** its answer. A closed panel that threw its
   *  explanation away would make every re-open a fresh Sonnet call for the same
   *  three lines; the answer is only invalidated by the turn moving on, which
   *  `explain_now` detects and `clearExplain` (below) forces. */
  function closeExplainer() {
    if (!get(showExplainer)) return;
    showExplainer.set(false);
    explainDeciding.set(false);
  }

  // The panel is about the focused row, so focus moving puts it away. Watching
  // the focused (handle, id) covers every path at once — the sidebar, ⌘[ / ⌘],
  // a project switch, ⌘W taking the row away — instead of one call per call
  // site. It cannot fire on open: opening the panel changes neither. The rows'
  // answers are kept, so coming back and pressing ⌘⇧E again is free.
  $effect(() => {
    void $activeProjectHandle;
    void $activeId;
    untrack(closeExplainer);
  });

  // The user sent the next prompt (or answered the question / approved the plan
  // that was on screen): the focused claude goes back to `working`, and the
  // explanation is now about the turn before this one. This is the one event
  // that DISCARDS an answer rather than just hiding it — the next ⌘⇧E must not
  // reuse a summary of the previous turn. Only the transition INTO working
  // counts; `working` → `working` is the same turn's next tool call, which is
  // not news.
  let wasWorking = false;
  $effect(() => {
    const cur = $activeId;
    const h = $activeProjectHandle;
    const now = cur == null ? undefined : $statuses.get(cur);
    const working = now === "working";
    const started = working && !wasWorking;
    wasWorking = working;
    if (!started) return;
    untrack(() => {
      closeExplainer();
      if (h != null && cur != null) {
        removeExplainsFor(h, cur);
        clearExplain(h, cur); // also cancels a job still producing an answer
      }
    });
  });

  /**
   * Mute or unmute one session of the active project (⌘M, or the row's 🔇).
   *
   * Mute never moves focus: the muted session's terminal stays visible and
   * typeable, its row just dims and sinks. Muting is a statement about how loudly
   * the sidebar should talk about an instance, not about whether you're done
   * with it.
   */
  function muteSession(id: number, muted: boolean) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    setSessionMutedLocal(h, id, muted);
    setSessionMuted(h, id, muted); // persists; fire-and-forget
    syncMuteMenu();
  }

  /** Push the focused session's muted state into the menu's check item — the
   *  native menu has no view of which session is active.
   *
   *  Deduped: the trigger below re-runs on every hub poll that changes anything
   *  (statuses churn at 200 ms), and the tick almost never moves with it.
   *
   *  `force` defeats the dedup for the one case where the item's real state and
   *  our last-pushed value have diverged: muda ticks a check item *itself* before
   *  firing the event, so a click that resolves to "no session to mute" leaves a
   *  tick we never wrote and would otherwise never clear. */
  let lastMuteChecked: boolean | null = null;
  function syncMuteMenu(force = false) {
    const cur = get(activeId);
    const s = cur == null ? undefined : get(sessions).find((x) => x.id === cur);
    const checked = s?.muted ?? false;
    if (checked === lastMuteChecked && !force) return;
    lastMuteChecked = checked;
    setMuteMenuChecked(checked);
  }

  // Focus and the flag both move the tick, and both land here as a store change.
  $effect(() => {
    void $activeId;
    void $sessions;
    syncMuteMenu();
  });

  /** Open a project by path, or focus it if already open (picker/+/palette/drop). */
  async function openOrFocusProject(path: string) {
    const existing = findByDir(path);
    if (existing) {
      selectProject(existing.handle);
      return;
    }
    try {
      openError = null;
      const info = await openProject(path);
      await bootstrapProject(info, true);
    } catch (e) {
      // Surfaced in the picker: this is how a missing `claude` used to present
      // itself as the app silently ignoring the click.
      openError = e instanceof Error ? e.message : String(e);
      console.error("open project failed:", e);
    }
  }

  // Wire form of a dropped path: backslash-escape everything a shell would act
  // on. This is what Terminal.app/iTerm insert when you drag a file in, so the
  // prompt reads `/Users/me/My\ File.csv` — matching stock Claude Code — rather
  // than a quote-wrapped path. Characters ≥ U+0080 are left bare; real
  // terminals don't escape unicode and a backslash there just reads as noise.
  //
  // Control characters take the other branch: they have no printable backslash
  // form, and a literal \n or \r on the wire would submit the message halfway
  // through the path. ANSI-C `$'…'` keeps the bytes CR/LF-free while still
  // round-tripping to the real name.
  function escapePath(path: string): string {
    if (!/[\x00-\x1f\x7f]/.test(path))
      return path.replace(/[^\w@%+:,./=-]/g, (c) =>
        c.charCodeAt(0) < 0x80 ? "\\" + c : c,
      );
    const esc = path
      .replace(/\\/g, "\\\\")
      .replace(/'/g, "\\'")
      .replace(/\n/g, "\\n")
      .replace(/\r/g, "\\r")
      .replace(/\t/g, "\\t")
      .replace(/[\x00-\x1f\x7f]/g, (c) =>
        "\\" + c.charCodeAt(0).toString(8).padStart(3, "0"),
      );
    return `$'${esc}'`;
  }

  /**
   * A drop puts the absolute path(s) at the focused session's prompt — files
   * and folders alike, since a folder is a legitimate argument to hand Claude.
   *
   * The paths go over as a single **bracketed paste** (`ESC[200~ … ESC[201~`),
   * not as typed keystrokes, because that is the channel Claude Code inspects
   * for attachments: a pasted image path becomes an `[Image #N]` attachment it
   * can actually see, while a non-image path stays as text. Typed keystrokes
   * get no such treatment — measured, that was the whole reason dropping a
   * screenshot only ever produced a path string. One paste holding every
   * dropped path is the right shape: Claude extracts each image and leaves the
   * rest as text.
   *
   * The trailing space goes *inside* the paste, which looks wrong and isn't:
   * a space typed after `ESC[201~` **wipes the prompt** for any non-image path
   * (measured — the path renders, then Claude erases the line; presumably the
   * keystroke races its async paste handling). Inside the markers both text
   * paths and image attachments survive.
   *
   * With no session to type into (the picker, or a project sitting at zero
   * sessions) a drop falls back to opening the path as a project; the backend
   * rejects non-directories.
   */
  function dropPaths(paths: string[]) {
    if (!paths.length) return;
    const h = get(activeProjectHandle);
    const id = h == null ? null : (get(projects).get(h)?.activeSessionId ?? null);
    if (h == null || id == null) {
      for (const path of paths) openOrFocusProject(path);
      return;
    }
    const body = paths.map(escapePath).join(" ");
    // One write: split across reads, Claude can consume a lone ESC and lose
    // the paste framing (same hazard as the Shift+Enter carve-out).
    sendBytes(h, id, new TextEncoder().encode(`\x1b[200~${body} \x1b[201~`));
    terminals.refocus();
  }

  async function pickAndOpen() {
    const dir = await openFolder({ directory: true, title: "Open Project" });
    if (typeof dir === "string") await openOrFocusProject(dir);
  }

  async function closeProjectHandle(handle: ProjectHandle) {
    await closeProject(handle);
    terminals.disposeProject(handle);
    removeProject(handle); // re-picks the active handle (neighbor / null)
    const next = get(activeProjectHandle);
    if (next != null) selectProject(next);
    else recents = await listRecentProjects(); // none left → picker
  }

  async function newSession() {
    const h = get(activeProjectHandle);
    if (h == null) return;
    let info;
    try {
      info = await createSession(h);
    } catch (e) {
      // Spawning can be refused before it starts — most often because macOS is
      // withholding the project's folder from this app, which the backend
      // checks for by name. Without this the rejection was unhandled and ⌘T
      // simply did nothing at all, which is the failure it exists to explain.
      flashNotice(`Could not start Claude: ${e}`, 8000);
      return;
    }
    const p = get(projects).get(h);
    setSessionsFor(h, [...(p?.sessions ?? []), info]);
    await tick();
    selectSession(info.id);
  }

  /** ⌘⇧T — open a plain shell terminal and focus it. (A terminal an *instance*
   *  opens arrives via `sessions-changed` instead, and never steals focus.) */
  async function newTerminal() {
    const h = get(activeProjectHandle);
    if (h == null) return;
    let info;
    try {
      info = await createTerminal(h);
    } catch (e) {
      // The picker's error line only renders when no project is open, so a
      // failure here (no $SHELL, spawn refused) would otherwise be invisible.
      flashNotice(`Could not open a terminal: ${e}`, 4000);
      return;
    }
    const p = get(projects).get(h);
    setSessionsFor(h, [...(p?.sessions ?? []), info]);
    await tick();
    selectSession(info.id);
  }

  /**
   * ⌘⇧R — quit a claude and bring it straight back on the same row, resuming the
   * same conversation.
   *
   * Why it exists: a `claude` reads its world once, at exec. A rotated
   * CLAUDE_CODE_OAUTH_TOKEN, a skill installed a minute ago, an edited
   * settings.json reach a running instance never. ⌘W + ⌘T does pick them up but
   * hands back a *different* instance — new number, no name, empty inbox, and the
   * conversation to go and find.
   *
   * Confirmed, unlike ⌘W, and for a reason that isn't squeamishness: ⌘W closes a
   * session you were looking at and meant to be rid of, while this key sits one
   * shift away from ⌘R (Rename) and kills a process that may be mid-turn. The
   * native dialog gives Enter = Restart and Esc = Cancel for free.
   *
   * `handle`/`id` are passed in rather than read off the focus, because the
   * sidebar's context menu aims this at the row you right-clicked — which is
   * usually not the focused one.
   */
  async function restartInstance(handle: ProjectHandle, id: number) {
    const s = get(projects)
      .get(handle)
      ?.sessions.find((x) => x.id === id);
    if (!s) return;
    if (s.kind === "shell") {
      // The menu item is always enabled, so a ⌘⇧R on a terminal has to say why
      // nothing happened rather than look like a dead key.
      flashNotice(
        `term #${id} can't be restarted — only a claude has a conversation to resume.`,
        4000,
      );
      return;
    }
    const who = s.name ? `claude #${id} (${s.name})` : `claude #${id}`;
    const ok = await confirm(
      `Quit ${who} and start it again, resuming the same conversation?\n\n` +
        `It comes back with its number, name and unread messages intact, but ` +
        `anything it is doing right now is lost.`,
      {
        title: "Restart Session",
        kind: "warning",
        okLabel: "Restart",
        cancelLabel: "Cancel",
      },
    );
    if (!ok) {
      terminals.refocus();
      return;
    }
    try {
      await restartSession(handle, id);
    } catch (e) {
      // The backend refuses without killing anything when there is no transcript
      // to resume yet, which is the message worth showing — the instance the user
      // was about to lose is still running.
      flashNotice(`Could not restart claude #${id}: ${e}`, 8000);
      terminals.refocus();
      return;
    }
    // Same row, different process: the pane has to be wiped and bound to the new
    // PTY, which the backend is buffering until we do.
    terminals.reattach(handle, id);
    terminals.refocus();
  }

  /**
   * Cycle sessions within the active project, in the order the sidebar shows
   * them — claudes, then terminals. What you see is what you cycle.
   *
   * Muted rows are *skipped*: muting says "stop putting this in front of me",
   * and a row you have to tab past is still in front of you. They stay
   * reachable by clicking, and they still hold focus if one is selected — from
   * there the walk below simply steps to the nearest unmuted row in the
   * direction of travel, so ⌘] out of a muted row lands where the eye expects
   * rather than at the end of the list.
   *
   * The walk starts *before* the head of the list when nothing is focused, so a
   * first ⌘] selects the top row rather than the second one.
   */
  function cycle(delta: number) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    const list = displayOrder(get(projects).get(h)?.sessions ?? []);
    const n = list.length;
    if (n === 0) return;
    const cur = get(activeId);
    const at = list.findIndex((s) => s.id === cur);
    const start = at >= 0 ? at : delta > 0 ? -1 : 0;
    for (let k = 1; k <= n; k++) {
      const next = list[(((start + delta * k) % n) + n) % n];
      // Everything muted (and the focused row among them): nothing to move to,
      // so the keystroke does nothing rather than reselecting a silenced row.
      if (next && !next.muted) {
        selectSession(next.id);
        return;
      }
    }
  }

  /** Cycle between open projects (⌘⇧[ / ⌘⇧]). */
  function cycleProject(delta: number) {
    const keys = [...get(projects).keys()];
    if (keys.length < 2) return;
    const cur = get(activeProjectHandle);
    const idx = keys.findIndex((h) => h === cur);
    const next = keys[(idx + delta + keys.length) % keys.length];
    if (next != null) selectProject(next);
  }

  /**
   * Slide the active project one slot left/right in the tab bar (⌘⇧← / ⌘⇧→),
   * committing through the same path a drag does — so the new order is persisted
   * to `open.txt` and ⌘1–⌘9 remap with it.
   *
   * Clamped, not wrapped: the edges are a no-op. Moving is not cycling — a tab
   * teleporting from one end of the strip to the other reads as a mistake, and
   * the tab you just moved has to stay where your eye left it.
   */
  function moveProject(delta: number) {
    const order = [...get(projects).keys()];
    const cur = get(activeProjectHandle);
    const from = order.findIndex((h) => h === cur);
    const to = from + delta;
    if (from < 0 || to < 0 || to >= order.length) return;
    order.splice(to, 0, ...order.splice(from, 1));
    applyProjectOrder(order);
  }

  async function handleMenu(id: string) {
    const h = get(activeProjectHandle);
    switch (id) {
      case "open_project":
        await pickAndOpen();
        break;
      case "close_project":
        if (h != null) await closeProjectHandle(h);
        break;
      case "next_project":
        cycleProject(1);
        break;
      case "prev_project":
        cycleProject(-1);
        break;
      case "move_project_right":
        moveProject(1);
        break;
      case "move_project_left":
        moveProject(-1);
        break;
      case "new_session":
        if (h != null) await newSession();
        break;
      case "new_terminal":
        if (h != null) await newTerminal();
        break;
      case "close_session": {
        const cur = get(activeId);
        if (h != null && cur != null) closeSession(h, cur);
        break;
      }
      case "restart": {
        const cur = get(activeId);
        if (h != null && cur != null) await restartInstance(h, cur);
        break;
      }
      case "rename": {
        const cur = get(activeId);
        if (h != null && cur != null) {
          const s = get(sessions).find((x) => x.id === cur);
          rename.set({ handle: h, id: cur, value: s?.name ?? "" });
        }
        break;
      }
      case "mute": {
        const cur = get(activeId);
        if (h != null && cur != null) {
          const s = get(sessions).find((x) => x.id === cur);
          muteSession(cur, !s?.muted);
        } else {
          // Nothing focused: the check item already toggled itself on click, so
          // put it back rather than leaving a tick with nothing behind it.
          syncMuteMenu(true);
        }
        break;
      }
      case "messages":
        showMessages.update((v) => !v);
        break;
      case "explainer":
        toggleExplainer();
        break;
      case "minimize":
        // Custom item (muda hard-binds the predefined one to ⌘M, which is Mute).
        await getCurrentWindow().minimize();
        break;
      case "check_updates":
        // Manual: reports "up to date" and network errors too, unlike the
        // periodic check which stays silent on both.
        await checkForUpdate(true);
        break;
      case "next":
        cycle(1);
        break;
      case "prev":
        cycle(-1);
        break;
      default:
        // ⌘1–⌘9 → the Nth open project, in tab-bar order (the projects Map's
        // insertion order, which is what ProjectTabBar renders).
        if (id.startsWith("project_")) {
          const n = parseInt(id.slice("project_".length), 10);
          const h2 = [...get(projects).keys()][n - 1];
          if (h2 != null && h2 !== h) selectProject(h2);
        }
    }
  }

  /**
   * Commands chosen in ⌘P run through `handleMenu` — the same string-keyed switch
   * the native menu feeds — so the palette owns no second implementation of any
   * command and cannot drift from the menu.
   *
   * Closing the overlay drops keyboard focus, and most commands don't take it
   * anywhere, so the terminal has to be handed it back. The exceptions are the
   * commands whose whole job is to put focus somewhere else: `rename` and
   * `messages` open their own focus-taking UI, and `open_project` opens a native
   * folder dialog. Refocusing the terminal after those would fight them.
   */
  const PALETTE_KEEPS_FOCUS = new Set(["rename", "messages", "open_project"]);
  async function runPaletteAction(id: string) {
    showPalette.set(false);
    await handleMenu(id);
    if (!PALETTE_KEEPS_FOCUS.has(id)) terminals.refocus();
  }

  function onGlobalKey(e: KeyboardEvent) {
    // ⌘P / Ctrl+P toggles the command palette (projects, sessions, commands). Not a
    // menu accelerator, so it reaches the webview; preventDefault stops the print
    // dialog.
    if ((e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === "p") {
      e.preventDefault();
      if (get(projects).size > 0) showPalette.update((v) => !v);
      return;
    }
    // ⌘⇧[ / ⌘⇧] — Next/Previous Project. These ARE declared as menu accelerators
    // (menu.rs) and the menu shows them, but they were REPORTED not to fire while
    // their unshifted twins ⌘[ / ⌘] (sessions) work. Not measured: the suspected
    // cause is AppKit matching a key equivalent against the event's *shifted*
    // character, so an item whose keyEquivalent is "]" never matches a ⌘⇧] whose
    // character is "}" — letters survive it because AppKit case-folds them and
    // punctuation has no case to fold. Treat that as the hypothesis, not the
    // finding; what is certain is only that the menu path did not reach here.
    //
    // Catching it in the webview rather than re-keying the menu keeps the
    // advertised shortcut true, and is safe either way: a key equivalent AppKit
    // *does* match is consumed by the menu and never reaches the webview, so this
    // arm cannot double-fire — it simply stops seeing the key.
    if (e.metaKey && e.shiftKey && !e.altKey && !e.ctrlKey) {
      // `code` is the physical key — `key` is "}"/"{" here, and layout-dependent.
      const back = e.code === "BracketLeft";
      if (back || e.code === "BracketRight") {
        e.preventDefault();
        cycleProject(back ? -1 : 1);
        return;
      }
      // ⌘⇧← / ⌘⇧→ — Move Project Left/Right. Also declared as menu accelerators,
      // and unlike the brackets those DO fire — but only while focus is outside
      // the terminal. With the terminal focused the keys land on xterm's hidden
      // textarea, where ⌘⇧←/→ is a standard AppKit text-selection command
      // (`moveToLeftEndOfLineAndModifySelection:`); WebKit performs it and
      // reports the event handled, so AppKit never falls through to the main
      // menu. A DOM keydown runs *before* that default action, so claiming the
      // key here is what makes the shortcut work with the terminal focused —
      // which is where focus almost always is.
      const left = e.code === "ArrowLeft";
      if ((left || e.code === "ArrowRight") && !inTextField(e.target)) {
        e.preventDefault();
        moveProject(left ? -1 : 1);
      }
    }
  }

  /**
   * A field where ⌘⇧←/→ really is a selection gesture — the rename box, the
   * palette's query — so the shortcut must be left alone there.
   *
   * xterm's helper textarea is deliberately excluded: it is not a text field you
   * can see or edit, it's how the terminal receives keys, and its selection is
   * never rendered. Stealing the key there costs nothing.
   */
  function inTextField(target: EventTarget | null): boolean {
    const el = target as HTMLElement | null;
    if (!el?.tagName) return false;
    const editable =
      el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.isContentEditable;
    return editable && !el.classList.contains("xterm-helper-textarea");
  }

  onMount(() => {
    const unlisteners: Array<Promise<() => void>> = [];

    unlisteners.push(
      listen<string>("menu", (e) => handleMenu(e.payload)),
      listen<HubUpdateEvent>("hub-update", (e) =>
        applyHubFor(e.payload.handle, e.payload.snapshot),
      ),
      listen<ExplainUpdateEvent>("explain-update", (e) =>
        applyExplainFor(e.payload.handle, e.payload.id, e.payload.entry),
      ),
      listen<ExplainPendingEvent>("explain-pending", (e) =>
        setExplainPendingFor(e.payload.handle, e.payload.id, e.payload.active),
      ),
      listen<SessionExitedEvent>("session-exited", (e) => {
        terminals.dispose(e.payload.handle, e.payload.id);
        // The backend forgets the feed on reap; mirror it so a reused id can't
        // resurrect a dead instance's explanations.
        removeExplainsFor(e.payload.handle, e.payload.id);
      }),
      listen<SessionsChangedEvent>("sessions-changed", async (e) => {
        const { handle, sessions: list } = e.payload;
        setSessionsFor(handle, list);
        const cur = get(projects).get(handle)?.activeSessionId ?? null;
        if (cur == null || !list.some((s) => s.id === cur)) {
          const first = list[0]?.id ?? null;
          setActiveSession(handle, first);
          if (handle === get(activeProjectHandle)) {
            if (first != null) selectSession(first);
            else terminals.focus(handle, -1);
          }
        }
        // New xterms (e.g. hub_spawn children) mount hidden; refit brings them —
        // and their PTYs, spawned at the default size — to the shared geometry,
        // even for a background project.
        await tick();
        terminals.refit();
      }),
      // Drop files/folders onto the window to type their paths at the prompt.
      // Tauri's webview-level drag-drop (`dragDropEnabled`, on by default)
      // swallows the native drop before the DOM sees it, so xterm never gets a
      // `drop` event and this handler is the only place a drop can be honoured.
      getCurrentWebview().onDragDropEvent((ev) => {
        if (ev.payload.type === "drop") dropPaths(ev.payload.paths);
      }),
    );

    const onResize = () => terminals.refit();
    window.addEventListener("resize", onResize);

    // Check at startup, then every 6h. Silent unless something is actually
    // available — a failed check never surfaces on this path.
    const stopUpdateChecks = startUpdateChecks();

    // Dock badge + "a claude needs you" notifications. Clicking a banner routes
    // through the same select path as clicking the sidebar row, so it lands on
    // the pane with the question rather than merely raising the window.
    let stopAttention: (() => void) | null = null;
    initAttention((handle, id) => {
      selectProject(handle);
      setActiveSession(handle, id);
      focusSession(handle, id);
      terminals.focus(handle, id);
    }).then((stop) => {
      stopAttention = stop;
    });

    (async () => {
      const ws = await bootstrap();
      // BEFORE any TerminalView mounts. Restored sessions' PTYs have been running
      // since before the window painted, and `attach_session` flushes everything
      // they printed — so each xterm has to be built at the size those bytes were
      // rendered for. Building at xterm's 80x24 default and resizing afterwards
      // corrupts the pane permanently; see `terminals.ts::create`.
      terminals.setGeometry(ws.cols, ws.rows);
      if (ws.projects.length) {
        for (const info of ws.projects) {
          await bootstrapProject(info, info.handle === ws.active);
        }
      } else {
        recents = await listRecentProjects();
      }
      ready = true;
      // Non-blocking: the probe shells out to the login shell, so let the UI
      // paint first. Only matters for the picker's banner.
      claude = await claudeStatus();
    })();

    return () => {
      window.removeEventListener("resize", onResize);
      stopUpdateChecks();
      stopAttention?.();
      unlisteners.forEach((p) => p.then((f) => f()));
    };
  });
</script>

<svelte:window onkeydown={onGlobalKey} />

{#if ready && $project}
  <div class="shell" class:with-explainer={$showExplainer}>
    <ProjectTabBar
      onselect={selectProject}
      onclose={closeProjectHandle}
      onadd={pickAndOpen}
      onreorder={applyProjectOrder}
    />
    <TopBar />
    <aside class="sidebar">
      <InstanceList
        onselect={selectSession}
        onmute={muteSession}
        onreorder={applySessionOrder}
        oncontext={openRowMenu}
        oncontextempty={openEmptyMenu}
      />
      <HubPanel />
    </aside>
    <main class="pane">
      <TerminalPane />
    </main>
    {#if $showExplainer}
      <ExplainerPanel />
    {/if}
    <BottomBar />
  </div>
  {#if $showPalette}
    <CommandPalette
      onproject={(h) => {
        showPalette.set(false);
        selectProject(h);
      }}
      onsession={(id) => {
        showPalette.set(false);
        selectSession(id);
      }}
      onaction={runPaletteAction}
      onclose={() => showPalette.set(false)}
    />
  {/if}
  {#if $showMessages}
    <MessageReader onclose={() => showMessages.set(false)} />
  {/if}
  {#if $rename}
    <RenameDialog />
  {/if}
  {#if ctx}
    <ContextMenu x={ctx.x} y={ctx.y} items={ctx.items} onclose={closeContextMenu} />
  {/if}
{:else if ready}
  <ProjectPicker
    {recents}
    {claude}
    error={openError}
    onpick={pickAndOpen}
    onopen={openOrFocusProject}
  />
{/if}

<!-- Outside both branches AND ungated by `ready`: an update is equally relevant
     with a project open and sitting on the picker, and the card is fixed-position
     so it needs no slot in either layout.

     `ready` deliberately does NOT gate this. It flips only after bootstrap has
     walked every open project and built + attached an xterm for every session,
     serially — so with several projects restoring, the startup check would finish
     early and the banner still sit invisible for as long as that took. Measured:
     with zero projects the banner is up ~6s after launch; with three projects it
     was late enough to look like the launch check had simply not run. The card
     owns nothing that bootstrap provides, so it has no reason to wait for it. -->
<UpdateBanner />

<style>
  .shell {
    display: grid;
    grid-template-columns: var(--sidebar-w) 1fr;
    grid-template-rows: auto auto 1fr auto;
    grid-template-areas:
      "tabs tabs"
      "top top"
      "side pane"
      "bottom bottom";
    height: 100%;
  }
  /* The Explainer is a real third column: the pane narrows, its ResizeObserver
     refits, and every PTY workspace-wide follows (one geometry — the same class
     of resize as dragging the window edge). */
  .shell.with-explainer {
    grid-template-columns: var(--sidebar-w) 1fr var(--explainer-w);
    grid-template-areas:
      "tabs tabs tabs"
      "top top top"
      "side pane explain"
      "bottom bottom bottom";
  }
  .sidebar {
    grid-area: side;
    display: grid;
    /* The hub panel is half its old share (55% → 28%): messages are reference
       material, the session list is what you steer with. */
    grid-template-rows: 72% 28%;
    min-height: 0;
    background: var(--bg-sidebar);
    border-right: 1px solid var(--border);
  }
  /* Both rows scroll independently. Without min-height:0 a grid item's auto
     minimum lets it grow past its track instead of overflowing, and the
     `overflow-y: auto` inside InstanceList/HubPanel never engages. */
  .sidebar > :global(*) {
    min-height: 0;
  }
  .pane {
    grid-area: pane;
    min-width: 0;
    min-height: 0;
    background: var(--bg);
  }
</style>
