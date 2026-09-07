//! `mpx` — Mulpex in a terminal, for working over ssh.
//!
//! tmux is the terminal multiplexer: it owns every PTY, so this binary contains
//! no VT emulation, no key encoding, no scrollback and no mouse handling — the
//! four things the original terminal-UI Mulpex hand-rolled and the desktop
//! rewrite deleted. What it does own is the coordination hub, and that is shared
//! verbatim with the desktop app through `mulpex-core`.
//!
//! The layout is tmux's to draw: every instance window is `[ sidebar | instance ]`
//! with project tabs along the top, so `mpx` arranges panes rather than painting
//! them. See `layout.rs` for the shape and `sidebar.rs` for the only thing here
//! that draws anything at all.

mod bidi;
mod claude;
mod claudewin;
mod core;
mod daemon;
mod fanout;
mod ipc;
mod layout;
mod messages;
mod picker;
mod recents;
mod recorder;
mod remotes;
mod sidebar;
mod spec;
mod statedir;
mod sweep;
mod terminals;
mod tmux;
mod tui;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

fn main() {
    if let Err(e) = run() {
        eprintln!("mpx: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        // Internal: the launcher a tmux window actually runs. Not for humans, but
        // deliberately inspectable — the spec file it names is the whole truth
        // about how a claude was started.
        Some("exec-spec") => {
            let path = args.get(2).context("exec-spec needs a spec file path")?;
            spec::exec(Path::new(path))?;
            unreachable!("exec replaces the process")
        }
        Some("up") => up(args.get(2).map(String::as_str)),
        // The poll loop. Started automatically by `up`; runnable by hand when
        // you want to watch it.
        Some("daemon") => daemon::run(&statedir::state_root(), conf_path()),
        Some("new") => ask("new", "", args.get(2).map(String::as_str)),
        Some("term") => ask("term", args.get(2).map(String::as_str).unwrap_or(""), None),
        Some("close") => {
            let id = args.get(2).context("usage: mpx close <instance-id> [dir]")?;
            ask("close", id, args.get(3).map(String::as_str))
        }
        Some("restart") => {
            let id = args.get(2).context("usage: mpx restart <instance-id> [dir]")?;
            ask("restart", id, args.get(3).map(String::as_str))
        }
        Some("mute") => {
            let id = args.get(2).context("usage: mpx mute <instance-id> [dir]")?;
            ask("mute", id, args.get(3).map(String::as_str))
        }
        Some("unmute") => {
            let id = args.get(2).context("usage: mpx unmute <instance-id> [dir]")?;
            ask("unmute", id, args.get(3).map(String::as_str))
        }
        Some("ls") => ls(),
        Some("messages") | Some("msgs") => {
            let n = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(messages::DEFAULT_MAX);
            show_messages(n)
        }
        Some("doctor") => doctor(),
        Some("down") => down(args.get(2).map(String::as_str)),
        // The front door. No arguments, from anywhere: open the workspace.
        None => open_workspace(),
        // Internal: what a bare key binding runs. Not for humans.
        Some("key") => {
            let action = args.get(2).context("usage: mpx key <action> <window-id>")?;
            let pane = args.get(3).context("usage: mpx key <action> <window-id>")?;
            key_action(action, pane)
        }
        Some("sidebar") => {
            let dir = args.get(2).context("sidebar needs a project directory")?;
            sidebar::run(Path::new(dir), conf_path())
        }
        // The project picker. Runs inside `display-popup -E`, and does its own
        // switching — a popup has nowhere to return a choice to.
        Some("pick") => picker::run(conf_live()),
        Some("-h") | Some("--help") | Some("help") => {
            print_help();
            Ok(())
        }
        Some(other) => {
            print_help();
            bail!("unknown command {other:?}");
        }
    }
}

fn print_help() {
    eprintln!(
        "mpx — Mulpex in a terminal\n\
         \n\
         USAGE\n\
         \x20 mpx                  open Mulpex (this is the one to remember) —\n\
         \x20                      rejoins what is running, else offers a project\n\
         \x20 mpx up [dir]         open a project by path, no questions (default: cwd)\n\
         \x20 mpx new [dir]        add another claude to it\n\
         \x20 mpx term [label]     open a shell terminal in the current project\n\
         \x20 mpx close <id> [dir] close one instance\n\
         \x20 mpx restart <id>     quit and relaunch a claude in place, resuming it\n\
         \x20 mpx mute|unmute <id> quieten a row without touching the claude\n\
         \x20 mpx ls               what is running, everywhere\n\
         \x20 mpx messages [n]     the hub message feed, with reply-able addresses\n\
         \x20 mpx down [dir]       tear this project down (other projects keep running)\n\
         \x20 mpx doctor           check tmux, claude and auth before anything else runs\n\
         \x20 mpx daemon           run the poll loop in the foreground (it starts itself)\n\
         \n\
         INSIDE — four keys, no prefix\n\
         \x20 Ctrl-]  Ctrl-[       next / previous instance\n\
         \x20 Ctrl-T               new claude\n\
         \x20 Ctrl-W               close this instance (asks first)\n\
         \x20 Ctrl-P               open a project — type to filter, ⇥ completes\n\
         \n\
         The strip down the left is the instance list: status, name and unread\n\
         mail per row, updating on its own. Everything else is a command you type\n\
         in a terminal — `mpx term`, `mpx mute <id>`, `mpx restart <id>`,\n\
         `mpx messages`.\n\
         \n\
         A claude you exit closes its row; one that dies in its first 10 seconds\n\
         is kept, marked ✗, because that row is the only place the error is shown.\n\
         Ctrl-[ needs a terminal that reports extended keys (it is byte 0x1b, the\n\
         same as Escape); where it does nothing, Ctrl-] wraps round the other way.\n\
         Mulpex takes Ctrl-T, Ctrl-W and Ctrl-P from claude and the shell. If you\n\
         ever want the real key, C-q C-t / C-w / C-p sends it through.\n"
    );
}

/// Client side of a mutating command: make sure the daemon is up, then post.
///
/// Mutations go through the daemon because instance-id allocation and the reply
/// handshake have to be single-threaded — two `mpx new` at once must not both
/// decide they are claude#3.
fn ask(op: &str, arg: &str, dir_arg: Option<&str>) -> Result<()> {
    println!("{}", ask_inner(op, arg, dir_arg)?);
    Ok(())
}

fn ask_inner(op: &str, arg: &str, dir_arg: Option<&str>) -> Result<String> {
    let dir = project_dir(dir_arg)?;
    let state_root = statedir::state_root();
    let me = std::env::current_exe().context("locating the mpx binary")?;
    daemon::ensure_running(&state_root, &me)?;
    ipc::post(
        &state_root,
        &ipc::Request {
            op: op.to_string(),
            project: dir.to_string_lossy().to_string(),
            arg: arg.to_string(),
        },
    )
}

/// Read-only, and deliberately **does not go through the daemon** — a listing
/// that can be blocked by a busy poll loop is one you stop trusting.
fn ls() -> Result<()> {
    let t = tmux::Tmux::new(conf_path());
    let projects = core::scan(&t)?;
    if projects.is_empty() {
        println!("nothing running");
        return Ok(());
    }
    let root = statedir::state_root();
    let hub = if daemon::heartbeat_fresh(&root) {
        "hub ok".to_string()
    } else {
        format!("HUB DOWN — see {}", daemon::log_path(&root).display())
    };
    println!("{hub}");
    for p in &projects {
        println!("\n{}  {}", p.session, p.dir.display());
        for i in &p.instances {
            let mut notes: Vec<String> = Vec::new();
            if i.dead {
                notes.push(match i.dead_status.as_str() {
                    "" => "exited".to_string(),
                    code => format!("exited {code}"),
                });
            }
            if i.muted {
                notes.push("muted".into());
            }
            // A spawned child's task-delivery verdict. Absent means delivered and
            // verified, so only the exceptions are worth a column.
            if let Some(v) = delivery_note(&p.state_dir, i.id) {
                notes.push(v);
            }
            // The window name carries whatever the instance called itself through
            // `hub_set_name` — model text, in the language the user works in. One
            // named itself in Hebrew the first time this was run, so this line is
            // as much an RTL surface as the messages feed. See `bidi.rs`.
            let name = bidi::visual(&i.window_name);
            println!("  {}#{:<3} {:<34} {}", i.kind, i.id, name, notes.join("  "));
        }
    }
    Ok(())
}

/// The messages feed for the project you are standing in.
///
/// Read-only, and like `ls` it goes **straight to disk, never the daemon** — a
/// feed you can be locked out of by a busy poll loop is one you stop trusting,
/// and this is the surface you reach for precisely when something looks stuck.
fn show_messages(max: usize) -> Result<()> {
    let dir = project_dir(None)?;
    let state_dir = statedir::state_dir_for(&dir);
    let msgs = messages::read(&state_dir.join("messages.log"), max);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    print!("{}", messages::render(&msgs, now));
    Ok(())
}

/// What `spawning/<id>` says, when it says anything. Absent is the good case: the
/// child's own hook removed it, having checked what it actually received.
fn delivery_note(state_dir: &Path, id: usize) -> Option<String> {
    let v = std::fs::read_to_string(mulpex_core::spawn_delivery_path(state_dir, id)).ok()?;
    Some(match v.trim() {
        "pending" => "task not landed yet".into(),
        "failed" => "TASK NEVER LANDED".into(),
        "partial" => "STARTED ON THE WRONG TEXT".into(),
        other => other.to_string(),
    })
}

/// Everything `mpx` needs from the machine, checked in one place so a failure is
/// a sentence rather than a puzzle.
///
/// The three failures this catches all *look* like a Mulpex bug otherwise: an old
/// tmux rejects `-e` with an obscure usage error, a missing `claude` produces a
/// bare ENOENT inside a pane that then vanishes, and a missing token surfaces
/// much later as claude's own "Not logged in · Please run /login".
fn doctor() -> Result<()> {
    let mut problems = 0;

    match tmux::Tmux::version() {
        Ok((maj, min)) => {
            let ok = (maj, min) >= tmux::MIN_VERSION;
            println!(
                "tmux         {maj}.{min}  {}",
                if ok {
                    "ok".to_string()
                } else {
                    problems += 1;
                    format!(
                        "TOO OLD — need {}.{} for `new-window -e` and `display-popup`",
                        tmux::MIN_VERSION.0,
                        tmux::MIN_VERSION.1
                    )
                }
            );
        }
        Err(e) => {
            problems += 1;
            println!("tmux         MISSING — {e}");
        }
    }

    match claude::resolve() {
        Some(p) => println!("claude       {}", p.display()),
        None => {
            problems += 1;
            println!("claude       NOT FOUND on PATH, ~/.local/bin or ~/.claude/local");
        }
    }

    let token = std::env::var_os("CLAUDE_CODE_OAUTH_TOKEN").is_some();
    let creds = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".claude/.credentials.json").exists())
        .unwrap_or(false);
    if token || creds {
        println!(
            "auth         {}",
            if token { "CLAUDE_CODE_OAUTH_TOKEN set" } else { "~/.claude/.credentials.json" }
        );
    } else {
        problems += 1;
        println!(
            "auth         NONE VISIBLE — claude will say \"Not logged in\".\n\
             \x20            Note `ssh host mpx up` is not a login shell, so rc-file\n\
             \x20            exports are absent; ssh in first, then run mpx."
        );
    }

    let helper = claude::helper_path();
    if helper.exists() {
        println!("helper       {}", helper.display());
    } else {
        problems += 1;
        println!("helper       MISSING at {} — hooks and the MCP hub will not run", helper.display());
    }

    println!("home         {}", statedir::cli_home().display());
    println!("state root   {}", statedir::state_root().display());

    if problems > 0 {
        bail!("{problems} problem(s) above");
    }
    Ok(())
}

