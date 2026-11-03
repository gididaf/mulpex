// Prevents an extra console window on Windows in release (harmless on macOS).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // A CLI's two questions answer and exit. Before these existed, a claude
    // running `Mulpex.AppImage --version` started a whole second app
    // (src-tauri/src/instance.rs has what that did).
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => {
            println!("Mulpex {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--help" | "-h") => {
            println!(
                "Mulpex {} — many coordinated Claude Code sessions in one window.\n\n\
                 Usage: mulpex            open Mulpex (or bring the running one to the front)\n\
                 \x20      mulpex --version  print the version and exit\n\
                 \x20      mulpex --help     print this and exit\n\n\
                 MULPEX_HOME picks the data directory (default ~/.mulpex); one Mulpex runs per home.",
                env!("CARGO_PKG_VERSION")
            );
            return;
        }
        _ => {}
    }
    // `release.sh` asks the built binary for its wire fingerprint, so the
    // value in latest.json is the one that actually shipped.
    if std::env::args().nth(1).as_deref() == Some("--wire-hash") {
        println!("{}", env!("MULPEX_WIRE"));
        return;
    }
    // ...and for the PTY-daemon protocols it drives, `min max`: latest.json
    // carries them so a running Mulpex knows, before installing, whether its
    // claudes survive the update (docs/ptyd.md).
    if std::env::args().nth(1).as_deref() == Some("--ptyd-proto") {
        println!("{} {}", mulpex_ptyd::proto::PROTO_MIN, mulpex_ptyd::proto::PROTO);
        return;
    }
    mulpex_lib::run()
}
