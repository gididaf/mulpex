// Full Disk Access banner state. Checked once at launch: macOS applies the grant
// only when Mulpex relaunches, so re-checking mid-run can never flip it.
import { writable } from "svelte/store";
import { hasFullDiskAccess, openFullDiskAccessSettings } from "./ipc";

/** `missing` shows the banner; `requested` = Settings was opened, restart needed;
 *  `hidden` = granted, dismissed for this launch, or not checked yet. */
export type FdaState = "hidden" | "missing" | "requested";

export const fdaState = writable<FdaState>("hidden");
let granted = true;

export async function checkFda() {
  try {
    granted = await hasFullDiskAccess();
  } catch {
    return; // Unknown is not "missing" — never nag on a failed probe.
  }
  if (!granted) fdaState.set("missing");
}

/** Banner button and the menu item. The menu item stays usable when granted
 *  (to revisit the pane) and then shows nothing. */
export async function openFda() {
  try {
    await openFullDiskAccessSettings();
  } catch (e) {
    console.error(e);
    return;
  }
  if (!granted) fdaState.set("requested");
}

/** ✕: gone until the next launch. */
export function dismissFda() {
  fdaState.set("hidden");
}