/// Resolve the project directory the way every later command must: canonically,
/// because `registry::same_dir` and `MULPEX_PROJECT_DIR` are compared literally
/// and `/var` vs `/private/var` is enough to break the match.
fn project_dir(arg: Option<&str>) -> Result<PathBuf> {
    let raw = match arg {
        Some(a) => PathBuf::from(a),
        None => std::env::current_dir().context("getting the current directory")?,
    };
    let dir = std::fs::canonicalize(&raw)
        .with_context(|| format!("resolving {}", raw.display()))?;
    anyhow::ensure!(dir.is_dir(), "{} is not a directory", dir.display());
    Ok(dir)
}

/// The tmux config actually handed to `-f`: the shipped asset with this binary's
/// own absolute path substituted in.
///
/// A key binding that has to run `mpx` cannot rely on `PATH` — the tmux **server**
/// runs `run-shell`, and it inherits whatever environment it was first started
/// with, which may be a login shell from days ago or none at all. Substituting is
/// also what lets the path be quoted correctly, which `#{q:...}` cannot do.
fn conf_path() -> PathBuf {
    let src = conf_source();
    // An unrendered config is still a working config — only the bindings that
    // shell out are dead — so a render failure must not stop tmux starting.
    render_conf(&src).unwrap_or(src)
}

