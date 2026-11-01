//! `mulpex-helper send` — a plain script (not a claude) drops a hub message
//! into one instance's inbox, waking it exactly like `hub_send` does.
//!
//! ```text
//! mulpex-helper send --project cloudraw --to 9 --from slack-bridge [--file msg.txt | -m TEXT]
//! ```
//!
//! With neither `--file` nor `-m`, the body is read from stdin.
//!
//! **Why it exists.** A Monitor a claude arms itself expires after 30 minutes
//! (see the root CLAUDE.md), so a Slack/Trello watch kept that way silently
//! stops once nobody re-arms it. The hub listener never expires, so an
//! always-on service on the same machine can push into it instead.
//!
//! **How it finds the hub.** No `MULPEX_STATE_DIR` here — a script was not
//! spawned by Mulpex. The state root is `<temp>/mulpex-<daemon pid>`
//! (`mulpex-ptyd`'s `daemon/mod.rs`), and the daemon writes its pid to
//! `<mulpex home>/ptyd/ptyd.pid`. `MULPEX_STATE_ROOT` overrides both, for a
//! service whose `TMPDIR` differs from the app's.
//!
//! **What it refuses.** Mulpex not running, an unknown project, an instance
//! that is not live — exit 1, the reason on stderr, nothing written. No
//! queueing: Mulpex reaps a dead instance's inbox, so a message held for a
//! closed instance would rot. The caller retries.
//!
//! **How it reads.** The file carries `"from": 0` plus `"from_script"`, which
//! `hub_inbox` shows as `<from> (script)`. `0` is never a live id, so
//! `bounce_dead_inbox` skips it, and `hub_send` to a `(script)` sender is
//! refused with a clear reason (`mcp::parse_target`).

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::registry::{Address, ProjectEntry, Registry};

/// Largest body accepted, in bytes. A bridge sends a preview plus a file path.
pub const MAX_BODY: usize = 8 * 1024;
/// Largest `--from` label, in characters.
pub const MAX_FROM: usize = 40;
/// The suffix `hub_inbox` puts after a script's label.
pub const SCRIPT_SUFFIX: &str = " (script)";

const USAGE: &str = "usage: mulpex-helper send --project <name|path> --to <n|claude#n|sidebar name> \
--from <label> [--file <path> | -m <text>]   (body on stdin if neither)\n\
  --to may also be <project>#<n>, in which case --project is optional.\n\
  exit 0 = delivered, 1 = refused (reason on stderr), 2 = bad usage.";

#[derive(Debug, Default)]
struct Args {
    project: Option<String>,
    to: Option<String>,
    from: Option<String>,
    file: Option<PathBuf>,
    message: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match flag.as_str() {
            "--project" | "-p" => a.project = Some(val(flag)?),
            "--to" | "-t" => a.to = Some(val(flag)?),
            "--from" | "-f" => a.from = Some(val(flag)?),
            "--file" => a.file = Some(PathBuf::from(val(flag)?)),
            "--message" | "-m" => a.message = Some(val(flag)?),
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(a)
}

pub fn run(args: &[String]) -> anyhow::Result<()> {
    let a = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            if e.is_empty() {
                println!("{USAGE}");
                std::process::exit(0);
            }
            eprintln!("mulpex-helper send: {e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    let body = match read_body(&a) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("mulpex-helper send: {e}");
            std::process::exit(2);
        }
    };
    let root = match find_state_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mulpex-helper send: {e}");
            std::process::exit(1);
        }
    };
    match send(&root, &a, &body) {
        Ok(addr) => {
            println!("delivered to {addr}");
            Ok(())
        }
        Err(e) => {
            eprintln!("mulpex-helper send: {e}");
            std::process::exit(if e.starts_with("usage:") { 2 } else { 1 });
        }
    }
}

fn read_body(a: &Args) -> Result<String, String> {
    let body = match (&a.file, &a.message) {
        (Some(_), Some(_)) => return Err("give --file or -m, not both".into()),
        (Some(p), None) => std::fs::read_to_string(p)
            .map_err(|e| format!("could not read {}: {e}", p.display()))?,
        (None, Some(m)) => m.clone(),
        (None, None) => {
            let mut s = String::new();
            std::io::stdin()
                .take(MAX_BODY as u64 + 1)
                .read_to_string(&mut s)
                .map_err(|e| format!("could not read stdin: {e}"))?;
            s
        }
    };
    check_body(&body)?;
    Ok(body)
}

fn check_body(body: &str) -> Result<(), String> {
    if body.trim().is_empty() {
        return Err("the message is empty".into());
    }
    if body.len() > MAX_BODY {
        return Err(format!(
            "the message is over {MAX_BODY} bytes — send a short preview and a file path instead"
        ));
    }
    Ok(())
}

