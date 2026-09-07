//! Where things sit on screen.
//!
//! One place, because the layout is a contract several callers have to agree on:
//! every instance window is `[ sidebar | instance ]` with the sidebar at pane
//! index 0, and `sidebar.rs` jumps to `<window>.1` on Enter because of it. A
//! window created without its sidebar would look fine until the first jump.

use crate::core::Project;
use crate::tmux::Tmux;

/// Put the sidebar into a freshly created instance window.
///
/// Best-effort on purpose. A window with no sidebar is a usable instance with a
/// missing list; a spawn that *fails* because its decoration did not attach is a
/// lost claude. The list is a view, and a view is never worth the thing it views.
pub fn attach_sidebar(t: &Tmux, p: &Project, window: &str) {
    let Ok(me) = std::env::current_exe() else { return };
    let argv = vec![
        me.to_string_lossy().to_string(),
        "sidebar".into(),
        p.dir.to_string_lossy().to_string(),
    ];
    match t.split_sidebar(window, &p.dir, &argv) {
        // Tag the pane so the scan can skip it. The sidebar lives in the
        // instance's own window and inherits its `@mpx_id`/`@mpx_kind`, so without
        // a mark of its own every instance is listed **twice** — once for itself
        // and once for the strip that draws it. A pane option rather than a pane
        // index, because a window whose sidebar failed to attach still has to read
        // correctly as one instance.
        Ok(pane) => {
            let _ = t.set_pane_option(&pane, SIDEBAR_OPT, "1");
        }
        Err(e) => eprintln!("mpx: could not attach the sidebar to {window}: {e:#}"),
    }
}

/// Marks the pane that draws the list, as opposed to the pane that is the
/// instance. Read by `core::scan`.
pub const SIDEBAR_OPT: &str = "@mpx_side";

#[cfg(test)]
mod tests {
    use super::*;

    /// The sidebar shares its window with the instance and inherits that window's
    /// options, so it needs a mark of its own or `core::scan` counts it as a
    /// second instance. Observed: every row listed twice.
    #[test]
    fn the_sidebar_pane_is_marked_so_the_scan_can_skip_it() {
        assert_eq!(SIDEBAR_OPT, "@mpx_side");
        assert!(SIDEBAR_OPT.starts_with('@'), "tmux user options begin with @");
    }
}
