//! The native macOS menu bar. This is the clean replacement for the old TUI's
//! Ctrl/Kitty keybinding minefield: ⌘-accelerators are intercepted by the menu
//! before they ever reach xterm, and Claude never uses ⌘, so there's zero
//! collision. Plain keys (arrows, Ctrl+C, Esc, Shift+Enter) fall straight through
//! to the focused terminal.
//!
//! Custom items carry a stable id; `on_menu_event` (wired in `lib.rs`) forwards
//! the id to the frontend as a `menu` event, which performs the action.

use tauri::menu::{Menu, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};
use tauri::{AppHandle, Runtime};

/// Build the full menu. Item ids match the strings the frontend switches on.
pub fn build<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    // App menu (Mulpex): About + Quit (⌘Q → triggers our RunEvent teardown).
    let app_menu = SubmenuBuilder::new(app, "Mulpex")
        .about(None)
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

    // Session: rename, message reader, focus navigation.
    let rename = MenuItemBuilder::with_id("rename", "Rename Session…")
        .accelerator("Cmd+R")
        .build(app)?;
    let messages = MenuItemBuilder::with_id("messages", "Messages")
        .accelerator("Cmd+M")
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
        .item(&messages)
        .separator()
        .item(&next)
        .item(&prev)
        .build()?;

    // Window: standard minimize/zoom.
    let window_menu = SubmenuBuilder::new(app, "Window")
        .item(&PredefinedMenuItem::minimize(app, None)?)
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
