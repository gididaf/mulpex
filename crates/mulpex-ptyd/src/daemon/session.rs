//! One PTY the daemon owns: the child on it, the reader thread, the output
//! ring a reattaching app repaints from, and (for a shell) its transcript
//! recorder. This is the half of the app's old `pty::Session` that has to
//! outlive the app; argv, env and everything Mulpex-specific stay in the app.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use serde_json::{json, Value};

use super::Server;
use crate::procs;
use crate::proto;
use crate::vtgrid::Recorder;

/// How often the recorder publishes an unchanged-but-unpublished screen. The
/// reader thread only runs when there is output, so without this the final
/// chunk of a burst (typically the shell prompt itself) would sit unpublished.
const RECORDER_SETTLE: Duration = Duration::from_millis(200);

/// The last stretch of a session's output at absolute stream offsets, with
/// every resize inside it — what a reattaching app replays at the right sizes
/// (docs/rendering.md: bytes drawn for one size must never reach an xterm of
/// another).
pub struct Ring {
    cap: usize,
    pub buf: VecDeque<u8>,
    /// Absolute offset of `buf[0]` since spawn.
    pub start: u64,
    pub resizes: VecDeque<(u64, u16, u16)>,
    /// The PTY's size at spawn, (cols, rows).
    pub init: (u16, u16),
}

impl Ring {
    pub fn new(cap: usize, init: (u16, u16)) -> Self {
        Self { cap: cap.max(1), buf: VecDeque::new(), start: 0, resizes: VecDeque::new(), init }
    }

    pub fn total(&self) -> u64 {
        self.start + self.buf.len() as u64
    }

    /// Append; returns the offset the bytes start at.
    pub fn push(&mut self, bytes: &[u8]) -> u64 {
        let at = self.total();
        self.buf.extend(bytes);
        let over = self.buf.len().saturating_sub(self.cap);
        self.buf.drain(..over);
        self.start += over as u64;
        let start = self.start;
        while self.resizes.front().is_some_and(|r| r.0 < start) {
            self.resizes.pop_front();
        }
        at
    }

    pub fn resized(&mut self, cols: u16, rows: u16) {
        let at = self.total();
        self.resizes.push_back((at, cols, rows));
    }
}

/// Where a shell's transcript goes (the app's `pty::terminal_*_path`).
pub struct RecordPaths {
    pub log: PathBuf,
    pub screen: PathBuf,
    pub frames: PathBuf,
}

pub struct SpawnReq {
    pub key: String,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: String,
    pub rows: u16,
    pub cols: u16,
    pub ring_cap: usize,
    pub record: Option<RecordPaths>,
    pub meta: Value,
}

pub struct DSession {
    pub sid: u32,
    pub key: String,
    pub meta: Mutex<Value>,
    pub pid: Option<libc::pid_t>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    pub ring: Mutex<Ring>,
    pub alive: Arc<AtomicBool>,
    tty_dev: Arc<AtomicU32>,
    recorder: Option<Arc<Mutex<Recorder>>>,
    /// (cols, rows).
    size: Mutex<(u16, u16)>,
    pub started: Instant,
    /// The client generation this session streams to (`Server::client`). A new
    /// app connecting bumps the generation, so nothing flows to it until it
    /// attaches — and attaching sets this under the ring lock, so the ring it is
    /// handed and the first live byte after it can never overlap or gap.
    pub stream_gen: AtomicU64,
    killed: AtomicBool,
}

