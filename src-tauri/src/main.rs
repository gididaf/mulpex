// Prevents an extra console window on Windows in release (harmless on macOS).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `release.sh` asks the built binary for its wire fingerprint, so the
    // value in latest.json is the one that actually shipped.
    if std::env::args().nth(1).as_deref() == Some("--wire-hash") {
        println!("{}", env!("MULPEX_WIRE"));
        return;
    }
    mulpex_lib::run()
}
