//! Full Disk Access: detect it, and open the System Settings pane that grants it.
//!
//! macOS charges everything a child `claude` or shell touches to Mulpex, so a
//! claude reading another app's container pops "Mulpex.app would like to access
//! data from other apps" — a modal on the Mac that blocks that claude until
//! someone clicks it, which nobody on Remote Control can. Full Disk Access
//! covers that prompt (and Documents/Desktop/Downloads). No app can grant it to
//! itself; the best we can do is notice it's missing and open the right pane.

#[cfg(target_os = "macos")]
use std::path::PathBuf;

/// The TCC database is readable only with Full Disk Access, which makes opening
/// it the standard probe. `stat` succeeds without the grant, so it has to be an
/// actual open. Only changes after a relaunch: macOS applies the grant at launch.
#[cfg(target_os = "macos")]
#[tauri::command]
pub fn has_full_disk_access() -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    let db = PathBuf::from(home).join("Library/Application Support/com.apple.TCC/TCC.db");
    std::fs::File::open(db).is_ok()
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub fn open_full_disk_access_settings() -> Result<(), String> {
    std::process::Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
        .status()
        .map_err(|e| format!("could not open System Settings: {e}"))
        .and_then(|s| {
            if s.success() {
                Ok(())
            } else {
                Err(format!("could not open System Settings ({s})"))
            }
        })
}

// No TCC anywhere else: there is nothing to grant, so report it granted and the
// banner never appears.
#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub fn has_full_disk_access() -> bool {
    true
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub fn open_full_disk_access_settings() -> Result<(), String> {
    Err("Full Disk Access is a macOS setting; there is nothing to grant here.".into())
}
