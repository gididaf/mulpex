//! The poll loop: the CLI's equivalent of `src-tauri/src/hub.rs`.
//!
//! One daemon serves every project on the `mulpex` tmux server. It ticks every
//! 200 ms — reaping dead windows, republishing `instances`, applying
//! `hub_set_name`, rewriting `registry.json` — and answers the client requests
//! that mutate state.
//!
//! **Why a detached process and not a tmux window.** A window is something the
//! user can close. Closing it would stop the hub with no error anywhere, which is
//! this codebase's most expensive failure shape ("a real event with nowhere to
//! arrive"), self-inflicted. It would also have to belong to one session while
//! serving all of them, and would occupy a window index.
//!
//! But the tmux-window design gets one thing right — you can *see* it die — so
//! that is built in explicitly: the daemon touches `daemon.heartbeat` every tick,
//! and anything reading the hub can tell that a stale heartbeat means the loop is
//! gone. An invisible poll loop is worse than a fragile one.
//!
//! **Restartability is free** because tmux holds the state (see `core.rs`). A
//! daemon that is killed and restarted re-reads every window's `@mpx_id` and
//! carries on; there is no separate registry that could disagree with reality,
//! and so no "adopt or wipe" decision to get backwards.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};

use crate::core::{self, Project};
use crate::tmux::Tmux;

pub const TICK: Duration = Duration::from_millis(200);

/// A heartbeat older than this means the loop is not running. Generous against
/// the 200 ms tick so a slow `list-panes` under load never reads as death.
pub const HEARTBEAT_STALE: Duration = Duration::from_secs(3);

pub fn pidfile(state_root: &Path) -> PathBuf {
    state_root.join("daemon.pid")
}
pub fn heartbeat_path(state_root: &Path) -> PathBuf {
    state_root.join("daemon.heartbeat")
}
pub fn log_path(state_root: &Path) -> PathBuf {
    state_root.join("daemon.log")
}

/// Is a daemon currently running for this state root?
pub fn is_running(state_root: &Path) -> bool {
    let Some(pid) = read_pid(state_root) else { return false };
    process_alive(pid)
}

fn read_pid(state_root: &Path) -> Option<i32> {
    std::fs::read_to_string(pidfile(state_root))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// `kill(pid, 0)` — asks the kernel whether the pid exists without signalling it.
fn process_alive(pid: i32) -> bool {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // SAFETY: signal 0 performs error checking only and delivers nothing.
    unsafe { kill(pid, 0) == 0 }
}

/// True when the loop has ticked recently. Distinguishes "the hub is running"
/// from "a process with that pid exists" — a daemon wedged on a hung `tmux` call
/// is alive by pid and dead by heartbeat, and the second is the useful answer.
pub fn heartbeat_fresh(state_root: &Path) -> bool {
    std::fs::metadata(heartbeat_path(state_root))
        .and_then(|m| m.modified())
        .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() < HEARTBEAT_STALE)
        .unwrap_or(false)
}

/// Start a daemon if none is running. Idempotent: safe to call from every client
/// command, which is what makes `mpx up` work with no separate setup step.
pub fn ensure_running(state_root: &Path, exe: &Path) -> Result<()> {
    if is_running(state_root) {
        return Ok(());
    }
    std::fs::create_dir_all(state_root)?;
    // A stale pidfile from a crashed daemon is expected, not exceptional: the
    // tmux sessions it was serving are still alive, and the new daemon adopts
    // them by re-reading tmux. Nothing is wiped.
    let _ = std::fs::remove_file(pidfile(state_root));

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(state_root))?;
    let err = log.try_clone()?;
    std::process::Command::new(exe)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(err))
        .spawn()
        .context("starting the mulpex daemon")?;

    // Wait for it to claim the pidfile, so a client that immediately posts a
    // request does not time out against a daemon that has not started polling.
    for _ in 0..50 {
        if is_running(state_root) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    bail!("the daemon did not start — see {}", log_path(state_root).display())
}

