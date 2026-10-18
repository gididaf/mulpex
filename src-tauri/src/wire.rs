//! The wire fingerprint: which Mulpex builds can drive each other (Remote Hosts).
//!
//! `build.rs` hashes the code that defines what a remote desktop and its host
//! say to each other, and bakes it in as `MULPEX_WIRE`. Two builds with the
//! same fingerprint connect whatever their app versions are, so a release that
//! never touched these files never forces a host update (docs/remote-hosts.md).
//!
//! This file is shared with `build.rs` (`#[path]`), so it must stay plain std.

/// The files that make up the link. A change to any of them is a new wire.
pub const FILES: &[&str] = &[
    "src/remote/rpc.rs",
    "src/remote/desktop.rs",
    "src/remote/client.rs",
    "src/snapshot.rs",
    "src/remote_update.rs",
    "src/commands.rs",
];

/// One file as the fingerprint sees it: lines trimmed, blank lines and whole
/// comment lines (`//`, `///`, `//!`) dropped, so a doc-only edit is not a
/// new wire.
pub fn normalize(src: &str) -> String {
    let mut out = String::new();
    for line in src.lines().map(str::trim) {
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_blank_lines_do_not_count() {
        let a = "fn a() {\n    x();\n}\n";
        let b = "/// Doc.\nfn a() {\n\n        x();\n    // why\n}\n";
        assert_eq!(normalize(a), normalize(b));
    }

    #[test]
    fn code_counts() {
        assert_ne!(normalize("fn a(x: u8) {}\n"), normalize("fn a(x: u16) {}\n"));
    }

    #[test]
    fn baked_in() {
        let w = env!("MULPEX_WIRE");
        assert_eq!(w.len(), 16);
        assert!(w.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
