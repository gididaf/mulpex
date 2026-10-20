#[path = "src/wire.rs"]
#[allow(dead_code)]
mod wire;
#[path = "src/spawnprint.rs"]
#[allow(dead_code)]
mod spawnprint;

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

    // The spawn fingerprint (src/spawnprint.rs): what a running claude froze
    // at spawn. Same normalization, so a doc-only edit does not mark claudes.
    let mut h = Sha256::new();
    for f in spawnprint::FILES {
        println!("cargo:rerun-if-changed={f}");
        let src = std::fs::read_to_string(f).unwrap_or_else(|e| panic!("spawnprint file {f}: {e}"));
        h.update(f.as_bytes());
        h.update(b"\0");
        h.update(wire::normalize(&src).as_bytes());
    }
    let hex: String = h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect();
    println!("cargo:rustc-env=MULPEX_SPAWNPRINT={hex}");
    tauri_build::build()
}
