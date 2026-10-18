#[path = "src/wire.rs"]
#[allow(dead_code)]
mod wire;

use sha2::{Digest, Sha256};

fn main() {
    // The wire fingerprint (src/wire.rs): Remote Hosts connects on it, not on
    // the app version.
    let mut h = Sha256::new();
    for f in wire::FILES {
        println!("cargo:rerun-if-changed={f}");
        let src = std::fs::read_to_string(f).unwrap_or_else(|e| panic!("wire file {f}: {e}"));
        h.update(f.as_bytes());
        h.update(b"\0");
        h.update(wire::normalize(&src).as_bytes());
    }
    let hex: String = h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect();
    println!("cargo:rustc-env=MULPEX_WIRE={hex}");
    tauri_build::build()
}