fn check_from(from: &str) -> Result<String, String> {
    let f = from.trim();
    if f.is_empty() {
        return Err("usage: --from is empty".into());
    }
    if f.chars().count() > MAX_FROM {
        return Err(format!("usage: --from is over {MAX_FROM} characters"));
    }
    if f.chars().any(|c| c.is_control()) {
        return Err("usage: --from may not contain tabs, newlines or control characters".into());
    }
    Ok(f.to_string())
}

/// `MULPEX_STATE_ROOT`, else `<temp>/mulpex-<pid in <home>/ptyd/ptyd.pid>`.
pub fn find_state_root() -> Result<PathBuf, String> {
    if let Some(r) = std::env::var_os("MULPEX_STATE_ROOT").filter(|r| !r.is_empty()) {
        let r = PathBuf::from(r);
        return if r.join(crate::registry::REGISTRY_FILE).is_file() {
            Ok(r)
        } else {
            Err(format!("MULPEX_STATE_ROOT={} holds no registry.json — is Mulpex running?", r.display()))
        };
    }
    let home = crate::mulpex_home();
    let pid_file = home.join("ptyd").join("ptyd.pid");
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| format!("Mulpex is not running (no {}).", pid_file.display()))?;
    if unsafe { libc::kill(pid, 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
    {
        return Err(format!("Mulpex is not running (its PTY daemon, pid {pid}, is gone)."));
    }
    let root = std::env::temp_dir().join(format!("mulpex-{pid}"));
    if root.join(crate::registry::REGISTRY_FILE).is_file() {
        Ok(root)
    } else {
        Err(format!(
            "Mulpex's daemon (pid {pid}) is up, but {} holds no registry.json. Either no project \
             is open, or this process's TMPDIR differs from the app's — then set \
             MULPEX_STATE_ROOT to the app's mulpex-{pid} directory.",
            root.display()
        ))
    }
}

/// Resolve and deliver. Returns the address delivered to (`cloudraw#9`).
fn send(root: &Path, a: &Args, body: &str) -> Result<String, String> {
    let from = check_from(a.from.as_deref().unwrap_or("script"))?;
    let to = a.to.as_deref().map(str::trim).filter(|t| !t.is_empty()).ok_or("usage: --to is required")?;
    let reg = Registry::read(root);

    // `--to cloudraw#9` carries its own project; `--project` then must agree or be absent.
    let (project_q, to_word) = match (a.project.as_deref(), to.rsplit_once('#')) {
        (None, Some((q, n))) if n.trim().parse::<usize>().is_ok() && crate::agent::Agent::parse(q.trim()).is_none() => {
            (q.trim().to_string(), n.trim().to_string())
        }
        (Some(p), _) => (p.trim().to_string(), to.to_string()),
        (None, _) => return Err("usage: --project is required (or give --to as <project>#<n>)".into()),
    };
    // `resolve`'s errors end with a hint meant for a claude; a script has no MCP tools.
    let project = reg
        .resolve(&project_q)
        .map_err(|e| e.replace(" Call mcp__mulpex__hub_instances to see them with their instances.", ""))?;
    let id = resolve_instance(project, &to_word)?;

    let addr = crate::addr(Path::new(&project.state_dir), id);
    let shown = format!("{from}{SCRIPT_SUFFIX}");
    let payload = json!({
        "from": 0,
        "from_script": from,
        "ts": crate::hook::now(),
        "body": body,
    });
    let state_dir = Path::new(&project.state_dir);
    let dir = project.inbox_dir(id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not reach {addr}'s inbox: {e}"))?;
    // Written beside the inbox and renamed in, so the listener never counts —
    // and `hub_inbox` never consumes — half a file.
    let uuid = crate::persist::new_uuid();
    let tmp = state_dir.join(format!(".send-{uuid}.tmp"));
    std::fs::write(&tmp, payload.to_string())
        .and_then(|_| std::fs::rename(&tmp, dir.join(format!("{uuid}.json"))))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("could not deliver to {addr}: {e}")
        })?;
    crate::mcp::log_message(&project.messages_log(), &shown, &addr, body);
    Ok(format!("{}#{id}", project.name))
}

