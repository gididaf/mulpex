//! The page's web process dying under a running app.
//!
//! The window is a webview whose page runs in a separate process. When that
//! process dies — on Linux WebKitGTK kills it once it passes a built-in memory
//! limit (4 GB measured, `Unable to shrink memory footprint … Killed`) — the
//! Rust side and the PTY daemon keep running, so every claude carries on and
//! even Remote Control still lists them, but nothing that needs the page works:
//! a message typed from the phone is handed to the page, nobody answers, and
//! the phone says "No answer". Nothing used to bring the page back.
//!
//! Now the page is reloaded, and `pty.rs::OutputSink::attach` rebuilds each
//! pane from the ring when the fresh page attaches again. A page that keeps
//! dying is not reloaded forever: the third crash inside five minutes shows
//! `crashed.html` instead.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many crashes inside `WINDOW` end the reloading.
const MAX: usize = 3;
const WINDOW: Duration = Duration::from_secs(5 * 60);

static CRASHES: Mutex<Vec<Instant>> = Mutex::new(Vec::new());

/// Count a crash at `now`; whether the page should be reloaded (false: it is
/// crash-looping, show the error page).
fn should_reload(crashes: &mut Vec<Instant>, now: Instant) -> bool {
    crashes.retain(|t| now.duration_since(*t) < WINDOW);
    crashes.push(now);
    crashes.len() < MAX
}

/// The page died: reload it, or show the error page if it keeps dying.
pub fn on_page_crash<R: tauri::Runtime>(webview: &tauri::Webview<R>, why: &str) {
    let reload = should_reload(&mut CRASHES.lock().unwrap(), Instant::now());
    if reload {
        eprintln!("[webview] the page's process died ({why}) — reloading");
        if let Err(e) = webview.reload() {
            eprintln!("[webview] reload failed: {e}");
        }
        return;
    }
    eprintln!("[webview] the page's process died ({why}) {MAX} times in {} min — not reloading", WINDOW.as_secs() / 60);
    match webview.url() {
        Ok(mut url) => {
            url.set_path("/crashed.html");
            url.set_query(None);
            url.set_fragment(None);
            if let Err(e) = webview.navigate(url) {
                eprintln!("[webview] showing the error page failed: {e}");
            }
        }
        Err(e) => eprintln!("[webview] no page url: {e}"),
    }
}

/// Linux: WebKitGTK reports the death on the GTK webview itself, which Tauri
/// doesn't forward. Hook it on the main window's webview.
#[cfg(target_os = "linux")]
pub fn watch_linux(app: &tauri::AppHandle) {
    use tauri::Manager;
    use webkit2gtk::{WebProcessTerminationReason, WebViewExt};
    let Some(window) = app.get_webview_window("main") else {
        eprintln!("[webview] no main window to watch");
        return;
    };
    let target = window.clone();
    let _ = window.with_webview(move |wv| {
        let target = target.clone();
        wv.inner().connect_web_process_terminated(move |_, reason| {
            let why = match reason {
                WebProcessTerminationReason::Crashed => "crashed",
                WebProcessTerminationReason::ExceededMemoryLimit => "exceeded WebKit's memory limit",
                // Asked for (`webkit_web_view_terminate_web_process`) —
                // nothing here does, so leave it be.
                WebProcessTerminationReason::TerminatedByApi => return,
                _ => "unknown reason",
            };
            on_page_crash(target.as_ref(), why);
        });
    });
}

/// Linux: WebKitGTK kills the page once it passes a fixed memory limit (4 GB
/// on the VM, whatever the machine has), and only lets an embedder raise it
/// when the web context is created — which wry does, without a hook. The
/// documented switch turns the monitor off instead. Set before any webview
/// exists, so the web process inherits it. A user's own setting wins.
#[cfg(target_os = "linux")]
pub fn disable_memory_killer() {
    const VAR: &str = "WEBKIT_DISABLE_MEMORY_PRESSURE_MONITOR";
    if std::env::var_os(VAR).is_none() {
        std::env::set_var(VAR, "1");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_third_crash_in_five_minutes_stops_the_reloading() {
        let mut c = Vec::new();
        let t0 = Instant::now();
        assert!(should_reload(&mut c, t0));
        assert!(should_reload(&mut c, t0 + Duration::from_secs(60)));
        assert!(!should_reload(&mut c, t0 + Duration::from_secs(120)), "third inside the window");
    }

    #[test]
    fn crashes_older_than_the_window_are_forgotten() {
        let mut c = Vec::new();
        let t0 = Instant::now();
        assert!(should_reload(&mut c, t0));
        assert!(should_reload(&mut c, t0 + Duration::from_secs(60)));
        // The first two are past five minutes by now.
        assert!(should_reload(&mut c, t0 + Duration::from_secs(400)));
        assert!(should_reload(&mut c, t0 + Duration::from_secs(420)));
        assert!(!should_reload(&mut c, t0 + Duration::from_secs(430)));
    }
}
