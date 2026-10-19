//! The daemon process: one unix socket, one connected app at a time, a table
//! of sessions that outlives every app that connects to it.
//!
//! An app disconnecting — quitting for an update, crashing, being killed — is
//! never a reason to touch a session. Only an explicit `kill` (a closed row) or
//! `shutdown` (a real ⌘Q) ends one. With no sessions and no app for
//! `IDLE_EXIT`, the daemon exits by itself.

pub mod live_token;
pub mod session;

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};

use crate::proto;
use session::{DSession, RecordPaths, SpawnReq};

/// No sessions and no app for this long: nothing left to keep alive.
const IDLE_EXIT: Duration = Duration::from_secs(15);

/// `IDLE_EXIT`, shortened by the integration tests (`MULPEX_PTYD_IDLE_MS`).
fn idle_exit() -> Duration {
    std::env::var("MULPEX_PTYD_IDLE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(IDLE_EXIT)
}

/// The connected app.
struct Conn {
    gen: u64,
    tx: mpsc::Sender<Vec<u8>>,
    stream: UnixStream,
}

pub struct Server {
    home: PathBuf,
    state_root: PathBuf,
    build: String,
    sessions: Mutex<HashMap<u32, Arc<DSession>>>,
    next_sid: AtomicU32,
    client: Mutex<Option<Conn>>,
    next_gen: Mutex<u64>,
    /// When the daemon last had an app or a session, for `IDLE_EXIT`.
    last_busy: Mutex<Instant>,
}

impl Server {
    /// Queue a frame for the connected app if `s` streams to it. Called with
    /// `s.ring` locked, which is what orders it against an `attach` snapshot.
    pub fn send_if_streaming(&self, s: &DSession, frame: impl FnOnce() -> Vec<u8>) {
        let c = self.client.lock().unwrap();
        if let Some(c) = c.as_ref() {
            if s.stream_gen.load(Ordering::Relaxed) == c.gen {
                let _ = c.tx.send(frame());
            }
        }
    }

    fn reply(&self, gen: u64, v: Value) {
        let c = self.client.lock().unwrap();
        if let Some(c) = c.as_ref().filter(|c| c.gen == gen) {
            let _ = c.tx.send(proto::json_frame(&v));
        }
    }

    fn session(&self, sid: u32) -> Option<Arc<DSession>> {
        self.sessions.lock().unwrap().get(&sid).cloned()
    }
}

/// `mulpex-ptyd --home <dir>`: serve the Mulpex home `<dir>` until idle.
pub fn run(home: PathBuf) -> anyhow::Result<()> {
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    std::fs::create_dir_all(proto::dir(&home))?;
    let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));

    // One daemon per Mulpex home. A second one exits quietly: an app that
    // raced another to start us will simply connect to the winner.
    let lock = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(proto::lock_path(&home))?;
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        eprintln!("[ptyd] another daemon holds {}", proto::lock_path(&home).display());
        return Ok(());
    }

    // Who holds the lock, for a person (or a test) looking at a home.
    let _ = std::fs::write(proto::dir(&home).join("ptyd.pid"), std::process::id().to_string());

    // Named for this process, so the app's stale-root sweep (`mulpex-<pid>`,
    // kept while that pid lives) protects it for exactly as long as we run —
    // and every claude's `MULPEX_STATE_DIR` stays valid across app restarts.
    let state_root = std::env::temp_dir().join(format!("mulpex-{}", std::process::id()));
    std::fs::create_dir_all(&state_root)?;

    let sock = proto::socket_path(&home);
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;

    let server = Arc::new(Server {
        home,
        state_root,
        build: build_id(),
        sessions: Mutex::new(HashMap::new()),
        next_sid: AtomicU32::new(1),
        client: Mutex::new(None),
        next_gen: Mutex::new(0),
        last_busy: Mutex::new(Instant::now()),
    });
    eprintln!("[ptyd] up: pid {} build {} root {}", std::process::id(), server.build, server.state_root.display());

    {
        let server = Arc::clone(&server);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(250));
            let busy = server.client.lock().unwrap().is_some() || !server.sessions.lock().unwrap().is_empty();
            let mut last = server.last_busy.lock().unwrap();
            if busy {
                *last = Instant::now();
            } else if last.elapsed() >= idle_exit() {
                eprintln!("[ptyd] idle, exiting");
                exit_clean(&server);
            }
        });
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if !same_user(&stream) {
            continue;
        }
        let server = Arc::clone(&server);
        thread::spawn(move || serve(server, stream));
    }
    Ok(())
}

