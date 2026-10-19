//! The app's end of `mulpex-ptyd`, the daemon that owns every PTY so claudes
//! and terminals outlive an app restart (docs/ptyd.md).
//!
//! - `client()` — the one connection, started on first use; the daemon is
//!   launched (from a stable copy of the bundled sidecar) if none is running.
//! - **Exit modes.** A real quit (⌘Q, closing the window) still ends every
//!   session: `teardown` kills them and sends `shutdown`. A *restart* — an
//!   update, `restart_app` with `detach` — sets `DETACH` first, and teardown
//!   then only closes the socket. A crash closes it too, by dying.
//! - **Snapshots.** Just before a detach the frontend serializes every xterm
//!   at its exact stream offset (`save_snapshots`); the next app repaints each
//!   pane from that, then from the daemon's ring after it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mulpex_ptyd::client::Client;
use mulpex_ptyd::proto;
use serde::Deserialize;
use serde_json::{json, Value};
use tauri::Emitter;

static CLIENT: Mutex<Option<Arc<Client>>> = Mutex::new(None);

/// Set before a restart that should leave every session running.
static DETACH: AtomicBool = AtomicBool::new(false);
/// The frontend has handed over its snapshots for this restart.
static SNAPPED: AtomicBool = AtomicBool::new(false);

/// How long a restart waits for the frontend's snapshots before going anyway
/// (the panes then repaint through the no-snapshot path).
const SNAPSHOT_WAIT: Duration = Duration::from_secs(3);

pub fn detaching() -> bool {
    DETACH.load(Ordering::SeqCst)
}

fn home() -> PathBuf {
    // Tests get a daemon of their own (`tests_daemon`), never a real home's.
    #[cfg(test)]
    return std::env::temp_dir().join(format!("mulpex-ptyd-test-{}", std::process::id()));
    #[cfg(not(test))]
    mulpex_core::mulpex_home()
}

/// The daemon connection, starting the daemon if there is none. One daemon
/// serves one Mulpex home, so a dev build (`~/.mulpex-dev`) never touches the
/// sessions of the installed app.
pub fn client() -> Result<Arc<Client>, String> {
    let mut g = CLIENT.lock().unwrap();
    if let Some(c) = g.as_ref().filter(|c| c.is_alive()) {
        return Ok(Arc::clone(c));
    }
    let c = connect_or_start(&home())?;
    *g = Some(Arc::clone(&c));
    Ok(c)
}

/// The connection if there is one — never starts a daemon.
pub fn existing() -> Option<Arc<Client>> {
    CLIENT.lock().unwrap().as_ref().filter(|c| c.is_alive()).cloned()
}

fn app_version() -> String {
    format!("mulpex {}", env!("CARGO_PKG_VERSION"))
}

fn connect_or_start(home: &Path) -> Result<Arc<Client>, String> {
    #[cfg(test)]
    tests_daemon::ensure(home);
    let sock = proto::socket_path(home);
    if let Ok(c) = Client::connect(&sock, &app_version()) {
        if proto::compatible(c.proto()) {
            return Ok(c);
        }
        // A daemon this app cannot drive — one from a release whose protocol
        // this one dropped (the updater warned before installing it). Its
        // sessions cannot be adopted, so they end here and come back through
        // the ordinary `--resume` restore; the frozen `list`/`shutdown` are
        // what every version can still say to every other.
        let n = c
            .request(json!({"op": "list"}))
            .ok()
            .and_then(|v| v["sessions"].as_array().map(Vec::len))
            .unwrap_or(0);
        eprintln!(
            "[ptyd] the running daemon speaks protocol {} (this app: {}..={}); \
             shutting it down, {n} session(s) will be resumed",
            c.proto(),
            proto::PROTO_MIN,
            proto::PROTO
        );
        let _ = c.request(json!({"op": "shutdown"}));
        let t = Instant::now();
        while sock.exists() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    start_daemon(home)?;
    let t = Instant::now();
    loop {
        match Client::connect(&sock, &app_version()) {
            Ok(c) => return Ok(c),
            Err(e) if t.elapsed() > Duration::from_secs(5) => {
                return Err(format!(
                    "the Mulpex PTY daemon did not start ({e}); see {}",
                    log_path(home).display()
                ))
            }
            Err(_) => std::thread::sleep(Duration::from_millis(30)),
        }
    }
}

fn log_path(home: &Path) -> PathBuf {
    proto::dir(home).join("ptyd.log")
}

/// The bundled daemon: beside the app executable, like `mulpex-helper`.
fn bundled_daemon() -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?.parent()?.join("mulpex-ptyd");
    p.is_file().then_some(p)
}

