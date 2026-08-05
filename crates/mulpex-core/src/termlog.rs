//! The on-disk format of a shell terminal's plain-text transcript.
//!
//! Two processes touch these files and neither owns the other: Mulpex writes
//! them (`src-tauri/src/vtgrid.rs`) and the MCP helper, running inside a
//! `claude`, reads them for `hub_terminal_read`. The header layout therefore
//! lives here, in the crate they share, rather than as a magic number repeated
//! on each side.
//!
//! `terminals/<id>.log` is a fixed-width header followed by plain text:
//!
//! ```text
//! MPXT1 <base:020> <last_out_ms:020> <state>\n
//! ```
//!
//! - `base` — logical offset of the first data byte. The file is trimmed from
//!   the front once it grows past its cap, so a reader's saved position has to
//!   be a *logical* offset that survives that; `base` is what maps it back.
//! - `last_out_ms` — wall-clock ms of the last output, for `idle_ms`. Kept in
//!   the header rather than taken from mtime, which a trim would reset.
//! - `state` — `0` running, `1` the shell has exited.
//!
//! The writer rewrites the header **in place, last** after a trim, so a reader
//! that sees `base` change across its own read knows the data moved under it and
//! can simply retry.

/// Byte width of the header. Data begins at this offset.
pub const HEADER_LEN: usize = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub base: u64,
    pub last_out_ms: u64,
    pub exited: bool,
}

/// Render a header. Always exactly `HEADER_LEN` bytes.
pub fn format_header(h: &Header) -> String {
    let s = format!(
        "MPXT1 {:020} {:020} {}\n",
        h.base,
        h.last_out_ms,
        u8::from(h.exited)
    );
    debug_assert_eq!(s.len(), HEADER_LEN);
    s
}

/// Parse a header from the first bytes of a log file. `None` if it is too short
/// or doesn't carry our magic (a truncated or foreign file).
pub fn parse_header(bytes: &[u8]) -> Option<Header> {
    if bytes.len() < HEADER_LEN || !bytes.starts_with(b"MPXT1 ") {
        return None;
    }
    let text = std::str::from_utf8(&bytes[..HEADER_LEN]).ok()?;
    let base = text.get(6..26)?.trim().parse().ok()?;
    let last_out_ms = text.get(27..47)?.trim().parse().ok()?;
    let exited = text.as_bytes().get(48)? == &b'1';
    Some(Header {
        base,
        last_out_ms,
        exited,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        for h in [
            Header {
                base: 0,
                last_out_ms: 0,
                exited: false,
            },
            Header {
                base: 1_234_567,
                last_out_ms: 1_764_000_000_000,
                exited: true,
            },
            Header {
                base: u64::MAX,
                last_out_ms: u64::MAX,
                exited: false,
            },
        ] {
            let s = format_header(&h);
            assert_eq!(s.len(), HEADER_LEN);
            assert_eq!(parse_header(s.as_bytes()), Some(h));
        }
    }

    #[test]
    fn a_short_or_foreign_file_is_rejected() {
        assert_eq!(parse_header(b""), None);
        assert_eq!(parse_header(b"MPXT1 "), None);
        assert_eq!(parse_header(&[b'x'; HEADER_LEN]), None);
    }

    #[test]
    fn trailing_data_is_ignored() {
        let h = Header {
            base: 42,
            last_out_ms: 7,
            exited: false,
        };
        let mut raw = format_header(&h).into_bytes();
        raw.extend_from_slice(b"hello\nworld\n");
        assert_eq!(parse_header(&raw), Some(h));
    }
}