impl DSession {
    pub fn spawn(server: &Arc<Server>, sid: u32, req: SpawnReq, gen: u64) -> anyhow::Result<Arc<Self>> {
        let rows = req.rows.max(1);
        let cols = req.cols.max(1);
        let pair = NativePtySystem::default().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
        let Some((prog, args)) = req.argv.split_first() else {
            anyhow::bail!("empty argv");
        };
        let mut cmd = CommandBuilder::new(prog);
        cmd.args(args);
        // The app sends the child's WHOLE environment: the daemon was started
        // with a minimal one, and nothing of its own may leak into a claude.
        cmd.env_clear();
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        cmd.cwd(&req.cwd);

        // Built before the child so the file exists the moment the terminal
        // does — a read that races the first output should return "nothing
        // yet", not a missing-file error.
        let recorder = match &req.record {
            Some(p) => Some(Arc::new(Mutex::new(Recorder::new(
                p.log.clone(),
                p.screen.clone(),
                p.frames.clone(),
                rows,
                cols,
            )?))),
            None => None,
        };

        let child = pair.slave.spawn_command(cmd)?;
        let pid = child.process_id().map(|p| p as libc::pid_t);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let s = Arc::new(Self {
            sid,
            key: req.key,
            meta: Mutex::new(req.meta),
            pid,
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            ring: Mutex::new(Ring::new(req.ring_cap, (cols, rows))),
            alive: Arc::new(AtomicBool::new(true)),
            tty_dev: Arc::new(AtomicU32::new(0)),
            recorder,
            size: Mutex::new((cols, rows)),
            started: Instant::now(),
            // The spawning app streams from byte 0: nothing of a fresh session's
            // first paint may be lost.
            stream_gen: AtomicU64::new(gen),
            killed: AtomicBool::new(false),
        });

        {
            let s = Arc::clone(&s);
            let server = Arc::clone(server);
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let bytes = &buf[..n];
                            {
                                let mut ring = s.ring.lock().unwrap();
                                let at = ring.push(bytes);
                                server.send_if_streaming(&s, || proto::output_frame(s.sid, at, bytes));
                            }
                            if s.tty_dev.load(Ordering::Relaxed) == 0 {
                                // It has painted, so it owns the tty by now.
                                // Latched so teardown still knows the device
                                // after the child is gone.
                                if let Some(dev) = s.pid.and_then(procs::tty_dev_of) {
                                    s.tty_dev.store(dev, Ordering::Relaxed);
                                }
                            }
                            if let Some(rec) = &s.recorder {
                                if let Ok(mut r) = rec.lock() {
                                    r.push(bytes);
                                }
                            }
                        }
                    }
                }
                // EOF on the master is the process's death certificate. It is
                // deliberately NOT `wait()`ed here: a zombie's pid can't be
                // recycled, which keeps a later `killpg` on it safe.
                if let Some(rec) = &s.recorder {
                    if let Ok(mut r) = rec.lock() {
                        r.finish();
                    }
                }
                s.alive.store(false, Ordering::Relaxed);
                let _ring = s.ring.lock().unwrap();
                server.send_if_streaming(&s, || proto::json_frame(&json!({"ev": "exit", "sid": s.sid})));
            });
        }

        if let Some(rec) = &s.recorder {
            let rec = Arc::clone(rec);
            let alive = Arc::clone(&s.alive);
            thread::spawn(move || {
                while alive.load(Ordering::Relaxed) {
                    thread::sleep(RECORDER_SETTLE);
                    if let Ok(mut r) = rec.lock() {
                        r.settle();
                    }
                }
            });
        }
        Ok(s)
    }

    pub fn write(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    pub fn size(&self) -> (u16, u16) {
        *self.size.lock().unwrap()
    }

    pub fn resize(&self, server: &Server, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        {
            let mut size = self.size.lock().unwrap();
            if *size == (cols, rows) {
                return;
            }
            *size = (cols, rows);
        }
        let _ = self.master.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        {
            let mut ring = self.ring.lock().unwrap();
            ring.resized(cols, rows);
            server.send_if_streaming(self, || {
                proto::json_frame(&json!({"ev": "resize", "sid": self.sid, "cols": cols, "rows": rows}))
            });
        }
        // The recorder's grid has to track the child's idea of the screen, or
        // cursor-addressed redraws land on the wrong rows.
        if let Some(rec) = &self.recorder {
            if let Ok(mut r) = rec.lock() {
                r.resize(rows, cols);
            }
        }
    }

    /// Tear the session down: everything on its terminal, a claude's background
    /// process groups, its own group, then the child itself — reaped. The full
    /// reasoning for each step is in `procs` (`kill_tty_session`,
    /// `kill_child_groups`); the order is the one the app always used.
    pub fn kill(&self) {
        if self.killed.swap(true, Ordering::SeqCst) {
            return;
        }
        // Ours only while the master is still open — which it is until this
        // session is dropped. Once released the kernel can hand the same
        // device number to someone else's pty.
        let dev = match self.tty_dev.load(Ordering::Relaxed) {
            0 => self.pid.and_then(procs::tty_dev_of),
            d => Some(d),
        };
        if let Some(dev) = dev {
            procs::kill_tty_session(dev);
        }
        let shell = self.meta.lock().unwrap().get("kind").and_then(Value::as_str) == Some("shell");
        if !shell {
            if let Some(pid) = self.pid {
                procs::kill_child_groups(pid);
            }
        }
        if let Some(pid) = self.pid {
            unsafe {
                libc::killpg(pid, libc::SIGHUP);
                libc::killpg(pid, libc::SIGKILL);
            }
        }
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }

    pub fn info(&self) -> Value {
        let ring = self.ring.lock().unwrap();
        let (cols, rows) = self.size();
        json!({
            "sid": self.sid,
            "key": self.key,
            "pid": self.pid,
            "alive": self.alive.load(Ordering::Relaxed),
            "cols": cols,
            "rows": rows,
            "age_ms": self.started.elapsed().as_millis() as u64,
            "total": ring.total(),
            "meta": self.meta.lock().unwrap().clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_offsets_and_drops_resizes_that_rolled_off() {
        let mut r = Ring::new(8, (100, 30));
        assert_eq!(r.push(b"abc"), 0);
        r.resized(120, 40);
        assert_eq!(r.push(b"de"), 3);
        assert_eq!((r.start, r.total()), (0, 5));
        assert_eq!(r.resizes, [(3, 120, 40)]);
        r.push(b"fghij");
        assert_eq!((r.start, r.total()), (2, 10));
        assert_eq!(r.resizes, [(3, 120, 40)], "offset 3 is still inside the ring");
        r.push(b"kl");
        assert_eq!(r.start, 4);
        assert!(r.resizes.is_empty(), "the resize at 3 rolled off with its bytes");
        assert_eq!(r.buf.iter().copied().collect::<Vec<_>>(), b"efghijkl");
    }
}