/// Copy `src` to `<dir>/<name>` unless an identical copy is there already —
/// written beside and renamed over, so a process running the old one keeps it.
pub fn install_copy(src: &Path, dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dest = dir.join(name);
    let bytes = std::fs::read(src)?;
    if std::fs::read(&dest).is_ok_and(|d| d == bytes) {
        return Ok(dest);
    }
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{name}.{}", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

fn short_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// Launch the daemon from a copy outside the app bundle. An update replaces
/// the bundle (and on Linux the AppImage's mount vanishes with the app), and a
/// daemon that outlives the app must not be running from either.
fn start_daemon(home: &Path) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let src = bundled_daemon().ok_or("mulpex-ptyd is missing from the app bundle")?;
    let bytes = std::fs::read(&src).map_err(|e| format!("{}: {e}", src.display()))?;
    let dir = proto::dir(home).join(format!("{}-{}", proto::PROTO, short_hash(&bytes)));
    let exe = install_copy(&src, &dir, "mulpex-ptyd").map_err(|e| format!("install mulpex-ptyd: {e}"))?;
    // Older copies go; the one starting now is in `dir`. A daemon still running
    // from a deleted copy is unaffected (its inode stays until it exits).
    if let Ok(entries) = std::fs::read_dir(proto::dir(home)) {
        for e in entries.flatten() {
            let p = e.path();
            let is_build = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.as_bytes()[0].is_ascii_digit());
            if is_build && p != dir && p.is_dir() {
                let _ = std::fs::remove_dir_all(&p);
            }
        }
    }
    // One old log is kept; a daemon lives long, but its log need not grow forever.
    if std::fs::metadata(log_path(home)).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
        let _ = std::fs::rename(log_path(home), log_path(home).with_extension("log.old"));
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(home))
        .map_err(|e| format!("{}: {e}", log_path(home).display()))?;
    let log2 = log.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--home").arg(home).current_dir("/").env_clear();
    // A minimal environment: every child the daemon spawns gets its whole env
    // from the app, so nothing here reaches a claude.
    for k in ["HOME", "USER", "LOGNAME", "PATH", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "SHELL"] {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    cmd.stdin(std::process::Stdio::null()).stdout(log).stderr(log2);
    // Its own session: no controlling terminal, and not in the app's process
    // group, so nothing aimed at the app (a terminal's hangup, a killpg) can
    // reach it. Measured on macOS: a setsid'd child of the app keeps the app's
    // folder permissions (Full Disk Access included) after the app exits.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("start mulpex-ptyd: {e}"))?;
    // Reap it if it exits at once (lost the lock race): no zombie in the app.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// `mulpex-helper` from a copy beside the daemon's, in the Mulpex home. Every
/// running claude names it by absolute path in its settings, MCP config and
/// listener — and a claude now outlives the app that started it, so that path
/// must outlive the app bundle too (on Linux, the AppImage mount it came from
/// disappears the moment the app quits). Refreshed on every launch, so after
/// an update the surviving claudes run the new helper, as they always did.
pub fn stable_helper(bundled: &Path) -> PathBuf {
    match install_copy(bundled, &home().join("bin"), "mulpex-helper") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[ptyd] could not copy mulpex-helper ({e}); using the bundle's");
            bundled.to_path_buf()
        }
    }
}

/// Where the daemon serves each project's scratch dir from — `None` without a
/// daemon (then the app falls back to a root of its own).
pub fn state_root() -> Option<PathBuf> {
    let c = client().ok()?;
    c.hello["state_root"].as_str().map(PathBuf::from)
}

/// Every session the daemon holds.
pub fn list() -> Vec<Value> {
    existing()
        .and_then(|c| c.request(json!({"op": "list"})).ok())
        .and_then(|v| v["sessions"].as_array().cloned())
        .unwrap_or_default()
}

pub fn kill_sid(sid: u32) {
    if let Some(c) = existing() {
        let _ = c.request(json!({"op": "kill", "sid": sid}));
    }
}

/// A real quit: the daemon kills whatever is left and exits.
pub fn shutdown() {
    if let Some(c) = existing() {
        let _ = c.request(json!({"op": "shutdown"}));
    }
}

/// A restart: leave the daemon and every session running for the next app.
pub fn detach() {
    if let Some(c) = existing() {
        let _ = c.request(json!({"op": "detach"}));
        c.close();
    }
}

// ---- restart with every session kept ----

/// Restart the app leaving every claude and terminal running: ask the
/// frontend for its pane snapshots, wait for them (briefly), then restart in
/// detach mode. Returns at once; the restart happens on its own thread.
pub fn restart_detached(app: tauri::AppHandle) {
    SNAPPED.store(false, Ordering::SeqCst);
    let _ = app.emit("ptyd-snapshot", ());
    std::thread::spawn(move || {
        let t = Instant::now();
        while !SNAPPED.load(Ordering::SeqCst) && t.elapsed() < SNAPSHOT_WAIT {
            std::thread::sleep(Duration::from_millis(20));
        }
        DETACH.store(true, Ordering::SeqCst);
        app.request_restart();
    });
}

