//! The per-project scratch dir a `claude` child is pointed at: the two config
//! files it is launched with, and the directory tree the hook and MCP server
//! expect to find.
//!
//! Shared by both frontends. The desktop app and `mpx` must agree on this tree
//! exactly — the hook binary and the MCP server are the *same* processes in both
//! cases, and they look for these paths by name.
//!
//! Everything here is rewritten before **every** spawn, not once per project. See
//! the note on the three-day fuse below.

use std::path::Path;

use crate::config::{HOOK_SETTINGS_JSON, MCP_CONFIG_JSON};

/// Lay out (or repair) a project's scratch dir: the `--settings` / `--mcp-config`
/// files every `claude` is spawned with, plus the subdirectories the hub writes
/// into.
///
/// Idempotent, and called before **every** spawn rather than only at open,
/// because the scratch root lives in `$TMPDIR` and macOS deletes anything there
/// it has not seen touched in 3 days (`com.apple.bsd.dirhelper`,
/// `CLEAN_FILES_OLDER_THAN_DAYS=3`, daily at 03:35). `settings.json` and
/// `mcp.json` were the only write-once files in the tree — everything else is
/// rewritten by the 200 ms poll or re-created by the hook binary — so in a
/// Mulpex left open past three days they, and only they, were purged. Running
/// instances kept working (both files are read once, at spawn); every new ⌘T
/// died instantly with `Error: Settings file not found: …`. Reported from a
/// v0.8 session that had been open since Aug 23.
pub fn write_state_dir(state_dir: &Path, helper_path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let helper = helper_path.to_string_lossy();
    std::fs::write(
        state_dir.join("settings.json"),
        HOOK_SETTINGS_JSON.replace("__MULPEX_BIN__", &helper),
    )?;
    std::fs::write(
        state_dir.join("mcp.json"),
        MCP_CONFIG_JSON.replace("__MULPEX_BIN__", &helper),
    )?;
    // Every name here contains no bare integer at the top level, which is what
    // keeps `mcp::live_ids`' integer-filename scan from mistaking one for an
    // instance status file.
    for sub in [
        "locks",
        "history",
        "tasks",
        "inbox",
        "waiting",
        "bg",
        "compacting",
        "spawn",
        "armed",
        crate::NAMED_DIR,
        crate::NAMEREQ_DIR,
        crate::RESUMED_DIR,
        crate::USERPROMPT_DIR,
        crate::RELISTEN_DIR,
        crate::PIDS_DIR,
        crate::LISTENERS_DIR,
        crate::EXPLAINREQ_DIR,
        crate::WATCHING_DIR,
        "terminals",
        "terminals/cursors",
        "termreq",
    ] {
        std::fs::create_dir_all(state_dir.join(sub))?;
    }
    Ok(())
}

/// The list `mcp::live_ids` reads to learn which instances exist.
///
/// Without it the MCP server falls back to scanning the state dir for
/// integer-named files, which reports whatever happens to be lying around rather
/// than what is actually running — which is why no subdirectory above may be
/// named with a bare integer.
pub fn write_live_instances(state_dir: &Path, ids: &[usize]) -> std::io::Result<()> {
    let body: String = ids.iter().map(|i| format!("{i}\n")).collect();
    std::fs::write(state_dir.join("instances"), body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `mcp::live_ids` treats an integer filename as an instance id when
    /// `instances` is missing, so a subdirectory called `3` would be read as
    /// instance 3.
    #[test]
    fn no_subdir_name_is_a_bare_integer() {
        let dir = std::env::temp_dir().join(format!("mpxsd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_state_dir(&dir, Path::new("/bin/true")).expect("write");
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            assert!(name.parse::<usize>().is_err(), "{name} reads as an instance id");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The helper path is substituted in both files, or `claude` execs the
    /// literal string `__MULPEX_BIN__` and every hook fails silently.
    #[test]
    fn the_helper_path_is_substituted_in_both_config_files() {
        let dir = std::env::temp_dir().join(format!("mpxsd2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_state_dir(&dir, Path::new("/opt/mulpex-helper")).expect("write");
        for f in ["settings.json", "mcp.json"] {
            let text = std::fs::read_to_string(dir.join(f)).unwrap();
            assert!(!text.contains("__MULPEX_BIN__"), "{f} still has the placeholder");
            assert!(text.contains("/opt/mulpex-helper"), "{f} lost the helper path");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_instances_is_one_id_per_line() {
        let dir = std::env::temp_dir().join(format!("mpxsd3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_live_instances(&dir, &[1, 2, 5]).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("instances")).unwrap(), "1\n2\n5\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
