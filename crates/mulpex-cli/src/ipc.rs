//! How `mpx` the client asks the daemon to do something.
//!
//! Deliberately **not** a socket. The hub already runs on a request/response file
//! handshake (`spawn/`, `termreq/`, `namereq/`), the daemon is already polling a
//! directory every tick, and a second transport would be a second thing to get
//! wrong. Requests land in `<state_root>/cli/`, the daemon consumes them in
//! filename order — which is time order, so two `mpx new` racing get distinct ids
//! — and writes `<stem>.done` beside each.
//!
//! **Mutating commands go through here; read-only ones must not.** `mpx ls` and
//! the status line read the state dir directly, because a status line that can be
//! blocked by a busy daemon is a status line that freezes tmux's redraw.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Result};

/// How long a client waits for the daemon. The tick is 200 ms, so this is ~25
/// ticks: long enough to cover a `claude` spawn, short enough that a dead daemon
/// is reported rather than hung on.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Replies older than this are swept, so a client that died before reading its
/// answer cannot leak files forever.
const DONE_TTL: Duration = Duration::from_secs(60);

pub const DIR: &str = "cli";

pub struct Request {
    pub op: String,
    /// Canonical project directory. Every op is scoped to one project.
    pub project: String,
    pub arg: String,
}

impl Request {
    fn to_json(&self) -> String {
        serde_json::json!({ "op": self.op, "project": self.project, "arg": self.arg }).to_string()
    }

    fn from_json(text: &str) -> Option<Request> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        Some(Request {
            op: v["op"].as_str()?.to_string(),
            project: v["project"].as_str().unwrap_or_default().to_string(),
            arg: v["arg"].as_str().unwrap_or_default().to_string(),
        })
    }
}

fn dir(state_root: &Path) -> PathBuf {
    state_root.join(DIR)
}

/// Post a request and wait for the daemon's reply.
pub fn post(state_root: &Path, req: &Request) -> Result<String> {
    let d = dir(state_root);
    std::fs::create_dir_all(&d)?;
    // Microsecond stamp first so filename order is arrival order; the uuid only
    // breaks ties between two clients in the same microsecond.
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let stem = format!("{stamp:020}-{}", mulpex_core::persist::new_uuid());
    let req_path = d.join(format!("{stem}.json"));
    let done_path = d.join(format!("{stem}.done"));

    // Write beside then rename, so the daemon can never read a half-written
    // request on the very tick we are writing it.
    let tmp = d.join(format!("{stem}.tmp"));
    std::fs::write(&tmp, req.to_json())?;
    std::fs::rename(&tmp, &req_path)?;

    let start = Instant::now();
    while start.elapsed() < REPLY_TIMEOUT {
        if let Ok(reply) = std::fs::read_to_string(&done_path) {
            let _ = std::fs::remove_file(&done_path);
            if let Some(err) = reply.strip_prefix("ERR ") {
                bail!("{}", err.trim());
            }
            return Ok(reply.trim().to_string());
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    let _ = std::fs::remove_file(&req_path);
    bail!(
        "the mulpex daemon did not answer in {}s — check {}",
        REPLY_TIMEOUT.as_secs(),
        state_root.join("daemon.log").display()
    )
}

/// Daemon side: take every pending request, oldest first.
pub fn take_all(state_root: &Path) -> Vec<(PathBuf, Request)> {
    let d = dir(state_root);
    let Ok(entries) = std::fs::read_dir(&d) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
        .collect();
    files.sort();

    let mut out = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        if let Some(req) = Request::from_json(&text) {
            out.push((path, req));
        }
    }
    out
}

/// Daemon side: answer one request. `Err` is sent as `ERR <message>` so the
/// client reports the daemon's own words rather than "no reply".
pub fn reply(req_path: &Path, result: Result<String>) {
    let body = match result {
        Ok(s) => s,
        Err(e) => format!("ERR {e:#}"),
    };
    let done = req_path.with_extension("done");
    let _ = std::fs::write(done, body);
}

/// Drop replies nobody collected. Called once a tick; cheap because the directory
/// is empty in the steady state.
pub fn sweep(state_root: &Path) {
    let Ok(entries) = std::fs::read_dir(dir(state_root)) else {
        return;
    };
    let now = SystemTime::now();
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().map(|x| x != "done").unwrap_or(true) {
            continue;
        }
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| now.duration_since(t).unwrap_or_default() > DONE_TTL)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_taken_in_arrival_order() {
        let root = std::env::temp_dir().join(format!("mpxipc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(DIR)).unwrap();
        for (i, name) in ["00000000000000000002-b", "00000000000000000001-a"]
            .iter()
            .enumerate()
        {
            let r = Request {
                op: "new".into(),
                project: "/p".into(),
                arg: i.to_string(),
            };
            std::fs::write(root.join(DIR).join(format!("{name}.json")), r.to_json()).unwrap();
        }
        let taken = take_all(&root);
        assert_eq!(taken.len(), 2);
        // The stamp, not the write order, decides — that is what makes two racing
        // `mpx new` calls allocate distinct ids deterministically.
        assert_eq!(taken[0].1.arg, "1");
        assert_eq!(taken[1].1.arg, "0");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_error_reply_reaches_the_client_as_its_own_message() {
        let root = std::env::temp_dir().join(format!("mpxipc2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(DIR)).unwrap();
        let req = root.join(DIR).join("x.json");
        std::fs::write(&req, "{}").unwrap();
        reply(&req, Err(anyhow::anyhow!("claude not found")));
        let body = std::fs::read_to_string(root.join(DIR).join("x.done")).unwrap();
        assert_eq!(body, "ERR claude not found");
        let _ = std::fs::remove_dir_all(&root);
    }
}
