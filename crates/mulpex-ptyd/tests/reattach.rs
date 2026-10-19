//! The daemon, driven as the app drives it: a real `mulpex-ptyd` binary on a
//! scratch home, real PTYs, the real client.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use mulpex_ptyd::client::{Client, Event, Handler};
use mulpex_ptyd::proto;
use serde_json::{json, Value};

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "ptyd-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn start(home: &Path, idle_ms: u64) -> Child {
    let c = Command::new(env!("CARGO_BIN_EXE_mulpex-ptyd"))
        .arg("--home")
        .arg(home)
        .env("MULPEX_PTYD_IDLE_MS", idle_ms.to_string())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for(|| proto::socket_path(home).exists(), "the socket");
    c
}

fn wait_for(f: impl Fn() -> bool, what: &str) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[derive(Default)]
struct Collect(Mutex<Vec<Event>>);

impl Handler for Collect {
    fn event(&self, ev: Event) {
        self.0.lock().unwrap().push(ev);
    }
}

impl Collect {
    fn bytes_from(&self, from: u64) -> Vec<u8> {
        let mut out = Vec::new();
        for ev in self.0.lock().unwrap().iter() {
            if let Event::Output(off, b) = ev {
                if *off >= from {
                    out.extend(b);
                }
            }
        }
        out
    }
    fn exited(&self) -> bool {
        self.0.lock().unwrap().iter().any(|e| *e == Event::Exit)
    }
}

fn spawn_sh(c: &Client, key: &str, script: &str) -> (u32, i64) {
    let env: Vec<Value> = std::env::vars().map(|(k, v)| json!([k, v])).collect();
    let r = c
        .request(json!({
            "op": "spawn", "key": key, "argv": ["/bin/sh", "-c", script], "env": env,
            "cwd": "/", "rows": 24, "cols": 80, "ring_cap": 1 << 20, "meta": {"kind": "claude"},
        }))
        .unwrap();
    (r["sid"].as_u64().unwrap() as u32, r["pid"].as_i64().unwrap())
}

fn alive(pid: i64) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// The whole point: an app goes away mid-stream (no goodbye — a crash), a new
/// one connects, and the session's output is continuous across the two, with
/// the resize recorded at the right place in it.
#[test]
fn a_session_survives_its_app_and_reattaches_without_a_gap() {
    let home = scratch("reattach");
    let mut d = start(&home, 60_000);
    let c1 = Client::connect(&proto::socket_path(&home), "test").unwrap();
    assert_eq!(c1.proto(), proto::PROTO);
    let state_root = PathBuf::from(c1.hello["state_root"].as_str().unwrap());
    assert!(state_root.is_dir());

    let (sid, pid) = spawn_sh(&c1, "p/1", "i=0; while :; do echo n$i; i=$((i+1)); sleep 0.005; done");
    let seen1 = Arc::new(Collect::default());
    c1.register(sid, seen1.clone());
    wait_for(|| seen1.bytes_from(0).len() > 200, "first output");
    c1.request(json!({"op": "resize", "sid": sid, "rows": 30, "cols": 100})).unwrap();
    wait_for(|| seen1.0.lock().unwrap().iter().any(|e| *e == Event::Resize(100, 30)), "the resize event");
    // The app vanishes without a word.
    c1.close();
    drop(c1);
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(pid), "the session died with its app");

    let c2 = Client::connect(&proto::socket_path(&home), "test").unwrap();
    let list = c2.request(json!({"op": "list"})).unwrap();
    let s = &list["sessions"][0];
    assert_eq!((s["sid"].as_u64(), s["key"].as_str(), s["alive"].as_bool()), (Some(sid as u64), Some("p/1"), Some(true)));
    assert_eq!((s["cols"].as_u64(), s["rows"].as_u64()), (Some(100), Some(30)));

    let seen2 = Arc::new(Collect::default());
    c2.register(sid, seen2.clone());
    let a = c2.request(json!({"op": "attach", "sid": sid})).unwrap();
    assert_eq!(a["start"], 0, "a 1 MiB ring still holds everything");
    assert_eq!(a["init"], json!([80, 24]));
    let marks = a["resizes"].as_array().unwrap();
    assert_eq!(marks.len(), 1);
    assert_eq!((marks[0][1].as_u64(), marks[0][2].as_u64()), (Some(100), Some(30)));
    let total = a["total"].as_u64().unwrap();
    let mut stream = base64::engine::general_purpose::STANDARD.decode(a["ring"].as_str().unwrap()).unwrap();
    assert_eq!(stream.len() as u64, total);
    std::thread::sleep(Duration::from_millis(400));
    let live = seen2.bytes_from(total);
    assert!(!live.is_empty(), "nothing streamed after the attach");
    stream.extend(live);

    // Every number, in order, no gap and no repeat across ring + live stream.
    let text = String::from_utf8_lossy(&stream);
    let nums: Vec<u64> = text.split_whitespace().filter_map(|w| w.strip_prefix('n')?.parse().ok()).collect();
    assert!(nums.len() > 50);
    for w in nums.windows(2) {
        assert_eq!(w[1], w[0] + 1, "gap or repeat between {} and {}", w[0], w[1]);
    }
    assert_eq!(nums[0], 0);

    c2.request(json!({"op": "kill", "sid": sid})).unwrap();
    assert!(!alive(pid), "kill reaps the child");
    let _ = c2.request(json!({"op": "shutdown"}));
    let _ = d.wait();
    let _ = std::fs::remove_dir_all(&home);
}

