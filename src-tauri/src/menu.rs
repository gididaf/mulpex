//! The native macOS menu bar. This is the clean replacement for the old TUI's
//! Ctrl/Kitty keybinding minefield: ⌘-accelerators are intercepted by the menu
//! before they ever reach xterm, and Claude never uses ⌘, so there's zero
//! collision. Plain keys (arrows, Ctrl+C, Esc, Shift+Enter) fall straight through
//! to the focused terminal.
//!
//! Custom items carry a stable id; `on_menu_event` (wired in `lib.rs`) forwards
//! the id to the frontend as a `menu` event, which performs the action.

use tauri::menu::{
    CheckMenuItemBuilder, Menu, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder,
};
use tauri::{AppHandle, Runtime};

/// Id of the "Mute Session" check item, shared with the command that ticks it.
pub const MUTE_ITEM_ID: &str = "mute";

/// Tick or untick "Mute Session" so the menu reflects the focused session.
///
/// `Menu::get` only looks at the menu's own children, and the item lives inside
/// the Session submenu, so this walks one level down. Best-effort: a missing menu
/// (no window yet) just does nothing.
pub fn set_mute_checked<R: Runtime>(app: &AppHandle<R>, checked: bool) {
    let Some(menu) = app.menu() else { return };
    let Ok(items) = menu.items() else { return };
    for item in items {
        let Some(submenu) = item.as_submenu() else {
            continue;
        };
        if let Some(found) = submenu.get(MUTE_ITEM_ID) {
            if let Some(check) = found.as_check_menuitem() {
                let _ = check.set_checked(checked);
            }
            return;
        }
    }
}

/// Build the full menu. Item ids match the strings the frontend switches on.
pub fn build<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    // App menu (Mulpex): About + Quit (⌘Q → triggers our RunEvent teardown).
    // No accelerator: the periodic check is the normal path, this is the manual
    // "is there one right now?" escape hatch, and every free ⌘-key is worth more
    // to a terminal app than to a menu item you use twice a month.
    let check_updates = MenuItemBuilder::with_id("check_updates", "Check for Updates…").build(app)?;
    let app_menu = SubmenuBuilder::new(app, "Mulpex")
        .about(None)
        .item(&check_updates)
        .separator()
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .quit()
        .build()?;

    // File: open/close a project, project navigation, new/close session.
    let open_project = MenuItemBuilder::with_id("open_project", "Open Project…")
        .accelerator("Cmd+O")
        .build(app)?;
    let close_project = MenuItemBuilder::with_id("close_project", "Close Project")
        .accelerator("Cmd+Shift+W")
        .build(app)?;
    let next_project = MenuItemBuilder::with_id("next_project", "Next Project")
        .accelerator("Cmd+Shift+]")
        .build(app)?;
    let prev_project = MenuItemBuilder::with_id("prev_project", "Previous Project")
        .accelerator("Cmd+Shift+[")
        .build(app)?;
    let new_session = MenuItemBuilder::with_id("new_session", "New Session")
        .accelerator("Cmd+T")
        .build(app)?;
    let close_session = MenuItemBuilder::with_id("close_session", "Close Session")
        .accelerator("Cmd+W")
        .build(app)?;
    let mut file_builder = SubmenuBuilder::new(app, "File")
        .item(&open_project)
        .item(&close_project)
        .separator()
        .item(&next_project)
        .item(&prev_project)
        .separator();
    // ⌘1–⌘9 switch to the Nth open project (tab-bar order), matching the
    // browser/terminal convention where ⌘N selects a tab.
    for n in 1..=9u8 {
        let item = MenuItemBuilder::with_id(format!("project_{n}"), format!("Project {n}"))
            .accelerator(format!("Cmd+{n}"))
            .build(app)?;
        file_builder = file_builder.item(&item);
    }
    let file_menu = file_builder
        .separator()
        .item(&new_session)
        .item(&close_session)
        .build()?;

    // Edit: the predefined items macOS routes to the focused xterm textarea.
    let edit_menu = SubmenuBuilder::new(app, "Edit")
        .item(&PredefinedMenuItem::undo(app, None)?)
        .item(&PredefinedMenuItem::redo(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::cut(app, None)?)
        .item(&PredefinedMenuItem::copy(app, None)?)
        .item(&PredefinedMenuItem::paste(app, None)?)
        .item(&PredefinedMenuItem::select_all(app, None)?)
        .build()?;

    // Session: rename, mute, message reader, focus navigation.
    let rename = MenuItemBuilder::with_id("rename", "Rename Session…")
        .accelerator("Cmd+R")
        .build(app)?;
    // A check item so the menu reports the *active* session's state; the
    // frontend keeps the tick in sync via `set_mute_menu_checked` whenever the
    // focus or the flag changes.
    let mute = CheckMenuItemBuilder::with_id(MUTE_ITEM_ID, "Mute Session")
        .accelerator("Cmd+M")
        .checked(false)
        .build(app)?;
    // Messages moved off ⌘M to make room for mute (⌘⇧M).
    let messages = MenuItemBuilder::with_id("messages", "Messages")
        .accelerator("Cmd+Shift+M")
        .build(app)?;
    let next = MenuItemBuilder::with_id("next", "Next Session")
        .accelerator("Cmd+]")
        .build(app)?;
    let prev = MenuItemBuilder::with_id("prev", "Previous Session")
        .accelerator("Cmd+[")
        .build(app)?;
    // Sessions are navigated with ⌘[ / ⌘] only — ⌘1–9 belong to projects.
    let session_menu = SubmenuBuilder::new(app, "Session")
        .item(&rename)
        .item(&mute)
        .item(&messages)
        .separator()
        .item(&next)
        .item(&prev)
        .build()?;

    // Window: standard minimize/zoom. Minimize is a *custom* item rather than the
    // predefined one because muda hard-binds that to ⌘M with no way to override
    // the accelerator — and ⌘M is Mute Session here. Two items on one key means
    // the earlier menu (Session) silently wins while the Window menu goes on
    // advertising ⌘M, so this trades the shortcut for a menu that doesn't lie.
    // The item still minimizes; the frontend handles the id.
    let minimize = MenuItemBuilder::with_id("minimize", "Minimize").build(app)?;
    let window_menu = SubmenuBuilder::new(app, "Window")
        .item(&minimize)
        .item(&PredefinedMenuItem::maximize(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::close_window(app, None)?)
        .build()?;

    MenuBuilder::new(app)
        .item(&app_menu)
        .item(&file_menu)
        .item(&edit_menu)
        .item(&session_menu)
        .item(&window_menu)
        .build()
}