/// One xterm's serialized screen, as the frontend's `terminals.serialize`
/// produces it.
#[derive(Debug, Deserialize)]
pub struct PaneSnapshot {
    #[serde(rename = "projectHandle")]
    pub project_handle: crate::snapshot::ProjectHandle,
    pub id: usize,
    pub data: String,
    pub offset: u64,
    pub cols: u16,
    pub rows: u16,
}

fn snapshots_dir() -> PathBuf {
    proto::dir(&home()).join("snapshots")
}

/// Where (project scratch dir, id)'s snapshot lives.
pub fn snapshot_path(state_dir: &Path, id: usize) -> PathBuf {
    let proj = state_dir.file_name().and_then(|n| n.to_str()).unwrap_or("x");
    snapshots_dir().join(proj).join(format!("{id}.json"))
}

pub fn write_snapshot(state_dir: &Path, s: &PaneSnapshot) {
    let p = snapshot_path(state_dir, s.id);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let v = json!({"data": s.data, "offset": s.offset, "cols": s.cols, "rows": s.rows});
    let _ = std::fs::write(p, v.to_string());
}

pub fn snapshots_saved() {
    SNAPPED.store(true, Ordering::SeqCst);
}

/// Take (and delete) a pane's snapshot, if one was saved within the hour.
pub fn take_snapshot(state_dir: &Path, id: usize) -> Option<crate::pty::Snapshot> {
    let p = snapshot_path(state_dir, id);
    let text = std::fs::read_to_string(&p).ok()?;
    let fresh = std::fs::metadata(&p)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(3600));
    let _ = std::fs::remove_file(&p);
    if !fresh {
        return None;
    }
    let v: Value = serde_json::from_str(&text).ok()?;
    Some(crate::pty::Snapshot {
        data: v["data"].as_str()?.to_string(),
        offset: v["offset"].as_u64()?,
        cols: v["cols"].as_u64()? as u16,
        rows: v["rows"].as_u64()? as u16,
    })
}

/// Snapshots nothing took: older than an hour, or for a session gone.
pub fn sweep_snapshots() {
    let Ok(projects) = std::fs::read_dir(snapshots_dir()) else { return };
    for p in projects.flatten() {
        let Ok(files) = std::fs::read_dir(p.path()) else { continue };
        for f in files.flatten() {
            let old = f
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_none_or(|age| age > Duration::from_secs(3600));
            if old {
                let _ = std::fs::remove_file(f.path());
            }
        }
        let _ = std::fs::remove_dir(p.path());
    }
}

/// Whether installing a release survives: the running daemon is within the
/// range of daemons the NEW app says it can drive (`latest.json`'s
/// `ptyd_min`/`ptyd`). A release from before this feature has neither field
/// and does not survive — which is what its warning has always said.
pub fn update_survives(ptyd: Option<u32>, ptyd_min: Option<u32>) -> bool {
    let (Some(max), Some(min)) = (ptyd, ptyd_min) else { return false };
    existing().is_some_and(|c| proto::update_survives(c.proto(), min, max))
}

/// The Live Token config the daemon's proxy runs with, pushed on every change.
pub fn configure_live_token(config: Value) -> Result<Value, String> {
    client()?.request(json!({"op": "live_token", "config": config}))
}

/// The project's directory name under the state root: stable across app
/// restarts, unlike a project handle, so a surviving claude's
/// `MULPEX_STATE_DIR` still names its project's dir after the next launch.
pub fn project_key(canon: &Path) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(canon.as_os_str().as_encoded_bytes());
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Tests run against a daemon inside the test process (the library's own
/// server), on a scratch home — never a real one, and never a binary that
/// might not be built.
#[cfg(test)]
mod tests_daemon {
    use std::path::Path;
    use std::sync::Once;

    pub fn ensure(home: &Path) {
        static ONCE: Once = Once::new();
        let home = home.to_path_buf();
        ONCE.call_once(move || {
            // An idle exit would `exit()` the whole test run.
            std::env::set_var("MULPEX_PTYD_IDLE_MS", "999999999");
            std::thread::spawn(move || {
                let _ = mulpex_ptyd::daemon::run(home);
            });
            let sock = mulpex_ptyd::proto::socket_path(&super::home());
            let t = std::time::Instant::now();
            while !sock.exists() && t.elapsed() < std::time::Duration::from_secs(5) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_key_is_stable_and_distinct() {
        let a = project_key(Path::new("/Users/x/proj"));
        assert_eq!(a, project_key(Path::new("/Users/x/proj")));
        assert_ne!(a, project_key(Path::new("/Users/x/proj2")));
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn an_update_without_ptyd_fields_never_survives() {
        assert!(!update_survives(None, None));
        assert!(!update_survives(Some(1), None));
    }
}