/// The loop. Runs until the tmux server is gone.
pub fn run(state_root: &Path, conf: PathBuf) -> Result<()> {
    std::fs::create_dir_all(state_root)?;
    // O_EXCL is the lock: two daemons on one state root would both allocate
    // instance ids and both answer requests.
    let mut f = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(pidfile(state_root))
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if is_running(state_root) {
                bail!("a daemon is already running for {}", state_root.display());
            }
            std::fs::remove_file(pidfile(state_root))?;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(pidfile(state_root))?
        }
        Err(e) => return Err(e.into()),
    };
    use std::io::Write;
    write!(f, "{}", std::process::id())?;
    drop(f);

    let t = Tmux::new(conf);
    eprintln!("[daemon] up, pid {}, root {}", std::process::id(), state_root.display());

    // The recorder is the one piece of the loop that carries state between ticks
    // (each terminal's scroll position and its open log handle). Losing it costs
    // nothing correctness-wise — a restarted daemon adopts each pane's current
    // `history_size` rather than replaying it — so it lives here, not on disk.
    let mut rec = crate::recorder::Recorder::default();
    // Likewise the fan-out queue: a batch mid-flight and the children whose first
    // turn we are still waiting on.
    let mut fan = crate::fanout::Fanout::default();
    // ...and the remote peers we are waiting on an answer from.
    let mut rem = crate::remotes::Remotes::default();
    // ...and the last thing written to each project's session store, so an idle
    // project costs a string compare rather than a disk write every 200 ms.
    let mut saver = crate::restore::Saver::default();

    let mut ticked_once = false;
    loop {
        match tick(&t, state_root, &mut rec, &mut fan, &mut rem, &mut saver) {
            Ok(sessions) => {
                // Exit only after a successful tick, so we never race our own
                // startup against a tmux server that is still coming up.
                if ticked_once && sessions == 0 {
                    eprintln!("[daemon] no sessions left, exiting");
                    break;
                }
                ticked_once = true;
            }
            Err(e) => eprintln!("[daemon] tick error: {e:#}"),
        }
        std::thread::sleep(TICK);
    }
    let _ = std::fs::remove_file(pidfile(state_root));
    let _ = std::fs::remove_file(heartbeat_path(state_root));
    Ok(())
}

/// One pass. Returns how many projects are open.
#[allow(clippy::too_many_arguments)]
fn tick(
    t: &Tmux,
    state_root: &Path,
    rec: &mut crate::recorder::Recorder,
    fan: &mut crate::fanout::Fanout,
    rem: &mut crate::remotes::Remotes,
    saver: &mut crate::restore::Saver,
) -> Result<usize> {
    let projects = core::scan(t)?;

    // One allocator per project for the whole tick. Terminal requests and client
    // requests both create windows, and both used to derive an id from the same
    // pre-tick scan — so a `hub_terminal_open` and an `mpx new` landing in the
    // same 200 ms would both have been told they were #5. Ids are identity here,
    // not position: two windows sharing one is two conversations at one address.
    let mut next_ids: HashMap<PathBuf, usize> =
        projects.iter().map(|p| (p.dir.clone(), p.next_id())).collect();

    for p in &projects {
        let mut removed = 0usize;
        for inst in &p.instances {
            if inst.dead {
                match core::reap_dead(t, inst) {
                    Ok(core::Reaped::Marked) => {
                        eprintln!("[daemon] {} {} failed to start, row kept", p.session, inst.window_name)
                    }
                    Ok(core::Reaped::Removed) => {
                        removed += 1;
                        eprintln!("[daemon] {} {} exited, row removed", p.session, inst.window_name)
                    }
                    Ok(core::Reaped::Nothing) => {}
                    Err(e) => eprintln!("[daemon] reaping {}: {e:#}", inst.window),
                }
            }
        }
        // The last claude exited on its own (`/exit`, or a crash past its grace
        // period), which destroys the session. Same event as closing it by hand.
        if removed > 0 && removed == p.instances.len() {
            closed_its_last_row(p);
        }
        if let Err(e) = core::publish_instances(p) {
            eprintln!("[daemon] {}: {e:#}", p.session);
        }
        // Where the keyboard is, so the window sidebars can read it off disk
        // instead of each asking tmux. See `core::publish_focus`.
        core::publish_focus(p);
        if let Err(e) = core::process_name_requests(t, p) {
            eprintln!("[daemon] {} names: {e:#}", p.session);
        }
        // A ⌘M-style mute is undone by the user simply prompting the row again.
        if let Err(e) = core::process_user_prompts(t, p) {
            eprintln!("[daemon] {} unmute: {e:#}", p.session);
        }
        // Terminal ops run before the transcripts are updated, so a `send` and
        // the output it produces cannot be separated by a whole tick.
        if let Some(next) = next_ids.get_mut(&p.dir) {
            crate::terminals::process_requests(t, p, rec, rem, next);
        }
    }

    // `hub_spawn`, drip-fed. Shares the tick's id allocator with everything else
    // that creates a window, so a fan-out and an `mpx new` in the same 200 ms
    // cannot be handed the same number.
    fan.tick(t, &projects, &mut next_ids);

    // Recording runs on the scan taken above, which is one tick behind any window
    // the requests just created. That is the intended order: a terminal starts
    // being recorded from its first *full* tick, and its seed waits in the
    // recorder until then.
    rec.tick(t, &projects);

    // Remote peers, read from the transcripts the recorder just refreshed — so a
    // signal is never a tick staler than the output that carried it.
    rem.tick(&projects);

    // The conversations, so a reboot or an `mpx down` is recoverable. Like the
    // recorder above it works from the scan taken at the top of the tick, so a
    // window created or killed by this tick's requests is accounted for on the
    // next one — which is the right lag in both directions: a brand-new claude has
    // not worked yet and would not be saved anyway, and a closed one drops out of
    // the store because you closed it.
    saver.tick(&projects);

    write_registry(state_root, &projects);

    for (path, req) in crate::ipc::take_all(state_root) {
        let result = handle(t, state_root, &projects, &req, &mut next_ids);
        if let Err(e) = &result {
            eprintln!("[daemon] {} failed: {e:#}", req.op);
        }
        crate::ipc::reply(&path, result);
    }
    crate::ipc::sweep(state_root);

    let _ = std::fs::write(heartbeat_path(state_root), "");
    Ok(projects.len())
}