/// `9`, `claude#9`, `codex#9`, or a sidebar name (case-insensitive), among
/// the project's LIVE instances only.
fn resolve_instance(p: &ProjectEntry, word: &str) -> Result<usize, String> {
    let live: Vec<String> = p.instances.iter().map(|i| format!("{}#{}", p.name, i.id)).collect();
    let not_running = |what: String| {
        format!(
            "{what} is not a running instance in project \"{}\" — nothing was sent. {}",
            p.name,
            if live.is_empty() { "No claude is running there.".to_string() } else { format!("Running: {}.", live.join(", ")) }
        )
    };
    if let Ok(Address::Local(id)) = crate::registry::parse_address(word) {
        return if p.has_instance(id) { Ok(id) } else { Err(not_running(format!("{}#{id}", p.name))) };
    }
    let want = word.trim().to_lowercase();
    let hits: Vec<usize> = p
        .instances
        .iter()
        .filter(|i| i.name.as_deref().is_some_and(|n| n.trim().to_lowercase() == want))
        .map(|i| i.id)
        .collect();
    match hits.as_slice() {
        [id] => Ok(*id),
        [] => Err(not_running(format!("\"{}\"", word.trim()))),
        many => Err(format!(
            "\"{}\" is the sidebar name of {} instances ({}) — nothing was sent. Use a number.",
            word.trim(),
            many.len(),
            many.iter().map(|i| format!("{}#{i}", p.name)).collect::<Vec<_>>().join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::InstanceEntry;

    fn setup(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("mpx-send-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sd = root.join("cloudraw-key");
        std::fs::create_dir_all(sd.join("inbox")).unwrap();
        let reg = Registry {
            projects: vec![ProjectEntry {
                handle: 1,
                name: "cloudraw".into(),
                dir: "/work/cloudraw".into(),
                state_dir: sd.to_string_lossy().into(),
                instances: vec![
                    InstanceEntry { id: 9, status: "waiting".into(), task: String::new(), name: Some("Orchestrator".into()), name_by: Some("user".into()) },
                    InstanceEntry { id: 12, status: "working".into(), task: String::new(), name: None, name_by: None },
                ],
            }],
        };
        std::fs::write(root.join(crate::registry::REGISTRY_FILE), reg.to_json()).unwrap();
        (root, sd)
    }

    fn args(project: Option<&str>, to: &str, from: &str) -> Args {
        Args { project: project.map(Into::into), to: Some(to.into()), from: Some(from.into()), ..Default::default() }
    }

    fn inbox_files(sd: &Path, id: usize) -> Vec<serde_json::Value> {
        std::fs::read_dir(sd.join("inbox").join(id.to_string()))
            .map(|d| d.flatten().map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).unwrap()).unwrap()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn delivers_into_the_inbox_and_the_log() {
        let (root, sd) = setup("ok");
        let addr = send(&root, &args(Some("cloudraw"), "9", "slack-bridge"), "new ask in #ops").unwrap();
        assert_eq!(addr, "cloudraw#9");
        let files = inbox_files(&sd, 9);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["from"], 0);
        assert_eq!(files[0]["from_script"], "slack-bridge");
        assert_eq!(files[0]["body"], "new ask in #ops");
        let log = std::fs::read_to_string(sd.join("messages.log")).unwrap();
        assert!(log.contains("\tslack-bridge (script)\tclaude#9\tnew ask in #ops\n"), "{log}");
        // No temp file left behind at the state-dir root.
        assert!(std::fs::read_dir(&sd).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[test]
    fn every_way_to_name_the_target() {
        let (root, sd) = setup("names");
        for (p, to) in [(Some("cloudraw"), "claude#9"), (Some("cloudraw"), "orchestrator"), (None, "cloudraw#9"), (Some("/work/cloudraw"), "9")] {
            assert_eq!(send(&root, &args(p, to, "b"), "x").unwrap(), "cloudraw#9", "{p:?} {to}");
        }
        assert_eq!(inbox_files(&sd, 9).len(), 4);
    }

    #[test]
    fn refuses_what_is_not_running_and_writes_nothing() {
        let (root, sd) = setup("refuse");
        let e = send(&root, &args(Some("cloudraw"), "3", "b"), "x").unwrap_err();
        assert!(e.contains("cloudraw#3 is not a running instance") && e.contains("cloudraw#9, cloudraw#12"), "{e}");
        assert!(send(&root, &args(Some("nope"), "9", "b"), "x").is_err());
        assert!(send(&root, &args(Some("cloudraw"), "Nobody", "b"), "x").is_err());
        assert!(send(&root, &args(None, "9", "b"), "x").unwrap_err().starts_with("usage:"));
        assert!(send(&root, &args(Some("cloudraw"), "9", "bad\tname"), "x").unwrap_err().starts_with("usage:"));
        assert!(!sd.join("inbox").join("3").exists());
        assert!(inbox_files(&sd, 9).is_empty());
    }

    #[test]
    fn body_limits() {
        assert!(check_body("   \n").is_err());
        assert!(check_body(&"a".repeat(MAX_BODY)).is_ok());
        assert!(check_body(&"a".repeat(MAX_BODY + 1)).is_err());
    }
}
