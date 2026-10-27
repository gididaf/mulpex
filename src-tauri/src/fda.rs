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

/// Files only Full Disk Access can read, so opening one is the probe. `stat`
/// succeeds without the grant, so it has to be an actual open (a directory has to
/// be listed). Only changes after a relaunch: macOS applies the grant at launch.
///
/// A list, not one file: macOS 27 removed the per-user TCC.db that used to be
/// the only probe, and Mulpex then nagged everyone who had the grant. Granted if
/// any of them opens; if none of them exists, we can't tell, and say granted — a
/// banner that lies is worse than none.
#[cfg(target_os = "macos")]
#[tauri::command]
pub fn has_full_disk_access() -> bool {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let probes = [
        PathBuf::from("/Library/Application Support/com.apple.TCC/TCC.db"),
        home.join("Library/Application Support/com.apple.TCC/TCC.db"),
        home.join("Library/Safari"),
    ];
    let mut any_exists = false;
    for p in &probes {
        let opened = if p.is_dir() {
            std::fs::read_dir(p).and_then(|mut d| d.next().transpose()).map(|_| ())
        } else {
            std::fs::File::open(p).map(|_| ())
        };
        match opened {
            Ok(()) => return true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => any_exists = true,
        }
    }
    !any_exists
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