/// A project leaves the reopen set when its **last row goes**, and only then.
///
/// Keyed on the deliberate act, never on the session being absent — which is the
/// distinction this got wrong first time, expensively. A killed tmux server makes
/// every project vanish at once, and read as "the user closed them" it emptied
/// `open.txt`: the reopen set was wiped by exactly the event it exists for.
/// Measured 2026-09-07, in the probe written to prove the opposite.
///
/// The two facts are not distinguishable from the outside a tick later, so this
/// does not try. Removing the last instance is something the daemon *does* — via
/// `mpx close`, Ctrl-W, or reaping a claude that exited on its own — and a server
/// dying is something it never sees. Only the first touches the file.
///
/// The store is untouched either way: reopen the project by name and its claudes
/// come back. Stop is not forget.
fn closed_its_last_row(p: &Project) {
    crate::openset::remove(&p.dir);
    eprintln!("[daemon] {} closed its last instance, left the reopen set", p.dir.display());
}

/// Publish every open project so `<project>#<n>` addressing resolves.
fn write_registry(state_root: &Path, projects: &[Project]) {
    let entries: Vec<_> = projects
        .iter()
        .enumerate()
        .map(|(i, p)| core::registry_entry(p, i as u64 + 1))
        .collect();
    let reg = mulpex_core::registry::Registry { projects: entries };
    mulpex_core::registry::Registry::write_if_changed(state_root, &reg);
}

/// Handle one client request. Every mutating op lands here, single-threaded, so
/// instance-id allocation cannot race.
fn handle(
    t: &Tmux,
    state_root: &Path,
    projects: &[Project],
    req: &crate::ipc::Request,
    next_ids: &mut HashMap<PathBuf, usize>,
) -> Result<String> {
    let dir = PathBuf::from(&req.project);
    let existing = projects.iter().find(|p| p.dir == dir);
    let mut take_id = |p: &Project| -> usize {
        let slot = next_ids.entry(p.dir.clone()).or_insert_with(|| p.next_id());
        let id = *slot;
        *slot += 1;
        id
    };

    match req.op.as_str() {
        "new" => {
            let p = existing.context("that project is not open")?;
            let id = take_id(p);
            crate::claudewin::spawn(t, p, id, None)?;
            core::publish_instances(&refetch(t, &dir)?)?;
            Ok(format!("claude#{id}"))
        }
        // Quit and relaunch in place, resuming the conversation. Keeps the number,
        // the name, the window position and the inbox.
        "restart" => {
            let p = existing.context("that project is not open")?;
            let id: usize = req.arg.trim().parse().context("restart needs an instance id")?;
            crate::claudewin::restart(t, p, id)?;
            Ok(format!("restarted claude#{id} (resuming its conversation)"))
        }
        "mute" | "unmute" => {
            let p = existing.context("that project is not open")?;
            let id: usize = req.arg.trim().parse().context("that needs an instance id")?;
            let on = req.op == "mute";
            crate::claudewin::set_muted(t, p, id, on)?;
            Ok(format!("claude#{id} {}", if on { "muted" } else { "unmuted" }))
        }
        // A terminal by hand. Same window kind and same id counter as one an
        // instance opens through `hub_terminal_open` — the only difference is who
        // asked.
        "term" => {
            let p = existing.context("that project is not open")?;
            let id = take_id(p);
            let label = Some(req.arg.trim()).filter(|s| !s.is_empty());
            crate::terminals::spawn_terminal_window(t, p, id, label)?;
            Ok(format!("term#{id}"))
        }
        "close" => {
            let p = existing.context("that project is not open")?;
            let id: usize = req.arg.trim().parse().context("close needs an instance id")?;
            let inst = p.find(id).with_context(|| format!("no instance #{id}"))?;
            t.kill_window(&inst.window)?;
            // Killing the only window destroys the session, so this *is* closing
            // the project — the same act as `mpx down`, reached a different way.
            if p.instances.len() == 1 {
                closed_its_last_row(p);
            }
            Ok(format!("closed #{id}"))
        }
        "ping" => Ok(format!("pid {} root {}", std::process::id(), state_root.display())),
        other => bail!("unknown op {other:?}"),
    }
}

/// Re-read one project immediately after changing it, so the reply and the files
/// we publish describe the new state rather than the pre-tick one.
fn refetch(t: &Tmux, dir: &Path) -> Result<Project> {
    core::scan(t)?
        .into_iter()
        .find(|p| p.dir == dir)
        .context("project vanished")
}