fn exit_clean(server: &Server) -> ! {
    let _ = std::fs::remove_file(proto::socket_path(&server.home));
    let _ = std::fs::remove_file(proto::dir(&server.home).join("ptyd.pid"));
    let _ = std::fs::remove_dir_all(&server.state_root);
    std::process::exit(0)
}

/// sha256 of our own executable, shortened: which daemon build is running.
fn build_id() -> String {
    use sha2::{Digest, Sha256};
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .map(|b| Sha256::digest(&b).iter().take(6).map(|x| format!("{x:02x}")).collect())
        .unwrap_or_default()
}

/// Only our own user may drive us (the socket is 0600 too — belt and braces).
fn same_user(s: &UnixStream) -> bool {
    let fd = std::os::fd::AsRawFd::as_raw_fd(s);
    let me = unsafe { libc::getuid() };
    #[cfg(target_os = "macos")]
    {
        let (mut uid, mut gid) = (0, 0);
        unsafe { libc::getpeereid(fd, &mut uid, &mut gid) == 0 && uid == me }
    }
    #[cfg(target_os = "linux")]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut libc::c_void, &mut len)
        };
        rc == 0 && cred.uid == me
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (fd, me);
        true
    }
}

/// One connection. It becomes THE app on `hello`, replacing any earlier one
/// (an app relaunched before the old connection's EOF was noticed).
fn serve(server: Arc<Server>, stream: UnixStream) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let Ok(mut wstream) = stream.try_clone() else { return };
    // The writer: PTY reader threads never block on a slow app.
    thread::spawn(move || {
        for frame in rx {
            if wstream.write_all(&frame).is_err() {
                break;
            }
        }
    });
    let mut gen: Option<u64> = None;
    let mut tx = Some(tx);
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    while let Ok(Some((kind, payload))) = proto::read_frame(&mut reader) {
        match kind {
            proto::INPUT => {
                if let Some((sid, bytes)) = proto::parse_input(&payload) {
                    if let Some(s) = server.session(sid) {
                        s.write(bytes);
                    }
                }
            }
            proto::JSON => {
                let Ok(req) = serde_json::from_slice::<Value>(&payload) else { continue };
                if req["op"] == "hello" {
                    let g = {
                        let mut n = server.next_gen.lock().unwrap();
                        *n += 1;
                        *n
                    };
                    let Some(t) = tx.take() else { continue };
                    let Ok(s2) = stream.try_clone() else { continue };
                    if let Some(old) = server.client.lock().unwrap().replace(Conn { gen: g, tx: t, stream: s2 }) {
                        let _ = old.stream.shutdown(std::net::Shutdown::Both);
                    }
                    gen = Some(g);
                }
                let Some(g) = gen else { continue };
                handle(&server, g, req);
            }
            _ => {}
        }
    }
    // EOF: the app went away (quit, restart, crash). Its sessions keep running.
    if let Some(g) = gen {
        let mut c = server.client.lock().unwrap();
        if c.as_ref().is_some_and(|c| c.gen == g) {
            *c = None;
        }
    }
}

