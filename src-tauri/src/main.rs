// Prevents an extra console window on Windows in release (harmless on macOS).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
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
