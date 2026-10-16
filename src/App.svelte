<script lang="ts">
  import { onMount, tick } from "svelte";
  import { get } from "svelte/store";
  import { listen } from "@tauri-apps/api/event";
  import { invoke } from "@tauri-apps/api/core";
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
    saveSession,
    saveCheck,
    focusSession,
    getHubSnapshot,
    sendBytes,
    remoteReply,
    setSessionMuted,
    setSessionCollapsed,
    setAutoCompact,
    setTodosEnabled,
    reparentSession,
    setMuteMenuChecked,
    quitApp,
    remoteSnapshotReply,
    type BootstrapInfo,
    type WorkspaceInfo,
    type ClaudeStatus,
    type HubUpdateEvent,
    type SaveProgressEvent,
    type ExplainProgressEvent,
    type BroadcastSummaryEvent,
    type SessionsChangedEvent,
    type SessionExitedEvent,
    type ProjectHandle,
  } from "./lib/ipc";
  import type { SessionInfo } from "./lib/ipc";
  import {
    projects,
    activeProjectHandle,
    sessions,
    activeId,
    showMessages,
    showPalette,
    rename,
    addProject,
    removeProject,
    setActiveProject,
    setActiveSession,
    setSessionsFor,
    setSessionMutedLocal,
    setSessionCollapsedLocal,
    setAutoCompactLocal,
    setTodosOnLocal,
    hiddenIds,
    collapsedAncestors,
    descendantsOf,
    applyHubFor,
    applySaveProgress,
    clearSaveState,
    saves,
    saveStatusOf,
    displayOrder,
    stepSlot,
    dragOrder,
    nestOrder,
    unnestOrder,
    setSessionParentLocal,
    claudeInAnotherProject,
    flashNotice,
    flashToast,
    reorderProjects as reorderProjectsLocal,
    reorderSessions as reorderSessionsLocal,
  } from "./lib/stores";
  import { hostProjects } from "./lib/stores";
  import { terminals } from "./lib/terminals";
  import {
    activeHost,
    geometryDriver,
    hostFrontendAction,
    hostName,
    hostOnline,
    hostUpload,
    hosts,
    isRemote,
    slotOf,
  } from "./lib/hosts";
  import { applyWorkspace, connectHost, initHosts, landingProject, removeHost, renameHost } from "./lib/remoteHosts";
  import { pastePaths } from "./lib/paths";
  import { IS_LINUX } from "./lib/platform";
  import { addPin, dropPins, loadPins, reloadPins } from "./lib/pins";
  import { openPanel, togglePanel, toggleTab } from "./lib/rightPanel";
  import { dropTodos, loadTodos, reloadTodos } from "./lib/todos";
  import { autoCompactTick, dropCompact, toggleCompact } from "./lib/compact";
  import { applyExplainProgress, dropExplains, startExplain } from "./lib/explain";
  import { initAttention } from "./lib/attention";

  import ProjectPicker from "./lib/components/ProjectPicker.svelte";
  import ProjectTabBar from "./lib/components/ProjectTabBar.svelte";
  import MenuBar from "./lib/components/MenuBar.svelte";
  import TopBar from "./lib/components/TopBar.svelte";
  import UpdateBanner from "./lib/components/UpdateBanner.svelte";
  import CheckpointPanel from "./lib/components/CheckpointPanel.svelte";
  import ResumeDialog from "./lib/components/ResumeDialog.svelte";
  import { checkpointOpen, checkResume, startCheckpoint } from "./lib/checkpoint";
  import FdaBanner from "./lib/components/FdaBanner.svelte";
  import { checkFda, openFda } from "./lib/fda";
  import { checkForUpdate, startUpdateChecks } from "./lib/updater";
  import BottomBar from "./lib/components/BottomBar.svelte";
  import InstanceList from "./lib/components/InstanceList.svelte";
  import HubPanel from "./lib/components/HubPanel.svelte";
  import TerminalPane from "./lib/components/TerminalPane.svelte";
  import MessageReader from "./lib/components/MessageReader.svelte";
  import CommandPalette from "./lib/components/CommandPalette.svelte";
  import RenameDialog from "./lib/components/RenameDialog.svelte";
  import LoadDialog from "./lib/components/LoadDialog.svelte";
  import SecretsDialog from "./lib/components/SecretsDialog.svelte";
  import LiveTokenDialog from "./lib/components/LiveTokenDialog.svelte";
  import ImportDialog from "./lib/components/ImportDialog.svelte";
  import RemoteDialog from "./lib/components/RemoteDialog.svelte";
  import BroadcastDialog from "./lib/components/BroadcastDialog.svelte";
  import { applySummaryEvent } from "./lib/broadcast";
  import ConnectHostDialog from "./lib/components/ConnectHostDialog.svelte";
  import RemoteProjectPicker from "./lib/components/RemoteProjectPicker.svelte";
  import ContextMenu from "./lib/components/ContextMenu.svelte";
  import type { CtxItem } from "./lib/components/ContextMenu.svelte";

  let recents: string[] = $state([]);
  let ready = $state(false);
  let claude: ClaudeStatus | null = $state(null);
  let openError: string | null = $state(null);

  /** Build one project's UI state + xterms, then activate it if asked. */
  async function bootstrapProject(info: BootstrapInfo, makeActive: boolean) {
    // Built already — `projects-changed` (this Mulpex's, or a host's) and the
    // call that opened it can both get here; the second only selects.
    if (get(projects).has(info.handle)) {
      if (makeActive) selectProject(info.handle);
      return;
    }
    addProject({
      handle: info.handle,
      dir: info.project_dir,
      name: info.project_name,
      sessions: info.sessions,
      statuses: new Map(),
      watching: new Set(),
      ctx: new Map(),
      tasks: new Map(),
      hub: null,
      activeSessionId: info.sessions[info.active]?.id ?? null,
    });
    void loadPins(info.handle);
    void loadTodos(info.handle);
    await tick(); // let TerminalView children mount + create their terminals
    const snap = await getHubSnapshot(info.handle).catch(() => null);
    if (snap) applyHubFor(info.handle, snap);
    if (makeActive) selectProject(info.handle);
  }

  /** The project last in front on each host, to come back to (`selectHost`). */
  const lastProject = new Map<number, ProjectHandle>();

  /** Switch which project is front-most: visible terminal, WebGL, backend active.
   *  Its host comes to the front with it (the tab bar shows that host's tabs). */
  function selectProject(handle: ProjectHandle) {
    const slot = slotOf(handle);
    activeHost.set(slot);
    lastProject.set(slot, handle);
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

  /**
   * Bring host `slot` to the front (the host switcher, ⌘⌥[ / ⌘⌥]): the first of
   * its projects with a claude that needs you, else the one last in front there.
   * A host with no project (or none known yet) shows its empty state.
   */
  function selectHost(slot: number) {
    activeHost.set(slot);
    const h = landingProject(slot, lastProject.get(slot));
    if (h != null) {
      selectProject(h);
    } else {
      setActiveProject(null);
      terminals.focus(-1, -1);
    }
  }

  /** ⌘⌥] / ⌘⌥[: the next/previous host, Local first. */
  function cycleHost(delta: number) {
    const slots = [0, ...[...get(hosts).keys()].sort((a, b) => a - b)];
    if (slots.length < 2) return;
    const at = slots.indexOf(get(activeHost));
    selectHost(slots[(at + delta + slots.length) % slots.length]);
  }

  /** Forget one project's UI (it closed, or its host's tabs are being rebuilt). */
  function dropProject(handle: ProjectHandle) {
    terminals.disposeProject(handle);
    dropPins(handle);
    dropTodos(handle);
    dropExplains(handle);
    removeProject(handle);
  }

  /** After a host's tabs changed: if the host in front lost the project in
   *  front, land on another of ITS projects — never jump to another host. */
  function afterHostChange(slot: number) {
    if (get(activeHost) !== slot) return;
    const cur = get(activeProjectHandle);
    if (cur != null && slotOf(cur) === slot && get(projects).has(cur)) return;
    const next = landingProject(slot, lastProject.get(slot));
    if (next != null) selectProject(next);
    else {
      setActiveProject(null);
      terminals.focus(-1, -1);
    }
  }

  /** The host switcher's right-click menu. */
  function openHostMenu(e: MouseEvent, slot: number) {
    const st = get(hosts).get(slot);
    if (!st) return;
    const connected = st.state === "connected";
    ctx = {
      x: e.clientX,
      y: e.clientY,
      items: [
        { label: "Rename…", run: () => (renameHostFor = { slot, value: st.name }) },
        st.enabled
          ? { label: "Disconnect", run: () => void connectHost(slot, false) }
          : { label: "Connect", run: () => void connectHost(slot, true) },
        ...(st.enabled && !connected
          ? [{ label: "Reconnect now", run: () => void connectHost(slot, true) }]
          : []),
        { sep: true },
        {
          label: "Remove…",
          danger: true,
          run: async () => {
            const ok = await confirm(
              `Remove ${st.name}? Its projects keep running there; this Mac just stops showing them. ` +
                `Connecting again needs a new pairing link.`,
              { title: "Remove Remote Mulpex", kind: "warning", okLabel: "Remove", cancelLabel: "Cancel" },
            );
            if (ok) await removeHost(slot);
          },
        },
      ],
    };
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
  function applySessionOrder(ids: number[], h = get(activeProjectHandle)) {
    if (h == null) return;
    reorderSessionsLocal(h, ids);
    reorderSessions(h, ids);
  }

  /** Remote Control: a drag on the phone, committed exactly like one here —
   *  tabs, or one project's rows (which need not be the project in front). The
   *  phone keeps the desktop's sibling rules, and the backend appends anything
   *  missing, so a stale list can't drop a tab or a row. */
  type RemoteReorderEvent = { handles?: ProjectHandle[] | null; handle?: ProjectHandle | null; ids?: number[] | null };
  function remoteReorder({ handles, handle, ids }: RemoteReorderEvent) {
    if (Array.isArray(handles)) {
      const open = get(projects);
      const order = handles.filter((h) => open.has(h));
      if (order.length) applyProjectOrder(order);
    } else if (handle != null && Array.isArray(ids) && get(projects).has(handle)) {
      applySessionOrder(ids, handle);
    }
  }

  /**
   * Drag-to-nest: row `from` (with its family) becomes the last child of row
   * `target` — indexes into the displayed list. Applied locally only once the
   * backend accepts: a refusal changes nothing there, so it would echo nothing
   * back to undo an optimistic repaint.
   */
  async function nestSession(from: number, target: number) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    const list = get(sessions);
    const row = list[from];
    const parent = list[target];
    if (!row || !parent) return;
    const ids = nestOrder(list, from, target);
    if (!(await reparentSession(h, row.id, parent.id, ids))) return;
    setSessionParentLocal(h, row.id, parent.id);
    reorderSessionsLocal(h, ids);
  }

  /** Drag-to-nest's inverse: nested row `from` (with its family) becomes
   *  top-level, before row `at` or at the end. Same accept-then-apply rule. */
  async function unnestSession(from: number, at: number) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    const list = get(sessions);
    const row = list[from];
    if (!row) return;
    const ids = unnestOrder(list, from, at);
    if (!(await reparentSession(h, row.id, null, ids))) return;
    setSessionParentLocal(h, row.id, null);
    reorderSessionsLocal(h, ids);
  }

  // ---- sidebar context menu (right-click) ----
  //
  // The menu acts on the row you clicked, NOT on the focused one — right-clicking
  // a background instance to rename or close it must not yank the center pane
  // away from whatever you were reading. That is also why the ⌘R/⌘M/⌘W hints are
  // conditional below: those keys act on the *focused* row, so printing them
  // beside an item aimed at a different one would advertise a key that does
  // something else.

  /** ⌘L: the project whose saves the Load list is showing, or null when closed. */
  let loadFor = $state<ProjectHandle | null>(null);
  /** File ▸ Import Docs…: the project whose import window is open. */
  let importFor = $state<ProjectHandle | null>(null);
  /** ⌘K: the claude the Secrets dialog is for, or null when closed. */
  let secretsFor = $state<{ handle: ProjectHandle; id: number } | null>(null);
  /** Mulpex ▸ Live Token…: the host (0 = this machine) whose settings are open. */
  let liveTokenHost = $state<number | null>(null);
  /** ⌘⇧O: the Remote Control dialog is open. */
  let showRemote = $state(false);
  /** ⌘⇧B: the Ask All Claudes popup is open (the round itself outlives it). */
  let showAskAll = $state(false);
  /** File ▸ Connect to Remote Mulpex…: the pairing dialog is open. */
  let showConnect = $state(false);
  /** ⌘O on a remote host: the host whose folders the picker browses. */
  let pickerHost = $state<number | null>(null);
  /** The host switcher's Rename…: which host, and the name being typed. */
  let renameHostFor = $state<{ slot: number; value: string } | null>(null);
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

  /** Context-menu thresholds for auto-compact; the backend accepts only these. */
  const AUTO_COMPACT_CHOICES = [50, 60, 70, 80];

  async function applyAutoCompact(h: ProjectHandle, id: number, pct: number | null) {
    try {
      if (await setAutoCompact(h, id, pct)) {
        setAutoCompactLocal(h, id, pct);
        autoCompactTick(h); // a child already over the line goes now
      }
    } catch (e) {
      flashToast(String(e));
    }
  }

  async function applyTodosEnabled(h: ProjectHandle, id: number, on: boolean) {
    try {
      if (await setTodosEnabled(h, id, on)) {
        setTodosOnLocal(h, id, on);
        if (on) openPanel("todo");
        flashToast(`To Do list ${on ? "on" : "off"} for claude#${id}`);
      }
    } catch (e) {
      flashToast(String(e));
    }
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
            {
              label: "Save…",
              hint: key("⌘S"),
              run: () => void saveInstance(h, s.id),
            },
            {
              label: "Secrets…",
              hint: key("⌘K"),
              run: () => openSecrets(h, s.id),
            },
            // Auto-compact this row's claudes (`compact.ts`). Only ever set here,
            // by the user — no model can turn it on. A submenu; the current
            // choice carries a ✓.
            // The To Do list (`todos.ts`) is off until the user turns it on here,
            // for this row only — an orchestrator collects its workers' asks.
            {
              label: "To Do list",
              hint: s.todos_on ? "on" : undefined,
              sub: [true, false].map((on) => ({
                label: on ? "On" : "Off",
                checked: s.todos_on === on,
                run: () => void applyTodosEnabled(h, s.id, on),
              })),
            },
            {
              label: "Auto-compact its claudes",
              hint: s.auto_compact != null ? `${s.auto_compact}%` : undefined,
              sub: ([null, ...AUTO_COMPACT_CHOICES] as (number | null)[]).map((pct) => ({
                label: pct == null ? "Off" : `${pct}%`,
                checked: s.auto_compact === pct,
                run: () => void applyAutoCompact(h, s.id, pct),
              })),
            },
          ]),
      { label: "Close", hint: key("⌘W"), danger: true, run: () => closeInstance(h, s.id) },
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

  /**
   * Focus can land on a row folded away under its parent — from a
   * notification, the palette, a reap picking the next row. Unfold whatever
   * hides it, so the visible terminal always has its row on screen.
   */
  function unfoldTo(h: ProjectHandle, id: number) {
    for (const up of collapsedAncestors(get(projects).get(h)?.sessions ?? [], id)) {
      setSessionCollapsedLocal(h, up, false);
      setSessionCollapsed(h, up, false);
    }
  }

  /** Bring one instance of any project to the front — a notification click, or
   * a row in the ⌘⇧Q checkpoint panel. Same path as clicking the sidebar row. */
  function focusInstance(handle: ProjectHandle, id: number) {
    selectProject(handle);
    unfoldTo(handle, id);
    setActiveSession(handle, id);
    focusSession(handle, id);
    terminals.focus(handle, id);
  }

  function selectSession(id: number) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    unfoldTo(h, id);
    setActiveSession(h, id);
    focusSession(h, id);
    terminals.focus(h, id);
  }

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

  /**
   * Fold or unfold a row's `hub_spawn` family. Folding away the focused row
   * hands focus to the parent, so the visible terminal always has its row on
   * screen.
   */
  function collapseSession(id: number, collapsed: boolean) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    const list = get(projects).get(h)?.sessions ?? [];
    if (collapsed && descendantsOf(list, id).some((s) => s.id === get(activeId))) {
      selectSession(id);
    }
    setSessionCollapsedLocal(h, id, collapsed);
    setSessionCollapsed(h, id, collapsed); // persists; fire-and-forget
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
  async function openOrFocusProject(path: string, host: number = 0) {
    const existing = [...get(projects).values()].find((p) => p.dir === path && slotOf(p.handle) === host);
    if (existing) {
      selectProject(existing.handle);
      return;
    }
    try {
      openError = null;
      const info = await openProject(path, host);
      await bootstrapProject(info, true);
    } catch (e) {
      // Surfaced in the picker: this is how a missing `claude` used to present
      // itself as the app silently ignoring the click.
      openError = e instanceof Error ? e.message : String(e);
      // The picker isn't on screen once anything is open.
      if (host || get(projects).size > 0) flashNotice(`Could not open ${path}: ${openError}`, 8000);
      console.error("open project failed:", e);
    }
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
  async function dropPaths(paths: string[]) {
    if (!paths.length) return;
    const h = get(activeProjectHandle);
    const id = h == null ? null : (get(projects).get(h)?.activeSessionId ?? null);
    if (h == null || id == null) {
      if (get(activeHost)) {
        flashToast("Drop files onto a claude or a terminal");
        return;
      }
      for (const path of paths) openOrFocusProject(path);
      return;
    }
    // A remote host's claude can't read this Mac's disk: the files (folders
    // too) go there first, and their paths THERE are what get pasted.
    if (isRemote(h)) {
      const slot = slotOf(h);
      flashNotice(`Uploading to ${hostName(slot)}…`, 30000);
      try {
        paths = await hostUpload(slot, paths);
        flashNotice(`Uploaded to ${hostName(slot)}`, 1500);
      } catch (e) {
        flashNotice(`Upload failed: ${e}`, 8000);
        return;
      }
    }
    // One write: split across reads, Claude can consume a lone ESC and lose
    // the paste framing (same hazard as the Shift+Enter carve-out).
    sendBytes(h, id, pastePaths(paths));
    terminals.refocus();
  }

  async function pickAndOpen() {
    // A remote host's disk has no native dialog: its own picker browses it.
    const slot = get(activeHost);
    if (slot) {
      if (!hostOnline(slot)) {
        flashToast(`${hostName(slot)} is offline`);
        return;
      }
      pickerHost = slot;
      return;
    }
    const dir = await openFolder({ directory: true, title: "Open Project" });
    if (typeof dir === "string") await openOrFocusProject(dir);
  }

  async function closeProjectHandle(handle: ProjectHandle) {
    try {
      await closeProject(handle);
    } catch (e) {
      flashToast(String(e));
      return;
    }
    dropProject(handle);
    afterHostChange(slotOf(handle));
    // None left anywhere → the picker.
    if (get(projects).size === 0 && get(hosts).size === 0) recents = await listRecentProjects();
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
    // Same row, different process: the pane is rebound to the new PTY by the
    // `session-restarted` the backend just emitted (every window holding the
    // row hears it — a remote desktop's restart included).
    terminals.refocus();
  }

  /** ⌘W / Close. Refused (toast) while a ⌘S save of that row is running. */
  function closeInstance(handle: ProjectHandle, id: number) {
    closeSession(handle, id).catch((e) => flashToast(String(e)));
  }

  /** The row's retry after a failed save: no second confirm — the user already
   *  said yes to this save. */
  async function retrySave(id: number) {
    const h = get(activeProjectHandle);
    if (h == null) return;
    try {
      await saveSession(h, id);
    } catch (e) {
      flashNotice(`Could not save claude #${id}: ${e}`, 8000);
    }
  }

  /** The prompt state that rules out typing into claude#id, as a toast; null when
   *  typing is fine. A draft is fine here: the line is appended to it. */
  function secretsBlocked(handle: ProjectHandle, id: number): string | null {
    const p = get(projects).get(handle);
    if (p?.statuses.get(id) === "needs") return "Answer or close the open question first";
    if (terminals.promptBox(handle, id) === "none") return "No prompt to type into right now";
    return null;
  }

  /** ⌘K: open the Secrets dialog for a claude (`secrets.rs`). Checked up front
   *  too, so you don't type a password only to be refused at Send. */
  function openSecrets(handle: ProjectHandle, id: number) {
    const s = get(projects)
      .get(handle)
      ?.sessions.find((x) => x.id === id);
    if (!s || s.kind === "shell" || s.exited) return;
    const why = secretsBlocked(handle, id);
    if (why) {
      flashToast(why);
      return;
    }
    secretsFor = { handle, id };
  }

  /** Type a `🔑 KEY` tag into claude#id's prompt, and no Enter: the user adds the
   *  request. The path and the rules reach the claude through the hook on every
   *  turn (`mulpex_core::secrets::context`); the tag only says which request
   *  they're for. */
  function sendSecretsTag(handle: ProjectHandle, id: number, keys: string[]) {
    const why = secretsBlocked(handle, id);
    if (why) {
      flashToast(why);
      return;
    }
    const lead = terminals.promptBox(handle, id) === "draft" ? " " : "";
    sendBytes(handle, id, new TextEncoder().encode(`${lead}🔑 ${keys.join(", ")} `));
  }

  /** Remote Control: a phone's message for claude#id (`remote/mod.rs::on_type`;
   *  over 900 bytes it is already a one-line pointer to a file). Refused like
   *  ⌘K — never onto a draft or into an open dialog — and the answer goes back to
   *  the phone. One bracketed paste plus Enter, in one write: measured to arrive
   *  exact, newlines and Hebrew included. */
  type RemoteTypeEvent = { handle: ProjectHandle; id: number; text: string; rid: string };
  function remoteType({ handle, id, text, rid }: RemoteTypeEvent) {
    const p = get(projects).get(handle);
    const s = p?.sessions.find((x) => x.id === id);
    let why: string | null = null;
    if (!p || !s || s.kind === "shell" || s.exited || s.failed) why = "That claude isn't running";
    else if (saveStatusOf(handle, id)) why = "That claude was saved and stopped";
    else if (p.statuses.get(id) === "needs") why = "Answer the open question first";
    else {
      const box = terminals.promptBox(handle, id);
      if (box === "draft") why = "There's unsent text in this claude's prompt on the Mac";
      else if (box === "none") why = "No prompt to type into right now";
    }
    // Control characters out: an ESC inside could end the paste early (`ESC[201~`)
    // and turn the rest of the message into keystrokes.
    const clean = text.replace(/[\x00-\x08\x0b-\x1f\x7f]/g, "");
    if (!why) sendBytes(handle, id, new TextEncoder().encode(`\x1b[200~${clean}\x1b[201~\r`));
    remoteReply(rid, why == null, why);
  }

  /** Remote Control: a phone answering claude#id's open question or plan. The
   *  keys were worked out in `remote/dialog.rs` (measured against a real
   *  claude); here they are played one write at a time, with a gap so the
   *  dialog keeps up — and only while the claude is still waiting on it. */
  type RemoteKeysEvent = { handle: ProjectHandle; id: number; chunks: string[]; rid: string };
  async function remoteKeys({ handle, id, chunks, rid }: RemoteKeysEvent) {
    const p = get(projects).get(handle);
    const s = p?.sessions.find((x) => x.id === id);
    if (!p || !s || s.kind === "shell" || s.exited) return remoteReply(rid, false, "That claude isn't running");
    if (p.statuses.get(id) !== "needs") return remoteReply(rid, false, "That question was already answered");
    for (const c of chunks) {
      sendBytes(handle, id, new TextEncoder().encode(c));
      await new Promise((r) => setTimeout(r, 250));
    }
    remoteReply(rid, true, null);
  }

  /** Remote Control: start a claude or terminal in a project, or close an
   *  instance. The same calls as ⌘T / ⌘⇧T / ⌘W, but aimed at the phone's
   *  project and never moving the Mac's focus — the new row just appears, the
   *  way an instance's `hub_spawn` does. The phone confirms a close itself. */
  type RemoteActionEvent = {
    action: "new" | "close" | "compact";
    handle: ProjectHandle;
    id: number | null;
    kind: "claude" | "shell" | null;
    rid: string;
  };
  async function remoteAction({ action, handle, id, kind, rid }: RemoteActionEvent) {
    const p = get(projects).get(handle);
    if (!p) return remoteReply(rid, false, "That project isn't open");
    // The phone's Compact button: ⌘⇧K on that claude, toggled.
    if (action === "compact") {
      if (id == null) return remoteReply(rid, false, "No claude to compact");
      const why = saveStatusOf(handle, id) ? "That claude was saved and stopped" : toggleCompact(handle, id);
      return remoteReply(rid, why == null, why);
    }
    if (action === "close") {
      if (id == null || !p.sessions.some((s) => s.id === id)) return remoteReply(rid, false, "Already closed");
      try {
        await closeSession(handle, id);
        remoteReply(rid, true, null);
      } catch (e) {
        remoteReply(rid, false, String(e));
      }
      return;
    }
    try {
      const info = kind === "shell" ? await createTerminal(handle) : await createSession(handle);
      const cur = get(projects).get(handle);
      if (cur && !cur.sessions.some((s) => s.id === info.id)) setSessionsFor(handle, [...cur.sessions, info]);
      remoteReply(rid, true, null, info.id);
    } catch (e) {
      remoteReply(rid, false, String(e));
    }
  }

  /** Remote Control's key bar. Arrows follow the claude's cursor-key mode, as
   *  xterm itself would send them. */
  type RemoteKeyEvent = { handle: ProjectHandle; id: number; key: string };
  function remoteKey({ handle, id, key }: RemoteKeyEvent) {
    const s = get(projects).get(handle)?.sessions.find((x) => x.id === id);
    if (!s || s.exited) return;
    const app = terminals.appCursorKeys(handle, id);
    const bytes: Record<string, string> = {
      esc: "\x1b",
      "ctrl-c": "\x03",
      enter: "\r",
      tab: "\t",
      "shift-tab": "\x1b[Z",
      up: app ? "\x1bOA" : "\x1b[A",
      down: app ? "\x1bOB" : "\x1b[B",
      right: app ? "\x1bOC" : "\x1b[C",
      left: app ? "\x1bOD" : "\x1b[D",
      "ctrl-d": "\x04",
      "ctrl-l": "\x0c",
      "ctrl-z": "\x1a",
    };
    const b = bytes[key];
    if (b) sendBytes(handle, id, new TextEncoder().encode(b));
  }

  /** ⌘⇧E: explain the focused claude's selection in the side panel's Explain tab, in simple
   *  Hebrew (`lib/explain.ts`, `explain.rs`). A hidden fork of the conversation
   *  writes it, so nothing is typed and mid-turn is fine. Claudes only — a shell
   *  has no conversation to explain from. */
  function explainSelection(handle: ProjectHandle, id: number) {
    const s = get(projects).get(handle)?.sessions.find((x) => x.id === id);
    if (!s) return;
    if (s.kind === "shell") {
      flashToast("Explain works on claudes only");
      return;
    }
    const text = terminals.selectionText(handle, id);
    if (!text) {
      flashToast("Select some text first");
      return;
    }
    startExplain(handle, id, text);
    openPanel("explain");
    terminals.refocus();
  }

  /** ⌘⇧P: add the focused claude's selection to its pins (`pins.ts`), shown in
   *  the side panel's Pins tab, opening it there; with nothing selected, show
   *  that tab — or hide the panel if it already shows it. Claudes only — a
   *  shell's rows are never restored, so a pin there could not outlive the app. */
  function pinSelection(handle: ProjectHandle, id: number) {
    const s = get(projects).get(handle)?.sessions.find((x) => x.id === id);
    if (!s) return;
    const lines = s.kind === "shell" ? null : terminals.pinSelection(handle, id);
    if (!lines) {
      if (s.kind === "shell" && terminals.hasSelection(handle, id)) {
        flashToast("Pins work on claudes only");
        return;
      }
      toggleTab("pins");
      terminals.refocus();
      return;
    }
    addPin(handle, id, lines);
    openPanel("pins");
    terminals.refocus();
  }

  /**
   * ⌘S: save one claude's work as a handoff doc in the repo (`saves.rs`), and
   * end it. Saving means the user is done with this claude, so the backend stops
   * it at once and its pane stays locked (`SaveOverlay`) until the save ends and
   * the user closes it — no wondering later whether it moved on since. Asked
   * first; refused mid-turn and over an open question, since the save is the
   * last word on the conversation.
   */
  async function saveInstance(handle: ProjectHandle, id: number) {
    const p = get(projects).get(handle);
    const s = p?.sessions.find((x) => x.id === id);
    if (!p || !s) return;
    if (s.kind === "shell") {
      flashNotice(`term #${id} can't be saved — only a claude has a conversation.`, 4000);
      return;
    }
    const sv = get(saves).get(id);
    if (sv) {
      // A row ⌘S already holds: the lock card is the way forward.
      if (sv.state === "error") void retrySave(id);
      else flashToast(sv.state === "done" ? "Already saved — close it" : "Already saving");
      return;
    }
    // The backend decides: a `working` claude whose turn is over and only has
    // background work left may be saved (`Core::save_refusal`).
    const why = await saveCheck(handle, id).catch((e) => String(e));
    if (why) {
      flashToast(why);
      return;
    }
    const who = s.name ? `claude #${id} (${s.name})` : `claude #${id}`;
    const bg = p.statuses.get(id) === "working" ? "\n\nIts background commands will be stopped too." : "";
    const ok = await confirm(`Save ${who}? It will stop now and close once saved.${bg}`, {
      title: "Save Session",
      kind: "info",
      okLabel: "Save",
      cancelLabel: "Cancel",
    });
    terminals.refocus();
    if (!ok) return;
    try {
      await saveSession(handle, id);
    } catch (e) {
      flashNotice(`Could not save claude #${id}: ${e}`, 8000);
    }
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
    const all = displayOrder(get(projects).get(h)?.sessions ?? []);
    // Rows folded under a collapsed parent are out of sight, so out of the walk.
    const hidden = hiddenIds(all);
    const list = all.filter((s) => !hidden.has(s.id));
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
    const keys = get(hostProjects).map((p) => p.handle);
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
    const order = get(hostProjects).map((p) => p.handle);
    const cur = get(activeProjectHandle);
    const from = order.findIndex((h) => h === cur);
    const to = from + delta;
    if (from < 0 || to < 0 || to >= order.length) return;
    order.splice(to, 0, ...order.splice(from, 1));
    applyProjectOrder(order);
  }

  /**
   * Slide the focused instance one row up/down in the sidebar (⌘⇧↑ / ⌘⇧↓) —
   * the Y-axis twin of `moveProject`, committing through the same path a drag
   * does, so the arrangement is persisted and ⌘[ / ⌘] cycle it in the new order.
   *
   * Clamped exactly like a drag: a row only trades places with its siblings
   * (same `hub_spawn` parent, or the other top-level rows of its block —
   * unmuted claudes / muted claudes / terminals), steps over a sibling's whole
   * family, carries its own along, and the ends are a no-op. Crossing a boundary could never stick anyway —
   * `displayOrder` re-applies on top of whatever order is committed — so a
   * clamped `to` that equals `from` is dropped here rather than round-tripping
   * an order that changes nothing.
   */
  function moveInstance(delta: number) {
    const list = get(sessions); // already in display order
    const cur = get(activeId);
    const from = list.findIndex((s) => s.id === cur);
    if (from < 0) return;
    const to = stepSlot(list, from, delta);
    if (to === from) return;
    applySessionOrder(dragOrder(list, from, to));
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
        if (h != null && cur != null) closeInstance(h, cur);
        break;
      }
      case "restart": {
        const cur = get(activeId);
        if (h != null && cur != null) await restartInstance(h, cur);
        break;
      }
      case "load_session":
        if (h != null) loadFor = h;
        break;
      case "import_docs":
        if (h != null) importFor = h;
        break;
      case "remote_control":
        showRemote = true;
        break;
      case "ask_all":
        showAskAll = true;
        break;
      case "connect_host":
        showConnect = true;
        break;
      case "next_host":
        cycleHost(1);
        break;
      case "prev_host":
        cycleHost(-1);
        break;
      case "save_session": {
        const cur = get(activeId);
        if (h != null && cur != null) await saveInstance(h, cur);
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
      case "secrets": {
        const cur = get(activeId);
        if (h != null && cur != null) openSecrets(h, cur);
        break;
      }
      case "explain": {
        const cur = get(activeId);
        if (h != null && cur != null) explainSelection(h, cur);
        break;
      }
      case "pin_selection": {
        const cur = get(activeId);
        if (h != null && cur != null) pinSelection(h, cur);
        break;
      }
      // ⌘⇧L: show/hide the side panel (To Do · Pins · Explain).
      case "toggle_panel":
        togglePanel();
        terminals.refocus();
        break;
      case "request_compact": {
        const cur = get(activeId);
        if (h == null || cur == null) break;
        if (isRemote(h)) {
          // ⌘⇧K's state machine runs in the host's own window.
          hostFrontendAction(slotOf(h), h, cur, "compact").catch((e) => flashToast(String(e)));
          break;
        }
        const why = saveStatusOf(h, cur) ? "That claude was saved and stopped" : toggleCompact(h, cur);
        if (why) flashToast(why);
        break;
      }
      case "messages":
        showMessages.update((v) => !v);
        break;
      case "minimize":
        // Custom item (muda hard-binds the predefined one to ⌘M, which is Mute).
        await getCurrentWindow().minimize();
        break;
      case "quit":
        // Linux Super+Q (lib/linuxKeys.ts). On macOS ⌘Q is the predefined menu
        // item and never reaches here.
        await quitApp();
        break;
      case "check_updates":
        // Manual: reports "up to date" and network errors too, unlike the
        // periodic check which stays silent on both.
        await checkForUpdate(true);
        break;
      case "full_disk_access":
        await openFda();
        break;
      case "live_token":
        // The settings of the machine being looked at: a remote host's tab edits that host.
        liveTokenHost = get(activeHost);
        break;
      case "checkpoint_quit":
        await startCheckpoint("quit");
        break;
      case "next":
        cycle(1);
        break;
      case "prev":
        cycle(-1);
        break;
      case "move_instance_up":
        moveInstance(-1);
        break;
      case "move_instance_down":
        moveInstance(1);
        break;
      default:
        // ⌘1–⌘9 → the Nth open project, in tab-bar order (the projects Map's
        // insertion order, which is what ProjectTabBar renders).
        if (id.startsWith("project_")) {
          const n = parseInt(id.slice("project_".length), 10);
          const h2 = get(hostProjects)[n - 1]?.handle;
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
  const PALETTE_KEEPS_FOCUS = new Set(["rename", "messages", "open_project", "remote_control", "connect_host", "ask_all"]);
  async function runPaletteAction(id: string) {
    showPalette.set(false);
    await handleMenu(id);
    if (!PALETTE_KEEPS_FOCUS.has(id)) terminals.refocus();
  }

  function onGlobalKey(e: KeyboardEvent) {
    // ⌘P / Ctrl+P toggles the command palette (projects, sessions, commands). Not a
    // menu accelerator, so it reaches the webview; preventDefault stops the print
    // dialog.
    // Not on Linux: Super+P is claimed in lib/linuxKeys.ts, and Ctrl+P there is
    // the shell's previous-history key.
    if (!IS_LINUX && (e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key.toLowerCase() === "p") {
      e.preventDefault();
      if (get(projects).size > 0 || get(hosts).size > 0) showPalette.update((v) => !v);
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
    // ⌘⌥[ / ⌘⌥] — Previous/Next Host. Declared in the menu too; Option turns
    // the bracket into another character (“ / ‘), so the physical key decides,
    // as for ⌘⇧[ / ⌘⇧] below.
    if (e.metaKey && e.altKey && !e.shiftKey && !e.ctrlKey) {
      const back = e.code === "BracketLeft";
      if (back || e.code === "BracketRight") {
        e.preventDefault();
        cycleHost(back ? -1 : 1);
        return;
      }
    }
    if (e.metaKey && e.shiftKey && !e.altKey && !e.ctrlKey) {
      // ⌘⇧P — Pin Selection. Declared in the menu too; if the menu takes the
      // key it never reaches here, so this cannot double-fire (see below).
      if (e.code === "KeyP") {
        e.preventDefault();
        void handleMenu("pin_selection");
        return;
      }
      // ⌘⇧E — Explain Selection, claimed here for the same reason.
      if (e.code === "KeyE") {
        e.preventDefault();
        void handleMenu("explain");
        return;
      }
      // ⌘⇧L — Show/Hide Side Panel, claimed here for the same reason.
      if (e.code === "KeyL") {
        e.preventDefault();
        void handleMenu("toggle_panel");
        return;
      }
      // ⌘⇧K — Request Compact, claimed here for the same reason.
      if (e.code === "KeyK") {
        e.preventDefault();
        void handleMenu("request_compact");
        return;
      }
      // ⌘⇧B — Ask All Claudes, likewise.
      if (e.code === "KeyB") {
        e.preventDefault();
        void handleMenu("ask_all");
        return;
      }
      // ⌘⇧Q — Checkpoint & Quit, likewise.
      if (e.code === "KeyQ") {
        e.preventDefault();
        void handleMenu("checkpoint_quit");
        return;
      }
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
        return;
      }
      // ⌘⇧↑ / ⌘⇧↓ — Move Instance Up/Down, the same story one axis over:
      // declared in the menu, but with the terminal focused the keys are
      // AppKit's `moveUp/DownAndModifySelection:` on xterm's hidden textarea
      // and never fall through to the main menu.
      const up = e.code === "ArrowUp";
      if ((up || e.code === "ArrowDown") && !inTextField(e.target)) {
        e.preventDefault();
        moveInstance(up ? -1 : 1);
      }
    }
  }

  /**
   * A field where ⌘⇧ + an arrow really is a selection gesture — the rename box,
   * the palette's query — so the shortcut must be left alone there.
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
      listen<RemoteTypeEvent>("remote-type", (e) => remoteType(e.payload)),
      listen<RemoteKeyEvent>("remote-key", (e) => remoteKey(e.payload)),
      listen<RemoteKeysEvent>("remote-keys", (e) => remoteKeys(e.payload)),
      listen<RemoteActionEvent>("remote-action", (e) => remoteAction(e.payload)),
      listen<RemoteReorderEvent>("remote-reorder", (e) => remoteReorder(e.payload)),
      listen<HubUpdateEvent>("hub-update", (e) => {
        applyHubFor(e.payload.handle, e.payload.snapshot);
        autoCompactTick(e.payload.handle);
      }),
      listen<SaveProgressEvent>("save-progress", (e) => applySaveProgress(e.payload)),
      // Every window holding a restarted row rebinds it from this one event:
      // this Mulpex's own panes reattach to the new PTY; a remote host's mirror
      // is re-snapshotted by its host instead.
      listen<SessionExitedEvent>("session-restarted", (e) => {
        dropCompact(e.payload.handle, e.payload.id);
        if (!isRemote(e.payload.handle)) terminals.reattach(e.payload.handle, e.payload.id);
      }),
      listen<SessionExitedEvent>("pins-changed", (e) => void reloadPins(e.payload.handle, e.payload.id)),
      // A To Do list changed — by its claude, a remote desktop, the phone, or an UPDATE landing.
      listen<SessionExitedEvent>("todos-changed", (e) => void reloadTodos(e.payload.handle, e.payload.id)),
      // A remote desktop opened, closed or reordered a project here.
      listen<WorkspaceInfo>("projects-changed", (e) => void applyWorkspace(0, e.payload)),
      // A remote desktop starting to mirror one of this Mulpex's panes.
      listen<{ rid: string; handle: ProjectHandle; id: number }>("host-serialize", async (e) => {
        const { rid, handle, id } = e.payload;
        const r = await terminals.serialize(handle, id);
        void remoteSnapshotReply(rid, r?.data ?? null, r?.offset ?? 0, r?.cols ?? 0, r?.rows ?? 0);
      }),
      // A remote desktop took (or gave back) this Mulpex's geometry.
      listen<{ cols: number; rows: number; owner: string | null }>("geometry-changed", (e) => {
        const { cols, rows, owner } = e.payload;
        geometryDriver.set(owner);
        terminals.setLock(owner ? { cols, rows } : null);
      }),
      listen<ExplainProgressEvent>("explain-progress", (e) => applyExplainProgress(e.payload)),
      listen<BroadcastSummaryEvent>("broadcast-summary", (e) => applySummaryEvent(e.payload)),
      listen<SessionExitedEvent>("session-exited", (e) => {
        terminals.dispose(e.payload.handle, e.payload.id);
        clearSaveState(e.payload.handle, e.payload.id);
        dropPins(e.payload.handle, e.payload.id);
        dropTodos(e.payload.handle, e.payload.id);
        dropExplains(e.payload.handle, e.payload.id);
        dropCompact(e.payload.handle, e.payload.id);
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

    // Headless testing only (`MULPEX_DEBUG_DUMP`): every pane's buffer to disk.
    let dumpTimer: ReturnType<typeof setInterval> | undefined;
    const driveLog: string[] = [];
    void invoke<boolean>("debug_dump_enabled").then((on) => {
      if (!on) return;
      dumpTimer = setInterval(async () => {
        for (const d of terminals.dumpAll()) void invoke("debug_dump", { name: d.key, text: d.text });
        // What the window shows, for a test that can't look at it.
        const ui = {
          activeHost: get(activeHost),
          activeProject: get(activeProjectHandle),
          activeSession: get(activeId),
          hosts: [...get(hosts).values()].map((h) => ({ slot: h.slot, name: h.name, state: h.state, error: h.error, geo: h.geo, update: h.update })),
          projects: [...get(projects).values()].map((p) => ({ handle: p.handle, name: p.name, dir: p.dir, sessions: p.sessions.map((s) => s.id) })),
          text: [...document.querySelectorAll(".empty, .banner, .toast, .notice, [role=dialog]")].map((e) => e.textContent?.trim()),
          dialogs: { pickerHost, showConnect, renameHostFor: renameHostFor?.slot ?? null },
        };
        void invoke("debug_dump", { name: "ui", text: JSON.stringify(ui, null, 1) });
        // One queued action per tick: a click (by selector and text), a menu
        // id, or text typed into the focused field.
        const raw = await invoke<string | null>("debug_drive_take");
        if (!raw) return;
        const a = JSON.parse(raw);
        try {
          await drive(a);
          driveLog.push(`ok ${raw}`);
          void invoke("debug_dump", { name: "drive-log", text: driveLog.join("\n") });
        } catch (e) {
          void invoke("debug_dump", { name: "drive-error", text: String((e as Error)?.stack ?? e) });
        }
      }, 700);
    });
    async function drive(a: any) {
        if (a.menu) await handleMenu(a.menu);
        if (a.click) {
          const el = [...document.querySelectorAll<HTMLElement>(a.click)].find((e) => !a.text || e.textContent?.includes(a.text));
          if (a.context) el?.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, clientX: 10, clientY: 10 }));
          else el?.click();
        }
        if (a.type != null) {
          const el = document.activeElement as HTMLInputElement | HTMLTextAreaElement | null;
          if (el && "value" in el) {
            el.value = a.type;
            el.dispatchEvent(new Event("input", { bubbles: true }));
          }
        }
        if (a.invoke) {
          const r = await Promise.race([
            invoke(a.invoke, a.args ?? {}).then((v) => `ok ${JSON.stringify(v)}`, (e) => `err ${e}`),
            new Promise((res) => setTimeout(() => res("timeout 3s"), 3000)),
          ]);
          driveLog.push(`invoke ${a.invoke} → ${r}`);
        }
        if (a.termInput != null) terminals.inputActive(a.termInput);
        if (a.rawInput != null) {
          const h = get(activeProjectHandle);
          const id = get(activeId);
          driveLog.push(`rawInput h=${h} id=${id}`);
          if (h != null && id != null) {
            const r = await Promise.race([
              sendBytes(h, id, new TextEncoder().encode(a.rawInput)).then(() => "ok", (e) => `err ${e}`),
              new Promise((res) => setTimeout(() => res("timeout 3s"), 3000)),
            ]);
            driveLog.push(`sendBytes → ${r}`);
          }
        }
        if (a.key) (document.activeElement ?? window).dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, ...a.key }));
    }

    const onResize = () => terminals.refit();
    window.addEventListener("resize", onResize);

    // Check at startup, then every 6h. Silent unless something is actually
    // available — a failed check never surfaces on this path.
    const stopUpdateChecks = startUpdateChecks();
    void checkFda();

    // Dock badge + "a claude needs you" notifications. Clicking a banner routes
    // through the same select path as clicking the sidebar row, so it lands on
    // the pane with the question rather than merely raising the window.
    let stopAttention: (() => void) | null = null;
    let stopHosts: (() => void) | null = null;
    initAttention(focusInstance).then((stop) => {
      stopAttention = stop;
    });

    // Linux has no native menu: the webview claims the Super shortcuts itself.
    // Loaded only there, so macOS never even fetches the module.
    let stopLinuxKeys: (() => void) | undefined;
    if (IS_LINUX) {
      void import("./lib/linuxKeys").then(({ installLinuxKeys }) => {
        stopLinuxKeys = installLinuxKeys(
          (id) => void handleMenu(id),
          () => {
            if (get(projects).size > 0 || get(hosts).size > 0) showPalette.update((v) => !v);
          },
        );
      });
    }

    (async () => {
      const ws = await bootstrap();
      // BEFORE any TerminalView mounts. Restored sessions' PTYs have been running
      // since before the window painted, and `attach_session` flushes everything
      // they printed — so each xterm has to be built at the size those bytes were
      // rendered for. Building at xterm's 80x24 default and resizing afterwards
      // corrupts the pane permanently; see `terminals.ts::create`.
      terminals.setGeometry(ws.cols, ws.rows);
      if (ws.geometry_owner) {
        geometryDriver.set(ws.geometry_owner);
        terminals.setLock({ cols: ws.cols, rows: ws.rows });
      }
      if (ws.projects.length) {
        for (const info of ws.projects) {
          await bootstrapProject(info, info.handle === ws.active);
        }
      } else {
        recents = await listRecentProjects();
      }
      // Remote hosts: their cached tabs now, each one's live ones as it connects.
      stopHosts = await initHosts({ bootstrapProject, dropProject, afterChange: afterHostChange });
      if (!ws.projects.length && get(hosts).size > 0) selectHost([...get(hosts).keys()].sort((a, b) => a - b)[0]);
      ready = true;
      // After a ⌘⇧Q Checkpoint & Quit: offer to wake the claudes that stopped.
      void checkResume();
      // Non-blocking: the probe shells out to the login shell, so let the UI
      // paint first. Only matters for the picker's banner.
      claude = await claudeStatus();
    })();

    return () => {
      window.removeEventListener("resize", onResize);
      clearInterval(dumpTimer);
      stopUpdateChecks();
      stopAttention?.();
      stopHosts?.();
      stopLinuxKeys?.();
      unlisteners.forEach((p) => p.then((f) => f()));
    };
  });
</script>

<svelte:window onkeydown={onGlobalKey} />

{#if IS_LINUX}
  <!-- Linux has no native menu; this is the Mac's, in the window. -->
  <MenuBar
    onmenu={(id) => void handleMenu(id)}
    onedit={(kind) => void import("./lib/linuxKeys").then((m) => m.editAction(kind))}
  />
{/if}

{#if ready && ($projects.size > 0 || $hosts.size > 0)}
  <div class="shell">
    <ProjectTabBar
      onselect={selectProject}
      onclose={closeProjectHandle}
      onadd={pickAndOpen}
      onreorder={applyProjectOrder}
      onhost={selectHost}
      onhostmenu={openHostMenu}
    />
    <TopBar onremote={() => (showRemote = true)} />
    <aside class="sidebar">
      <InstanceList
        onselect={selectSession}
        onmute={muteSession}
        oncollapse={collapseSession}
        onreorder={applySessionOrder}
        onnest={(from, target) => void nestSession(from, target)}
        onunnest={(from, at) => void unnestSession(from, at)}
        oncontext={openRowMenu}
        oncontextempty={openEmptyMenu}
        onsaveretry={retrySave}
      />
      <HubPanel />
    </aside>
    <main class="pane">
      <TerminalPane
        onsaveretry={retrySave}
        onsaveclose={(id) => {
          const h = get(activeProjectHandle);
          if (h != null) closeInstance(h, id);
        }}
      />
    </main>
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
  {#if importFor != null}
    <ImportDialog
      handle={importFor}
      onclose={() => {
        importFor = null;
        terminals.refocus();
      }}
      onapplied={(r) => {
        importFor = null;
        terminals.refocus();
        const parts = [
          r.saves && `${r.saves} save(s)`,
          r.guides && `${r.guides} guide(s)`,
          r.deleted && `${r.deleted} deleted`,
          r.skipped && `${r.skipped} skipped`,
        ].filter(Boolean);
        const bad = r.errors.length > 0 || r.commit_error != null || r.leftovers.length > 0;
        flashNotice(
          `Imported: ${parts.join(", ") || "nothing"}.` +
            (r.errors.length ? ` ${r.errors.length} failed: ${r.errors.join("; ")}.` : "") +
            (r.commit ? ` Committed ${r.commit}.` : "") +
            (r.commit_error ? ` NOT committed (files are changed): ${r.commit_error}` : "") +
            (r.leftovers.length
              ? ` Still pointing at a deleted doc: ${r.leftovers.join("; ")}`
              : ""),
          bad ? 15000 : 8000,
        );
      }}
    />
  {/if}
  {#if pickerHost != null}
    <RemoteProjectPicker
      host={pickerHost}
      onclose={() => {
        pickerHost = null;
        terminals.refocus();
      }}
      onopen={(path, host) => {
        // The host comes from the picker itself: closing it first would
        // leave nothing to read it from, and the project opened on this Mac.
        pickerHost = null;
        void openOrFocusProject(path, host);
      }}
    />
  {/if}
  {#if renameHostFor}
    <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
    <div class="host-rename" onclick={() => (renameHostFor = null)}>
      <!-- svelte-ignore a11y_autofocus -->
      <input
        autofocus
        value={renameHostFor.value}
        onclick={(e) => e.stopPropagation()}
        onkeydown={(e) => {
          const t = renameHostFor;
          if (e.key === "Escape") renameHostFor = null;
          if (e.key === "Enter" && t) {
            void renameHost(t.slot, (e.currentTarget as HTMLInputElement).value);
            renameHostFor = null;
            terminals.refocus();
          }
        }}
      />
    </div>
  {/if}
  {#if showRemote}
    <RemoteDialog
      onclose={() => {
        showRemote = false;
        terminals.refocus();
      }}
    />
  {/if}
  {#if showAskAll}
    <BroadcastDialog
      onclose={() => {
        showAskAll = false;
        terminals.refocus();
      }}
      onfocus={(h, id) => {
        showAskAll = false;
        focusInstance(h, id);
      }}
    />
  {/if}
  {#if showConnect}
    <ConnectHostDialog
      onclose={() => {
        showConnect = false;
        terminals.refocus();
      }}
      onpaired={(st) => {
        showConnect = false;
        selectHost(st.slot);
      }}
    />
  {/if}
  {#if secretsFor}
    <SecretsDialog
      handle={secretsFor.handle}
      id={secretsFor.id}
      onclose={() => {
        secretsFor = null;
        terminals.refocus();
      }}
      onsent={(keys) => {
        const t = secretsFor;
        secretsFor = null;
        if (t) sendSecretsTag(t.handle, t.id, keys);
        terminals.refocus();
      }}
    />
  {/if}
  {#if liveTokenHost != null}
    <LiveTokenDialog
      host={liveTokenHost}
      hostLabel={liveTokenHost > 0 ? hostName(liveTokenHost) : null}
      onclose={() => {
        liveTokenHost = null;
        terminals.refocus();
      }}
    />
  {/if}
  {#if loadFor != null}
    <LoadDialog
      handle={loadFor}
      onclose={() => {
        loadFor = null;
        terminals.refocus();
      }}
      onloaded={async ({ info, existing }) => {
        const h = loadFor;
        loadFor = null;
        if (h == null) return;
        // A new row: same as ⌘T's tail — add it, then focus once it has mounted.
        // An already-open conversation: just focus its row.
        if (!existing) {
          const p = get(projects).get(h);
          setSessionsFor(h, [...(p?.sessions ?? []), info]);
          await tick();
        }
        selectSession(info.id);
      }}
    />
  {/if}
  {#if ctx}
    <ContextMenu x={ctx.x} y={ctx.y} items={ctx.items} onclose={closeContextMenu} />
  {/if}
{:else if ready}
  {#if showConnect}
    <ConnectHostDialog
      onclose={() => (showConnect = false)}
      onpaired={(st) => {
        showConnect = false;
        selectHost(st.slot);
      }}
    />
  {/if}
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
<FdaBanner />
{#if $checkpointOpen}
  <CheckpointPanel onfocus={focusInstance} />
{/if}
<ResumeDialog
  onwoke={(n) => flashToast(`Waking ${n} ${n === 1 ? "claude" : "claudes"} as each one comes up…`)}
/>

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
  .host-rename {
    position: fixed;
    inset: 0;
    z-index: 60;
    display: grid;
    place-items: start center;
    padding-top: 4rem;
    background: rgba(0, 0, 0, 0.4);
  }
  .host-rename input {
    width: min(20rem, 80vw);
    padding: 0.5rem 0.6rem;
    background: var(--bg-elev);
    color: var(--text);
    border: 1px solid var(--border-focus);
    border-radius: 6px;
    font: inherit;
  }
  .pane {
    grid-area: pane;
    min-width: 0;
    min-height: 0;
    background: var(--bg);
  }
</style>