fn handle(server: &Arc<Server>, gen: u64, req: Value) {
    let id = req["id"].clone();
    let ok = |extra: Value| {
        let mut v = json!({"id": id, "ok": true});
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        v
    };
    let err = |e: String| json!({"id": id, "ok": false, "error": e});
    let sid = req["sid"].as_u64().unwrap_or(0) as u32;
    let v = match req["op"].as_str().unwrap_or("") {
        // ---- the frozen core: these three keep their shape forever ----
        "hello" => ok(json!({
            "proto": proto::PROTO,
            "build": server.build,
            "pid": std::process::id(),
            "state_root": server.state_root,
        })),
        "list" => {
            let mut all: Vec<Value> = server.sessions.lock().unwrap().values().map(|s| s.info()).collect();
            all.sort_by_key(|v| v["sid"].as_u64());
            ok(json!({ "sessions": all }))
        }
        "shutdown" => {
            let all: Vec<Arc<DSession>> = server.sessions.lock().unwrap().drain().map(|(_, s)| s).collect();
            for s in &all {
                s.kill();
            }
            server.reply(gen, ok(json!({})));
            // Let the writer flush the reply before the process goes.
            thread::sleep(Duration::from_millis(150));
            exit_clean(server);
        }
        // ---- everything else may change with a PROTO bump ----
        "spawn" => {
            let sid = server.next_sid.fetch_add(1, Ordering::Relaxed);
            match spawn_req(&req).and_then(|r| DSession::spawn(server, sid, r, gen)) {
                Ok(s) => {
                    let pid = s.pid;
                    server.sessions.lock().unwrap().insert(sid, s);
                    ok(json!({"sid": sid, "pid": pid}))
                }
                Err(e) => err(e.to_string()),
            }
        }
        "attach" => match server.session(sid) {
            Some(s) => {
                // Under the ring lock: the reply is queued before any byte
                // pushed after the snapshot, and streaming starts exactly there.
                let ring = s.ring.lock().unwrap();
                s.stream_gen.store(gen, Ordering::Relaxed);
                let bytes: Vec<u8> = ring.buf.iter().copied().collect();
                let (cols, rows) = s.size();
                let v = ok(json!({
                    "sid": sid,
                    "start": ring.start,
                    "total": ring.total(),
                    "init": [ring.init.0, ring.init.1],
                    "resizes": ring.resizes.iter().map(|r| json!([r.0, r.1, r.2])).collect::<Vec<_>>(),
                    "ring": base64::engine::general_purpose::STANDARD.encode(&bytes),
                    "cols": cols,
                    "rows": rows,
                    "alive": s.alive.load(Ordering::Relaxed),
                }));
                server.reply(gen, v);
                return;
            }
            None => err(format!("no session {sid}")),
        },
        "resize" => match server.session(sid) {
            Some(s) => {
                s.resize(server, req["rows"].as_u64().unwrap_or(24) as u16, req["cols"].as_u64().unwrap_or(80) as u16);
                ok(json!({}))
            }
            None => err(format!("no session {sid}")),
        },
        "set_meta" => match server.session(sid) {
            Some(s) => {
                *s.meta.lock().unwrap() = req["meta"].clone();
                ok(json!({}))
            }
            None => err(format!("no session {sid}")),
        },
        "kill" => {
            // Off this thread: a kill waits for the child, and the connection
            // must keep delivering keystrokes to every other session meanwhile.
            let server = Arc::clone(server);
            thread::spawn(move || {
                if let Some(s) = server.sessions.lock().unwrap().remove(&sid) {
                    s.kill();
                }
                server.reply(gen, json!({"id": id, "ok": true}));
            });
            return;
        }
        "live_token" => match live_token::configure(&req["config"]) {
            Ok(v) => ok(v),
            Err(e) => err(e),
        },
        "detach" => ok(json!({})),
        other => err(format!("unknown op {other:?}")),
    };
    server.reply(gen, v);
}

fn spawn_req(req: &Value) -> anyhow::Result<SpawnReq> {
    let strs = |v: &Value| -> Vec<String> {
        v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
    };
    let env = req["env"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|kv| Some((kv.get(0)?.as_str()?.to_string(), kv.get(1)?.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let record = req.get("record").filter(|r| r.is_object()).map(|r| RecordPaths {
        log: PathBuf::from(r["log"].as_str().unwrap_or_default()),
        screen: PathBuf::from(r["screen"].as_str().unwrap_or_default()),
        frames: PathBuf::from(r["frames"].as_str().unwrap_or_default()),
    });
    Ok(SpawnReq {
        key: req["key"].as_str().unwrap_or_default().to_string(),
        argv: strs(&req["argv"]),
        env,
        cwd: req["cwd"].as_str().unwrap_or("/").to_string(),
        rows: req["rows"].as_u64().unwrap_or(24) as u16,
        cols: req["cols"].as_u64().unwrap_or(80) as u16,
        ring_cap: req["ring_cap"].as_u64().unwrap_or(2 * 1024 * 1024) as usize,
        record,
        meta: req["meta"].clone(),
    })
}

/// Where a daemon serving `home` keeps its socket — for `main.rs`'s usage line.
pub fn describe(home: &Path) -> String {
    proto::socket_path(home).display().to_string()
}
