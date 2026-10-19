//! What the kernel knows about a process: its controlling terminal, its
//! parent, its command line, a shell's foreground job — and the teardown built
//! on those (the tty sweep, a claude's background groups, orphaned listeners).
//! Moved out of the app's `pty.rs` so the daemon (which now owns the PTYs and
//! kills sessions) and the app (shell state, the listener reaper) share one copy.

/// What a shell terminal is doing right now, as far as the kernel knows.
///
/// `running` on a terminal only ever meant "the shell process is alive", which
/// is not the question a reader usually has — a shell sitting at its prompt and
/// a shell three minutes into a build are both `running: true`, and from the
/// transcript alone the two are indistinguishable for a terminal the *user* is
/// driving (a command Mulpex submitted itself is tracked by its completion
/// marker, but nothing typed by hand is).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellState {
    /// A foreground command is running in this terminal.
    pub command_running: bool,
    /// The shell's own working directory. Unaffected by a foreground job, which
    /// cannot change it.
    pub cwd: Option<String>,
}

/// Ask the kernel what `pid`'s shell is doing. macOS-only, `None` elsewhere —
/// the same shape as the tty sweep, and `None` means "not known", never "no".
#[cfg(target_os = "macos")]
pub fn shell_state_of(pid: libc::pid_t) -> Option<ShellState> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if n != size {
        return None;
    }
    // `e_tpgid` is the process group the tty currently has in the FOREGROUND;
    // `pbi_pgid` is the shell's own. An interactive shell puts every job it runs
    // into a process group of its own and hands the terminal to it, so the two
    // differing IS a foreground command running — no heuristics, no parsing the
    // prompt. A shell with no controlling terminal reports 0, which is not a
    // disagreement worth reporting.
    let command_running = info.e_tpgid != 0 && info.e_tpgid != info.pbi_pgid;
    Some(ShellState {
        command_running,
        cwd: cwd_of(pid),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn shell_state_of(_pid: libc::pid_t) -> Option<ShellState> {
    None
}

/// `pid`'s current working directory.
#[cfg(target_os = "macos")]
pub fn cwd_of(pid: libc::pid_t) -> Option<String> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if n != size {
        return None;
    }
    // `vip_path` is a flat NUL-terminated buffer that `libc` declares as nested
    // arrays to keep an old rustc happy; read it as the C string it is.
    let raw = unsafe { std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
    let path = raw.to_str().ok()?;
    (!path.is_empty()).then(|| path.to_owned())
}

/// The device number of `pid`'s controlling terminal, or `None` if it has none
/// (or is gone). Still readable while the process is a zombie.
#[cfg(target_os = "macos")]
pub fn tty_dev_of(pid: libc::pid_t) -> Option<u32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    // NODEV is -1; 0 means "no controlling terminal".
    if n == size && info.e_tdev != u32::MAX && info.e_tdev != 0 {
        Some(info.e_tdev)
    } else {
        None
    }
}

/// SIGKILL every process whose controlling terminal is `dev`.
///
/// Deliberately excludes this process: Mulpex launched from a terminal has a
/// controlling tty of its own, and while that can never be one of our PTYs, the
/// check costs nothing and makes the blast radius obvious.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn kill_tty_session(dev: u32) {
    let me = std::process::id() as libc::pid_t;
    for pid in all_pids() {
        if pid <= 0 || pid == me {
            continue;
        }
        if tty_dev_of(pid) == Some(dev) {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

/// Hang up, then kill, the process group of every direct child of `parent`
/// that runs in a group of its own — a `claude`'s background commands.
///
/// Claude Code starts each background Bash command (a dev server, a `tail -f`,
/// a Monitor) as its own process group with no controlling terminal (measured:
/// `PGID == pid`, `SESS 0`, parent = the `claude`). So neither `killpg` on the
/// claude's group nor `kill_tty_session` reaches them, and closing a claude
/// left them running forever. Children in the claude's own group (its MCP
/// servers) are left to that `killpg`; Mulpex's own group is never touched.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn kill_child_groups(parent: libc::pid_t) {
    let theirs = unsafe { libc::getpgid(parent) };
    let mine = unsafe { libc::getpgid(0) };
    let mut groups: Vec<libc::pid_t> = all_pids()
        .into_iter()
        .filter(|&pid| pid > 1 && ppid_of(pid) == Some(parent))
        .map(|pid| unsafe { libc::getpgid(pid) })
        .filter(|&g| g > 1 && g != theirs && g != mine)
        .collect();
    groups.sort_unstable();
    groups.dedup();
    for g in &groups {
        unsafe { libc::killpg(*g, libc::SIGHUP) };
    }
    for g in &groups {
        unsafe { libc::killpg(*g, libc::SIGKILL) };
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn kill_child_groups(_parent: libc::pid_t) {}

/// `PROC_ALL_PIDS` from `<sys/proc_info.h>`; the `libc` crate exposes
/// `proc_listpids` but not this constant.
#[cfg(target_os = "macos")]
const PROC_ALL_PIDS: u32 = 1;

#[cfg(target_os = "macos")]
pub fn all_pids() -> Vec<libc::pid_t> {
    let bytes = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Vec::new();
    }
    let slot = std::mem::size_of::<libc::pid_t>() as libc::c_int;
    // Headroom: processes can appear between sizing the buffer and filling it.
    let cap = (bytes / slot) as usize + 64;
    let mut pids = vec![0 as libc::pid_t; cap];
    let bytes = unsafe {
        libc::proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr() as *mut libc::c_void,
            (cap * slot as usize) as libc::c_int,
        )
    };
    if bytes <= 0 {
        return Vec::new();
    }
    pids.truncate((bytes / slot) as usize);
    pids
}

/// `KERN_PROCARGS2` from `<sys/sysctl.h>`. Returns argc, then the executable
/// path, then the argv strings, then the environment — all NUL-separated in one
/// blob. We do not parse it: every string we look for is distinctive enough that
/// a substring search over the whole blob is both sufficient and immune to the
/// layout's padding rules.
#[cfg(target_os = "macos")]
const KERN_PROCARGS2: libc::c_int = 49;

/// A process's command line, as one lossy string with NUL turned into newline.
/// `None` when the process is gone or is not ours to inspect.
///
/// **Argv only, in practice.** `KERN_PROCARGS2` is documented to return the
/// environment after the arguments, and for our own children it does not: a
/// legacy listener's blob came back 756 bytes — exactly its `zsh -c` line, with
/// no `MULPEX_*` in it (measured against a live orphan). So nothing here may be
/// keyed on an environment variable; a legacy listener's argv names
/// `$MULPEX_STATE_DIR` unexpanded and cannot tell you *which* state dir it serves.
/// That is why `reap_orphaned_listeners` keys on the parent instead.
#[cfg(target_os = "macos")]
pub fn argv_of(pid: libc::pid_t) -> Option<String> {
    // The buffer has to be `KERN_ARGMAX`, not whatever a sizing call reports.
    // Asking `KERN_PROCARGS2` for its size with a null buffer answers 32 — enough
    // for `sleep 60` and nothing else (measured) — so a sized fetch silently
    // returns a *truncated* command line. That cost a sweep that matched nothing
    // at all while looking like it worked.
    let mut argmax: libc::c_int = 0;
    let mut argmax_len = std::mem::size_of::<libc::c_int>();
    let mut argmax_mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let rc = unsafe {
        libc::sysctl(
            argmax_mib.as_mut_ptr(),
            argmax_mib.len() as libc::c_uint,
            &mut argmax as *mut _ as *mut libc::c_void,
            &mut argmax_len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || argmax <= 0 {
        return None;
    }

    let mut mib = [libc::CTL_KERN, KERN_PROCARGS2, pid];
    let mut size = argmax as usize;
    let mut buf = vec![0u8; size];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(size);
    // NUL is the separator; turn the blob into one searchable line.
    for b in buf.iter_mut() {
        if *b == 0 {
            *b = b'\n';
        }
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// A process's parent pid, or `None` if it is gone.
#[cfg(target_os = "macos")]
pub fn ppid_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    (n == size).then_some(info.pbi_ppid as libc::pid_t)
}

/// Whether `pid` has lost the parent that started it. On macOS every orphan is
/// reparented to launchd, pid 1.
#[cfg(target_os = "macos")]
pub fn is_orphaned(pid: libc::pid_t) -> bool {
    ppid_of(pid) == Some(1)
}

/// SIGKILL every **orphaned** hub listener on this machine, and report how many.
///
/// This is the one cleanup no other code path can do. A listener runs in its own
/// process group with no controlling terminal (measured: `PGID == pid`, `SESS 0`,
/// state `Ss`), so `Session::kill`'s `killpg` misses it and so does
/// `kill_tty_session`. It outlives its `claude`, outlives app teardown, and is
/// reparented to launchd still spinning `sleep 1` — six were found alive on one
/// machine at once, the oldest more than a day old, belonging to a Mulpex that had
/// exited the previous morning. `listen.rs` makes new listeners exit by
/// themselves; this reaches the ones that predate that and the legacy shell loops
/// that will never learn to.
///
/// **Orphanhood is the parent, not the path.** The obvious test — "does it name a
/// scratch root that is dead?" — cannot be written: a legacy listener's argv
/// spells its state dir as the literal `$MULPEX_STATE_DIR`, and the expanded value
/// lives only in its environment, which `KERN_PROCARGS2` does not hand back for
/// our own children (measured). `ppid == 1` is both available and exactly the
/// signature that was observed. It is also safe by construction: a listener
/// serving a live instance is a child of that `claude`, in this Mulpex or in
/// another one or under `mpx`, and is therefore never matched.
/// Split out from the kill so the selection can be tested without a test run
/// SIGKILLing whatever orphans happen to be on the developer's machine.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn orphaned_listener_pids() -> Vec<libc::pid_t> {
    let me = std::process::id() as libc::pid_t;
    all_pids()
        .into_iter()
        .filter(|&pid| pid != me && pid > 1 && is_orphaned(pid))
        .filter(|&pid| {
            // One matcher, shared with the hook that excuses a listener from the
            // "still working" count — two spellings of "this is a listener" is how
            // one of them silently stops recognising the other.
            argv_of(pid).is_some_and(|argv| mulpex_core::hook::command_is_hub_listener(&argv))
        })
        .collect()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn reap_orphaned_listeners() -> usize {
    let pids = orphaned_listener_pids();
    for pid in &pids {
        unsafe { libc::kill(*pid, libc::SIGKILL) };
    }
    pids.len()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn reap_orphaned_listeners() -> usize {
    0
}

// ---- Linux: the same questions as above, answered from /proc ----
//
// Each `/proc/<pid>/…` file is read whole and handed to a pure parser, so the
// parsing is unit-tested on every platform against fixture strings, and the
// Linux wrappers are only the file reads.

#[cfg(any(target_os = "linux", test))]
/// The fields of `/proc/<pid>/stat` the process sweeps need.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcStat {
    /// The executable name, at most 15 bytes, as the kernel truncates it.
    comm: String,
    ppid: i32,
    pgrp: i32,
    /// Controlling terminal's device number; 0 means none.
    tty_nr: i32,
    /// The tty's FOREGROUND process group; -1 with no controlling terminal.
    tpgid: i32,
}

#[cfg(any(target_os = "linux", test))]
/// Parse one `/proc/<pid>/stat` line: `pid (comm) state ppid pgrp session
/// tty_nr tpgid …`. `comm` may itself hold spaces and parentheses, so it ends
/// at the LAST `)` — never split the whole line on whitespace.
fn parse_proc_stat(line: &str) -> Option<ProcStat> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    let comm = line.get(open + 1..close)?.to_owned();
    let mut rest = line.get(close + 1..)?.split_whitespace();
    let _state = rest.next()?;
    let mut num = || rest.next()?.parse::<i32>().ok();
    let ppid = num()?;
    let pgrp = num()?;
    let _session = num()?;
    let tty_nr = num()?;
    let tpgid = num()?;
    Some(ProcStat { comm, ppid, pgrp, tty_nr, tpgid })
}

#[cfg(any(target_os = "linux", test))]
/// A shell's foreground job, from its stat: the tty's foreground group differs
/// from the shell's own — the same test as `e_tpgid != pbi_pgid` on macOS. No
/// controlling terminal reports -1 (0 on some kernels), which is not a job.
fn foreground_job_running(st: &ProcStat) -> bool {
    st.tpgid > 0 && st.tpgid != st.pgrp
}

#[cfg(any(target_os = "linux", test))]
/// `/proc/<pid>/cmdline` as one searchable string, NUL turned into newline —
/// the same shape `argv_of` returns on macOS.
fn cmdline_text(raw: &[u8]) -> String {
    let joined: Vec<u8> = raw.iter().map(|&b| if b == 0 { b'\n' } else { b }).collect();
    String::from_utf8_lossy(&joined).into_owned()
}

#[cfg(any(target_os = "linux", test))]
/// Whether a process with this parent has lost the one that started it.
///
/// On macOS that is simply "parent is launchd". Linux reparents an orphan to
/// the nearest **child subreaper** instead of pid 1 when one exists — on a
/// desktop that is the user's `systemd --user` manager — so a parent named
/// `systemd` counts too. Never matches a live listener: that one's parent is
/// its `claude`.
fn parent_means_orphaned(ppid: i32, parent_comm: Option<&str>) -> bool {
    ppid == 1 || parent_comm == Some("systemd")
}

#[cfg(target_os = "linux")]
fn proc_stat(pid: libc::pid_t) -> Option<ProcStat> {
    parse_proc_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

#[cfg(target_os = "linux")]
pub fn shell_state_of(pid: libc::pid_t) -> Option<ShellState> {
    let st = proc_stat(pid)?;
    Some(ShellState {
        command_running: foreground_job_running(&st),
        cwd: cwd_of(pid),
    })
}

#[cfg(target_os = "linux")]
pub fn cwd_of(pid: libc::pid_t) -> Option<String> {
    let path = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    let path = path.to_str()?;
    (!path.is_empty()).then(|| path.to_owned())
}

#[cfg(target_os = "linux")]
pub fn tty_dev_of(pid: libc::pid_t) -> Option<u32> {
    let st = proc_stat(pid)?;
    (st.tty_nr != 0).then_some(st.tty_nr as u32)
}

#[cfg(target_os = "linux")]
pub fn all_pids() -> Vec<libc::pid_t> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<libc::pid_t>().ok())
        .collect()
}

#[cfg(target_os = "linux")]
pub fn argv_of(pid: libc::pid_t) -> Option<String> {
    Some(cmdline_text(&std::fs::read(format!("/proc/{pid}/cmdline")).ok()?))
}

#[cfg(target_os = "linux")]
pub fn ppid_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    proc_stat(pid).map(|st| st.ppid)
}

#[cfg(target_os = "linux")]
pub fn is_orphaned(pid: libc::pid_t) -> bool {
    let Some(ppid) = ppid_of(pid) else {
        return false;
    };
    let parent = proc_stat(ppid);
    parent_means_orphaned(ppid, parent.as_ref().map(|p| p.comm.as_str()))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn tty_dev_of(_pid: libc::pid_t) -> Option<u32> {
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn kill_tty_session(_dev: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A claude's background command runs in a process group of its own, which
    /// `killpg` on the claude misses; `kill_child_groups` reaches it, and leaves
    /// a child in the parent's own group alone.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn background_groups_of_a_child_are_killed() {
        use std::io::BufRead as _;
        use std::os::unix::process::CommandExt as _;
        // `sh` stands in for the claude: one child in a group of its own (perl
        // setpgrp, then exec sleep), one in sh's group. Both pids on stdout.
        let mut sh = std::process::Command::new("sh")
            .arg("-c")
            .arg("perl -e 'setpgrp(0,0); exec q(sleep), q(9999)' & echo $!; sleep 9999 & echo $!; wait")
            .process_group(0)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = std::io::BufReader::new(sh.stdout.take().unwrap()).lines();
        let own: libc::pid_t = lines.next().unwrap().unwrap().trim().parse().unwrap();
        let same: libc::pid_t = lines.next().unwrap().unwrap().trim().parse().unwrap();
        let alive = |pid: libc::pid_t| ppid_of(pid) == Some(sh.id() as libc::pid_t);
        let settled = |f: &dyn Fn() -> bool| {
            (0..100).any(|_| {
                std::thread::sleep(std::time::Duration::from_millis(20));
                f()
            })
        };
        assert!(
            settled(&|| unsafe { libc::getpgid(own) } == own),
            "the background child never got a group of its own"
        );

        kill_child_groups(sh.id() as libc::pid_t);

        assert!(settled(&|| !alive(own)), "the background group survived");
        assert!(alive(same), "a child in the parent's own group was killed");

        unsafe { libc::killpg(sh.id() as libc::pid_t, libc::SIGKILL) };
        let _ = sh.wait();
    }

    /// `/proc/<pid>/stat` as Linux writes it, including a `comm` that holds a
    /// space and a `)` — the reason the parser anchors on the LAST `)`.
    #[test]
    fn proc_stat_lines_parse() {
        let shell = "4242 (bash) S 4100 4242 4242 34817 4300 4194560 1 0 0 0 2 1 0 0 20 0 1 0 9 1 2 3";
        let st = parse_proc_stat(shell).unwrap();
        assert_eq!(st, ProcStat { comm: "bash".into(), ppid: 4100, pgrp: 4242, tty_nr: 34817, tpgid: 4300 });
        assert!(foreground_job_running(&st), "the tty's foreground group is a job, not the shell");

        let idle = "4242 (zsh) S 4100 4242 4242 34817 4242 0";
        assert!(!foreground_job_running(&parse_proc_stat(idle).unwrap()));

        // A daemon: no controlling terminal, tpgid -1.
        let daemon = "77 (we ird) proc)) S 1 77 77 0 -1 4194304 0";
        let st = parse_proc_stat(daemon).unwrap();
        assert_eq!(st.comm, "we ird) proc)");
        assert_eq!((st.ppid, st.tty_nr, st.tpgid), (1, 0, -1));
        assert!(!foreground_job_running(&st));

        assert!(parse_proc_stat("").is_none());
        assert!(parse_proc_stat("12 (cut").is_none());
        assert!(parse_proc_stat("12 (short) S 1").is_none());
    }

    #[test]
    fn cmdline_reads_as_one_line_per_argument() {
        let raw = b"/opt/Mulpex/mulpex-helper\0listen\0";
        assert_eq!(cmdline_text(raw), "/opt/Mulpex/mulpex-helper\nlisten\n");
        assert!(mulpex_core::hook::command_is_hub_listener(&cmdline_text(raw)));
    }

    /// Linux hands an orphan to a subreaper (`systemd --user`) when there is
    /// one, so "parent is pid 1" alone would never reap on a desktop — but a
    /// listener whose `claude` is alive must never qualify.
    #[test]
    fn an_orphan_is_parented_by_init_or_a_systemd_subreaper() {
        assert!(parent_means_orphaned(1, Some("systemd")));
        assert!(parent_means_orphaned(1, None));
        assert!(parent_means_orphaned(1834, Some("systemd")));
        assert!(!parent_means_orphaned(5120, Some("claude")));
        assert!(!parent_means_orphaned(5120, Some("node")));
        assert!(!parent_means_orphaned(5120, None));
    }
    /// The selection behind the one cleanup nothing else in this file can do —
    /// and the one that has to be *narrow*, because it reaches into the process
    /// table and SIGKILLs, with no undo.
    ///
    /// Both halves are load-bearing and each is the whole test on its own:
    /// **orphaned** without **listener** is every stray daemon on the machine;
    /// **listener** without **orphaned** is every listener still serving a live
    /// instance — in this Mulpex, in a second one, or under `mpx`.
    ///
    /// The obvious third condition, "names a dead scratch root", is deliberately
    /// absent and cannot be added: a legacy listener's argv spells its state dir
    /// as the literal `$MULPEX_STATE_DIR`, and `KERN_PROCARGS2` does not return
    /// the environment that would expand it (measured — see `argv_of`).
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn only_a_parentless_listener_is_reaped() {
        const MARK: &str = "\"/x/mulpex-helper\" listen";
        let pidfile = std::env::temp_dir().join(format!(
            "mulpex-orphan-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        // A genuine orphan: the shell we spawn backgrounds a listener-shaped child
        // and exits, so that child is reparented to launchd — exactly how a real
        // one is made, by its `claude` dying underneath it.
        let mut maker = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "/bin/sh -c 'echo $$ > {pf}; while :; do sleep 1; done # {MARK}' & exit 0",
                pf = pidfile.display()
            ))
            .spawn()
            .unwrap();
        let _ = maker.wait();

        // A listener whose parent is alive (us). Reaping this is the failure that
        // would deafen a running instance.
        let mut attached = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("while :; do sleep 1; done # {MARK}"))
            .spawn()
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(400));
        let orphan: libc::pid_t = std::fs::read_to_string(&pidfile)
            .expect("the orphan should have written its pid")
            .trim()
            .parse()
            .unwrap();
        assert!(is_orphaned(orphan), "the test's orphan must be parentless");

        let reapable = orphaned_listener_pids();
        assert!(reapable.contains(&orphan), "a parentless listener is reapable");
        assert!(
            !reapable.contains(&(attached.id() as libc::pid_t)),
            "a listener whose instance is still alive must never be reaped"
        );

        unsafe { libc::kill(orphan, libc::SIGKILL) };
        let _ = attached.kill();
        let _ = attached.wait();
        let _ = std::fs::remove_file(&pidfile);
    }
}