fn conf_source() -> PathBuf {
    // Phase 9 installs this alongside the binary; for now it is read from the
    // source tree so `cargo run` works, falling back to the installed location.
    let beside = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mulpex.tmux.conf")));
    if let Some(p) = beside.filter(|p| p.exists()) {
        return p;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/mulpex.tmux.conf")
}

fn render_conf(src: &Path) -> Result<PathBuf> {
    let me = std::env::current_exe()?;
    let text = std::fs::read_to_string(src)?.replace("__MPX_BIN__", &sh_quote(&me.to_string_lossy()));
    let home = statedir::cli_home();
    std::fs::create_dir_all(&home)?;
    let out = home.join("mulpex.tmux.conf");
    // Write beside then rename. Several `mpx` processes render this concurrently —
    // the daemon, each client, every sidebar — and tmux may be reading it at the
    // same moment. Rename is atomic; a partial `-f` is a server that starts with
    // half a config and no error.
    let tmp = home.join(format!("mulpex.tmux.conf.{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &out)?;
    Ok(out)
}

/// Quote for `/bin/sh`. `#{q:...}` is not this: it escapes tmux's own special
/// characters, and a path containing a quote was measured passing through it raw.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A key binding's route into the daemon.
///
/// Takes a **pane id** (`%12`), never a path: the binding runs through `/bin/sh`
/// and a project directory is arbitrary text. The path is read back out of tmux
/// here, where it stays a `String` and never touches a command line.
/// Runs with **stdout and stderr discarded** — see the config's `>/dev/null 2>&1`,
/// which is there because `run-shell` shows a command's output by dropping the
/// pane you are looking at into view-mode. So a failure has to reach the status
/// line, or the key silently does nothing: this project's most expensive shape.
fn key_action(action: &str, window: &str) -> Result<()> {
    let t = tmux::Tmux::new(conf_live());
    let r = key_run(&t, action, window);
    if let Err(e) = &r {
        let _ = t.message(&format!("mpx: {e:#}"));
    }
    r
}

fn key_run(t: &tmux::Tmux, action: &str, window: &str) -> Result<()> {
    match action {
        // Movement resolves the project from the scan it has to do anyway.
        "next" => step(t, window, 1),
        "prev" => step(t, window, -1),
        "new" | "term" => {
            let dir = t.user_option(window, "@mpx_project");
            anyhow::ensure!(!dir.is_empty(), "{window} is not in a Mulpex project");
            // Not `ask`: its `println!` of the reply is exactly what put a pane
            // into view-mode. The new row in the sidebar is the feedback.
            let reply = ask_inner(action, "", Some(&dir))?;
            // Go there, the way ⌘T does in the app. The rule the app draws — and
            // this follows it — is **who asked**: a person pressing a key lands in
            // what they just made, while a `hub_spawn` child or a
            // `hub_terminal_open` must not yank the screen away from them. Those go
            // through `fanout.rs` and `terminals.rs`, never through here, so the
            // distinction costs no flag.
            if let Some(id) = core::id_in_reply(&reply) {
                focus_instance(t, &dir, id);
            }
            Ok(())
        }
        // Close the instance this key was pressed in. Routed through the daemon
        // like every other close, rather than `kill-window` from the binding, so
        // one tick sees the removal and republishes `instances` — a window that
        // vanishes without the hub noticing leaves peers addressing a dead id.
        "close" => {
            let (dir, id) = instance_at(t, window)?;
            ask_inner("close", &id.to_string(), Some(&dir)).map(|_| ())
        }
        other => bail!("unknown key action {other:?}"),
    }
}

/// `-f` is read when the tmux **server starts** and ignored by every command sent
/// to a running one — so a key binding, which runs on every keystroke, must not
/// re-render the config to talk to it.
fn conf_live() -> PathBuf {
    let rendered = statedir::cli_home().join("mulpex.tmux.conf");
    if rendered.exists() {
        rendered
    } else {
        conf_path()
    }
}

/// Put the keyboard in one instance's pane.
///
/// Best-effort: the daemon creates the window *before* it replies, so the scan
/// always sees it — and if it somehow does not, the instance still exists and is one
/// key away. Failing a creation because the cursor did not move would be the wrong
/// trade entirely.
fn focus_instance(t: &tmux::Tmux, dir: &str, id: usize) {
    let Ok(dir) = std::fs::canonicalize(dir) else { return };
    let Ok(projects) = core::scan(t) else { return };
    for p in projects.iter().filter(|p| p.dir == dir) {
        if let Some(i) = p.instances.iter().find(|i| i.id == id) {
            let _ = t.focus(&i.window, &i.pane);
            return;
        }
    }
}

/// Which instance a window is, resolved from the scan.
///
/// Not `@mpx_id` off the window: that option **is** on the window, but so is
/// `@mpx_project`, which is a *session* option every window inherits — including
/// one made with tmux's own `new-window`. Asking the scan is the only answer that
/// distinguishes "an instance" from "a window in a project's session".
fn instance_at(t: &tmux::Tmux, window: &str) -> Result<(String, usize)> {
    for p in core::scan(t)? {
        if let Some(i) = p.instances.iter().find(|i| i.window == window) {
            return Ok((p.dir.to_string_lossy().to_string(), i.id));
        }
    }
    bail!("this window is not a Mulpex instance")
}

/// Move `delta` instances along **the list the sidebar draws**, wrapping.
///
/// Deliberately not `next-window`. tmux orders windows by index, Mulpex orders
/// instances by id, and nothing keeps those two agreeing — a window killed in the
/// middle frees its index for the next spawn to reuse, and the placeholder that
/// `up()` kills frees index 0 on every project. Walking the same list the user is
/// looking at is the only definition of "next instance" that cannot drift.
///
/// `Instance.pane` is the instance's own pane (`core::scan` skips sidebar panes),
/// so this lands in the claude and never on the strip beside it.
fn step(t: &tmux::Tmux, window: &str, delta: isize) -> Result<()> {
    let projects = core::scan(t)?;
    // Find the project by the window the key was pressed in — no second tmux call
    // to ask which project that is, and no path to canonicalise.
    let Some(project) = projects
        .iter()
        .find(|p| p.instances.iter().any(|i| i.window == window))
        .or_else(|| {
            // Pressed from a window that is not an instance — a shell the user made
            // with tmux's own `new-window` binding. Fall back to naming the project.
            let dir = t.user_option(window, "@mpx_project");
            projects.iter().find(|p| p.dir.to_string_lossy() == dir)
        })
    else {
        return Ok(());
    };
    let n = project.instances.len();
    if n == 0 {
        return Ok(());
    }
    let at = match project.instances.iter().position(|i| i.window == window) {
        Some(at) => (at as isize + delta).rem_euclid(n as isize) as usize,
        // Entering the list from outside it: go to the top rather than stepping
        // away from a position we never had.
        None => 0,
    };
    let target = &project.instances[at];
    t.focus(&target.window, &target.pane)
}

fn up(dir_arg: Option<&str>) -> Result<()> {
    let dir = project_dir(dir_arg)?;
    let t = tmux::Tmux::new(conf_path());
    let session = ensure_session(&t, &dir)?;
    // No "re-attaching to X" line: `attach` clears the screen, so it could only
    // flash. See `open_workspace`.
    t.attach(&session)?;
    unreachable!()
}

/// Make sure `dir` has a tmux session, and return its name. Does **not** attach.
///
/// Split out of `up` for the picker, which opens a project from inside a popup and
/// then `switch-client`s to it — there is no attach in that route at all, and a
/// second copy of this would be a second answer to "what is a Mulpex project" that
/// could drift from the first.
///
/// `dir` must already be canonical (`project_dir`).
fn ensure_session(t: &tmux::Tmux, dir: &Path) -> Result<String> {
    let (maj, min) = tmux::Tmux::version()?;
    anyhow::ensure!(
        (maj, min) >= tmux::MIN_VERSION,
        "tmux {maj}.{min} is too old; need {}.{} for `new-window -e`. Run `mpx doctor`.",
        tmux::MIN_VERSION.0,
        tmux::MIN_VERSION.1
    );

    let session = tmux::session_name(dir);
    let state_root = statedir::state_root();
    let me = std::env::current_exe().context("locating the mpx binary")?;

    // Opening a project is what makes it recent — including re-opening one, which
    // is why this is here and not only on the create path.
    recents::add(dir);

    // Re-attaching is the common case over ssh: the whole point of tmux here is
    // that a dropped connection left everything running.
    if t.has_session(&session) {
        daemon::ensure_running(&state_root, &me)?;
        // The server outlives every client, so its bindings are whatever they were
        // when it started — possibly from an older `mpx`. See `Tmux::source_conf`.
        let _ = t.source_conf();
        return Ok(session);
    }

    let state_dir = statedir::state_dir_for(dir);
    mulpex_core::state_dir::write_state_dir(&state_dir, &claude::helper_path())
        .with_context(|| format!("preparing {}", state_dir.display()))?;

    let (cols, rows) = create_size(t);
    t.new_session(&session, dir, cols, rows)
        .context("creating the tmux session")?;
    // The session carries the project identity, so one `list-panes` tells the
    // daemon everything about every project.
    //
    // **`@mpx_project` is written last, deliberately.** These are two tmux calls
    // with a 200 ms poll running against them, and `core::scan` adopts a session
    // the moment `@mpx_project` is set — so the gate has to be the field written
    // after everything it implies is already there.
    t.set_user_option(&session, false, "@mpx_state_dir", &state_dir.to_string_lossy())?;
    t.set_user_option(&session, false, "@mpx_project", &dir.to_string_lossy())?;

    let p = core::Project {
        session: session.clone(),
        dir: dir.to_path_buf(),
        state_dir,
        instances: Vec::new(),
        active_window: String::new(),
        active_pane: String::new(),
    };
    claudewin::spawn(t, &p, 1, None)?;
    core::publish_instances(&p)?;

    // The placeholder window `new-session` created has served its purpose.
    let _ = t.kill_window(&format!("={session}:0"));

    daemon::ensure_running(&state_root, &me)?;
    // A *new* session on an *old* server: `-f` was read when the server started,
    // which may have been days and one `mpx` update ago. Measured — the first run
    // of a new key binding did nothing at all, with the config on disk correct.
    let _ = t.source_conf();
    Ok(session)
}

fn down(dir_arg: Option<&str>) -> Result<()> {
    let dir = project_dir(dir_arg)?;
    let t = tmux::Tmux::new(conf_path());
    let session = tmux::session_name(&dir);
    if !t.has_session(&session) {
        println!("mpx: nothing running for {}", dir.display());
        return Ok(());
    }
    // Collect the ttys BEFORE killing, because a dead session reports no panes
    // and the sweep would then have nothing to look at.
    let ttys = t.session_ttys(&session).unwrap_or_default();
    let doomed = sweep::pids_to_sweep(&ttys);

    // One session, never `kill-server`: the server hosts every open project, so
    // killing it would take other projects' running claudes with it.
    t.kill_session(&session).context("killing the tmux session")?;

    // `kill-session` sends SIGHUP to each pane's process group, which a `nohup`'d
    // child ignores — measured surviving teardown. Sweep by controlling terminal.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let swept = sweep::kill_pids(&doomed);
    if swept > 0 {
        eprintln!("mpx: swept {swept} process(es) that ignored the hangup");
    }
    let state_dir = statedir::state_dir_for(&dir);
    let _ = std::fs::remove_dir_all(&state_dir);
    let left = t.session_count();
    println!(
        "mpx: {session} torn down{}",
        if left > 0 { format!(" ({left} other project(s) still running)") } else { String::new() }
    );
    Ok(())
}

/// The size to create a detached session at, so the first attach does not resize
/// every pane.
///
/// Two callers with two different ttys. `mpx up` runs on the terminal the session
/// is about to fill, so its own window is the right answer. The **picker** runs
/// inside `display-popup`, where the local tty is the popup's 70%×60% box — a
/// session created at that size would spawn its claude into a pane materially
/// smaller than the one it is a second away from being shown in. So when there is
/// a tmux client to ask, ask it.
fn create_size(t: &tmux::Tmux) -> (u16, u16) {
    if std::env::var_os("TMUX").is_some() {
        if let Some(wh) = client_size(t) {
            return wh;
        }
    }
    terminal_size().unwrap_or((120, 40))
}

fn client_size(t: &tmux::Tmux) -> Option<(u16, u16)> {
    let out = t.display_here("#{client_width} #{client_height}").ok()?;
    let mut fields = out.split_whitespace();
    let w: u16 = fields.next()?.parse().ok()?;
    let h: u16 = fields.next()?.parse().ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

/// The size of the tty this process is on. A detached tmux server otherwise
/// assumes 80x24.
fn terminal_size() -> Option<(u16, u16)> {
    // SAFETY: `ioctl(TIOCGWINSZ)` on stdout; `ws` is fully written by the kernel
    // or the call fails, and we only read it on success.
    #[repr(C)]
    struct WinSize {
        rows: u16,
        cols: u16,
        xpix: u16,
        ypix: u16,
    }
    extern "C" {
        fn ioctl(fd: i32, req: u64, ...) -> i32;
    }
    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: u64 = 0x4008_7468;
    #[cfg(not(target_os = "macos"))]
    const TIOCGWINSZ: u64 = 0x5413;
    let mut ws = WinSize { rows: 0, cols: 0, xpix: 0, ypix: 0 };
    let rc = unsafe { ioctl(1, TIOCGWINSZ, &mut ws as *mut WinSize) };
    if rc == 0 && ws.cols > 0 && ws.rows > 0 {
        Some((ws.cols, ws.rows))
    } else {
        None
    }
}

/// `mpx`, with nothing after it. The front door.
///
/// Attaches to whatever is already open, so the common case over ssh — reconnect
/// after a dropped connection — is the shortest possible command. With nothing
/// open it falls back to the project you are standing in, which is what someone
/// typing `mpx` inside a repo almost always means.
fn open_workspace() -> Result<()> {
    let t = tmux::Tmux::new(conf_path());
    let state_root = statedir::state_root();
    let me = std::env::current_exe().context("locating the mpx binary")?;

    let open = t.sessions();
    if !open.is_empty() {
        // Prefer the project you are standing in. Someone who types `mpx` inside a
        // repo means that repo, even when four others are open; the tabs are there
        // for the times they meant something else.
        let here = std::env::current_dir()
            .ok()
            .map(|d| tmux::session_name(&d))
            .filter(|name| open.iter().any(|s| s == name));
        let session = here.unwrap_or_else(|| open[0].clone());
        daemon::ensure_running(&state_root, &me)?;
        let _ = t.source_conf();
        t.attach(&session)?;
        unreachable!()
    }

    // Nothing open, so **offer rather than guess**. `mpx` used to create a project
    // for whatever directory the shell happened to be in, which is wrong for the
    // same reason an IDE does not do it: the odds are good but not good enough, and
    // the cost of being wrong is a project you now have to notice and close. The
    // picker puts that directory at the top of the list and waits — one keystroke
    // when the guess would have been right, and a list when it would not.
    picker::run(conf_path())
}
