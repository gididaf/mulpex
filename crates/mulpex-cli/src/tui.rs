//! The little bit of terminal handling `mpx` cannot avoid owning.
//!
//! tmux is the emulator, so there is no VT parsing, no key encoding and no
//! scrollback here — and there never should be. What is left is what any program
//! drawing into a pane needs: put the tty in raw mode and put it back, and measure
//! text in **display columns** rather than bytes.
//!
//! Shared by the two things `mpx` draws itself: the sidebar (`sidebar.rs`) and the
//! project picker (`picker.rs`). They had the same helpers twice for about an
//! hour, which is how a pad that counts bytes ends up in one of them.

use std::io::{Read, Write};

/// The terminal put into raw mode, and put back on the way out.
///
/// Restoring matters even though the pane is ours: a program that exits leaves the
/// pane at a shell, and a shell inheriting a raw tty shows no echo and no line
/// editing — which looks like a hung terminal rather than a crashed program.
pub struct RawMode {
    saved: Option<libc::termios>,
    pending: Vec<u8>,
}

impl RawMode {
    pub fn enable() -> Self {
        // SAFETY: `tcgetattr`/`tcsetattr` on fd 0, with a fully initialised struct.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return RawMode { saved: None, pending: Vec::new() };
            }
            let saved = t;
            libc::cfmakeraw(&mut t);
            // Read returns after the timeout with nothing, which is what lets the
            // same loop both wait for a key and redraw on a clock.
            t.c_cc[libc::VMIN] = 0;
            t.c_cc[libc::VTIME] = 2; // deciseconds
            libc::tcsetattr(0, libc::TCSANOW, &t);
            RawMode { saved: Some(saved), pending: Vec::new() }
        }
    }

    /// Whatever was typed since the last call, raw. Empty when the read timed out,
    /// which is what lets one loop both wait for a key and redraw on a clock.
    ///
    /// Raw rather than parsed because both callers need the **text**: a name and a
    /// path are arbitrary UTF-8, and cannot be reassembled from a key enum.
    pub fn read(&mut self) -> Vec<u8> {
        let mut buf = [0u8; 256];
        let n = std::io::stdin().read(&mut buf).unwrap_or(0);
        self.pending.extend_from_slice(&buf[..n]);
        std::mem::take(&mut self.pending)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(saved) = self.saved {
            // SAFETY: restoring the exact struct `tcgetattr` produced.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
        }
        let _ = write!(std::io::stdout(), "\x1b[?25h");
    }
}

/// The pane's size, or a usable guess.
pub fn size(fallback: (u16, u16)) -> (u16, u16) {
    #[repr(C)]
    struct WinSize {
        rows: u16,
        cols: u16,
        x: u16,
        y: u16,
    }
    let mut ws = WinSize { rows: 0, cols: 0, x: 0, y: 0 };
    // SAFETY: the kernel fully writes `ws` on success, and it is only read then.
    let rc = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws as *mut WinSize) };
    if rc == 0 && ws.cols > 0 && ws.rows > 0 {
        (ws.cols, ws.rows)
    } else {
        fallback
    }
}

/// Pad to `w` **display** columns, counting characters rather than bytes — the
/// text here is arbitrary, and byte length would misalign every row after the
/// first non-ASCII one.
pub fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        return trunc(s, w);
    }
    format!("{s}{}", " ".repeat(w - n))
}

pub fn trunc(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Keep the **end** of a string that does not fit. The opposite of `trunc`, and
/// right for a field you are typing into: the cursor is at the end, so that is the
/// part you need to see.
pub fn tail(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w || w == 0 {
        return s.to_string();
    }
    let mut out = String::from("…");
    out.extend(s.chars().skip(n - w.saturating_sub(1)));
    out
}

/// Break a line at `w` display columns, on spaces where there is one.
pub fn wrap(s: &str, w: usize) -> Vec<String> {
    if w == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split(' ') {
        let n = line.chars().count();
        if !line.is_empty() && n + 1 + word.chars().count() > w {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        // A single word longer than the pane still has to fit somewhere.
        if word.chars().count() > w {
            out.push(trunc(word, w));
        } else {
            line.push_str(word);
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every line must be exactly the pane width, or the rows stagger — and the
    /// text is arbitrary, so this has to count characters and not bytes.
    #[test]
    fn padding_counts_characters_not_bytes() {
        assert_eq!(pad("abc", 6), "abc   ");
        assert_eq!(pad("שלום", 6).chars().count(), 6, "Hebrew is 2 bytes per char");
        assert_eq!(pad("▸claude", 4).chars().count(), 4, "over-long is truncated, not wrapped");
        assert_eq!(trunc("abcdef", 4), "abc…");
        assert_eq!(trunc("ab", 4), "ab");
        assert_eq!(trunc("abc", 0), "");
    }

    /// A field you are typing into must show the END of what you typed — the
    /// opposite of every other truncation here, where the start identifies the row.
    #[test]
    fn an_overlong_typed_value_shows_its_end() {
        assert_eq!(tail("abcdefgh", 4), "…fgh");
        assert_eq!(trunc("abcdefgh", 4), "abc…");
        assert_eq!(tail("ab", 4), "ab");
        assert_eq!(tail("abc", 0), "abc");
        assert_eq!(tail("שלום עולם", 5).chars().count(), 5);
    }

    /// An error is arbitrary length and its useful half is the end, so text wraps
    /// rather than being cut — and still fits the pane exactly.
    #[test]
    fn a_long_message_wraps_instead_of_being_cut() {
        let msg = "✗ the mulpex daemon did not answer in 5s";
        let lines = wrap(msg, 26);
        assert!(lines.len() > 1, "expected a wrap: {lines:?}");
        assert!(lines.iter().all(|l| l.chars().count() <= 26), "{lines:?}");
        assert!(lines.last().unwrap().contains("5s"), "the end survives");
        // A single unbreakable word longer than the pane still has to land.
        assert_eq!(wrap("/very/long/path/with/no/spaces/at/all", 10), vec!["/very/lon…"]);
    }
}