/// A claude's background command runs in a process group of its own; killing
/// the session must reach it too (the app's old `Session::kill`, unchanged).
#[test]
fn kill_reaches_a_background_group_and_exit_is_reported() {
    let home = scratch("kill");
    let mut d = start(&home, 60_000);
    let c = Client::connect(&proto::socket_path(&home), "test").unwrap();
    let pidfile = home.join("bg.pid");
    let script = format!("perl -e 'setpgrp(0,0); exec q(sleep), q(9999)' & echo $! > {}; wait", pidfile.display());
    let (sid, _pid) = spawn_sh(&c, "p/2", &script);
    wait_for(|| std::fs::read_to_string(&pidfile).is_ok_and(|s| !s.trim().is_empty()), "the background pid");
    let bg: i64 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    wait_for(|| unsafe { libc::getpgid(bg as i32) } == bg as i32, "its own group");
    c.request(json!({"op": "kill", "sid": sid})).unwrap();
    wait_for(|| !alive(bg), "the background group to die");

    // A session that exits by itself says so.
    let (sid2, _) = spawn_sh(&c, "p/3", "echo bye");
    let seen = Arc::new(Collect::default());
    c.register(sid2, seen.clone());
    wait_for(|| seen.exited(), "the exit event");
    let list = c.request(json!({"op": "list"})).unwrap();
    assert_eq!(list["sessions"][0]["alive"], false, "a dead session stays listed until killed");

    let _ = c.request(json!({"op": "shutdown"}));
    let _ = d.wait();
    let _ = std::fs::remove_dir_all(&home);
}

/// `shutdown` (a real ⌘Q) kills everything and takes the state root with it;
/// a second daemon on the same home steps aside; an idle daemon goes away.
#[test]
fn shutdown_lock_and_idle_exit() {
    let home = scratch("life");
    let mut d = start(&home, 600);
    // Same home, second daemon: exits at once, the first keeps serving.
    let mut second = Command::new(env!("CARGO_BIN_EXE_mulpex-ptyd")).arg("--home").arg(&home).spawn().unwrap();
    let st = second.wait().unwrap();
    assert!(st.success());
    let c = Client::connect(&proto::socket_path(&home), "test").unwrap();
    let root = PathBuf::from(c.hello["state_root"].as_str().unwrap());
    let (_, pid) = spawn_sh(&c, "p/1", "sleep 9999");
    c.request(json!({"op": "shutdown"})).unwrap();
    let _ = d.wait();
    assert!(!alive(pid), "shutdown kills every session");
    assert!(!root.exists(), "shutdown removes the state root");
    assert!(!proto::socket_path(&home).exists());

    // Idle: no app, no sessions → gone by itself.
    let mut d = start(&home, 600);
    let t = Instant::now();
    let st = d.wait().unwrap();
    assert!(st.success() && t.elapsed() < Duration::from_secs(5));
    let _ = std::fs::remove_dir_all(&home);
}
