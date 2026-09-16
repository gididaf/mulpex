//! The headless coordination helper that each child `claude` process invokes.
//!
//! The desktop app writes a `--settings` file whose hooks call
//! `"<helper>" hook <event>` and an `--mcp-config` whose server is
//! `"<helper>" mcp`. `claude` (not Tauri) spawns those by absolute path, so this
//! binary is the stable, GUI-free entry point for both roles. It links only
//! `mulpex-core`, keeping it tiny and fast to exec — a `PreToolUse` hook forks on
//! every Read/Write/Edit/Bash and the MCP server is long-lived per instance.
//!
//! Identity (`MULPEX_INSTANCE_ID` / `MULPEX_STATE_DIR` / `MULPEX_PROJECT_DIR`)
//! arrives through the environment `claude` inherited from the app, so no args
//! beyond the subcommand are needed.

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("hook") => mulpex_core::hook::run(&args[2..]),
        Some("mcp") => mulpex_core::mcp::run(&args[2..]),
        // Long-lived, one per instance, started by the instance itself through
        // the `Monitor` tool — see `listen.rs` for why the loop is here rather
        // than in the prompt that asks for it.
        Some("listen") => mulpex_core::listen::run(&args[2..]),
        other => {
            eprintln!(
                "mulpex-helper: expected `hook <event>`, `mcp` or `listen`, got {:?}",
                other
            );
            std::process::exit(2);
        }
    }
}
