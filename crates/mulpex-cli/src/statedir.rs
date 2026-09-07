//! The per-project scratch dir a `claude` child is pointed at.
//!
//! The tree itself is written by `mulpex_core::state_dir`, shared with the
//! desktop app — both frontends point the *same* hook and MCP binaries at these
//! paths, so there is one implementation. What lives here is only what is
//! specific to the CLI: where its home is, and how a project directory maps to a
//! state dir.

use std::path::{Path, PathBuf};

/// Mulpex CLI's home, deliberately **separate from the desktop app's**.
///
/// `persist::SessionStore` is keyed by project directory, so if `mpx` and the app
/// shared `~/.mulpex` and both opened the same project they would restore the
/// same `--resume` uuids — two claudes appending to one transcript, which
/// `restart_instance` calls "not a state `--resume` can be asked to make sense of
/// afterwards". That is silent conversation corruption, so the CLI gets its own
/// home. The cost, which belongs in `docs/cli.md`: separate recents, open set and
/// session stores. On a server there is no desktop app, so nothing is lost where
/// it matters. `MULPEX_HOME` still overrides, for anyone who wants the collision
/// and understands it.
pub fn cli_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("MULPEX_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(if cfg!(debug_assertions) {
        ".mulpex-cli-dev"
    } else {
        ".mulpex-cli"
    })
}

/// The root every project's state dir hangs under.
///
/// **Not `$TMPDIR/mulpex-<pid>`, which is what the desktop app uses.** A live
/// claude holds `MULPEX_STATE_DIR` in its exec-time environment and `mpx`
/// sessions are meant to outlive the process that started them, so a pid-keyed
/// root would orphan every instance's hub the moment the daemon restarted — with
/// nothing anywhere saying so. `$TMPDIR` is also purged after three days, and
/// `$XDG_RUNTIME_DIR` is cleared at logout, which a detached tmux session is
/// specifically designed to survive.
pub fn state_root() -> PathBuf {
    cli_home().join("run")
}

/// A project's state dir: stable across `mpx` invocations, so re-running `mpx` in
/// the same directory rejoins the same hub.
///
/// Phase 3 replaces this hash with the monotonic handles the daemon allocates and
/// persists; a hash is enough while there is no daemon and no `registry.json`.
pub fn state_dir_for(project_dir: &Path) -> PathBuf {
    state_root().join(format!("p{:016x}", fnv1a(project_dir.as_os_str().as_encoded_bytes())))
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_dir_maps_to_a_stable_state_dir() {
        let a = state_dir_for(Path::new("/Users/x/code/cloud"));
        let b = state_dir_for(Path::new("/Users/x/code/cloud"));
        let c = state_dir_for(Path::new("/Users/x/code/other"));
        assert_eq!(a, b, "same project must rejoin the same hub");
        assert_ne!(a, c);
        assert!(a.starts_with(state_root()));
    }

    /// The CLI must not share `~/.mulpex` with the desktop app: `SessionStore` is
    /// keyed by project dir, so both opening one project would restore the same
    /// `--resume` uuids — two claudes appending to one transcript.
    #[test]
    fn the_cli_home_is_not_the_desktop_apps() {
        std::env::remove_var("MULPEX_HOME");
        let home = cli_home();
        let name = home.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with(".mulpex-cli"), "got {name}");
    }
}
