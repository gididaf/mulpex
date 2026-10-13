// The side panel: one column right of the terminal with three fixed tabs —
// For me · Pins · Explain. Tabs never close; only the whole panel shows or
// hides (⌘⇧L, or the ✕ in its header).
//
// One switch for the whole app, not per instance: every PTY shares one
// geometry (docs/rendering.md), so a column that came and went with the focused
// instance would resize every claude on each switch. `frac` is its share of the
// pane's width. Remembered in localStorage — a per-viewer nicety.

import { writable } from "svelte/store";

export type PanelTab = "forme" | "pins" | "explain";

export interface RightPanel {
  open: boolean;
  frac: number;
  tab: PanelTab;
}

const KEY = "mulpex.rightPanel";
/** The pins panel's own key, before it became a tab; read once to migrate. */
const OLD_KEY = "mulpex.pinsPanel";
export const PANEL_MIN_FRAC = 0.15;
export const PANEL_MAX_FRAC = 0.6;
export const clampFrac = (f: number) => Math.min(PANEL_MAX_FRAC, Math.max(PANEL_MIN_FRAC, f));

const TABS: PanelTab[] = ["forme", "pins", "explain"];

function load(): RightPanel {
  const def: RightPanel = { open: false, frac: 0.3, tab: "forme" };
  try {
    const v = JSON.parse(localStorage.getItem(KEY) ?? "null");
    if (v && typeof v.open === "boolean" && typeof v.frac === "number") {
      return { open: v.open, frac: clampFrac(v.frac), tab: TABS.includes(v.tab) ? v.tab : "forme" };
    }
    const old = JSON.parse(localStorage.getItem(OLD_KEY) ?? "null");
    if (old && typeof old.open === "boolean" && typeof old.frac === "number") {
      return { open: old.open, frac: clampFrac(old.frac), tab: "pins" };
    }
  } catch {
    /* no storage: defaults */
  }
  return def;
}

export const rightPanel = writable<RightPanel>(load());
rightPanel.subscribe((v) => {
  try {
    localStorage.setItem(KEY, JSON.stringify(v));
  } catch {
    /* not remembered, still works */
  }
});

/** Show the panel on `tab`. */
export const openPanel = (tab: PanelTab) => rightPanel.update((p) => ({ ...p, open: true, tab }));
export const setPanelOpen = (open: boolean) => rightPanel.update((p) => ({ ...p, open }));
export const togglePanel = () => rightPanel.update((p) => ({ ...p, open: !p.open }));
export const setTab = (tab: PanelTab) => rightPanel.update((p) => ({ ...p, tab }));
export const setPanelFrac = (frac: number) =>
  rightPanel.update((p) => ({ ...p, frac: clampFrac(frac) }));

/** Show `tab`, or — when the panel is already showing it — hide the panel.
 *  ⌘⇧P with nothing selected. */
export const toggleTab = (tab: PanelTab) =>
  rightPanel.update((p) => (p.open && p.tab === tab ? { ...p, open: false } : { ...p, open: true, tab }));
