//! Translate crossterm key events into the byte sequences a terminal program
//! (here, Claude Code) expects on its stdin.
//!
//! This is what makes the embedded session behave like a real `claude`: every
//! key the focused pane receives is encoded the way a normal terminal would.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Encode a key event as bytes to forward to the PTY, or `None` if the key has
/// no meaningful encoding.
pub fn key_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    let m = key.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);

    let mut out: Vec<u8> = Vec::new();
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let b = ctrl_byte(c)?;
                if alt {
                    out.push(0x1b);
                }
                out.push(b);
            } else {
                if alt {
                    out.push(0x1b);
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Esc => out.push(0x1b),

        KeyCode::Left => out.extend_from_slice(&csi_letter(b'D', m)),
        KeyCode::Right => out.extend_from_slice(&csi_letter(b'C', m)),
        KeyCode::Up => out.extend_from_slice(&csi_letter(b'A', m)),
        KeyCode::Down => out.extend_from_slice(&csi_letter(b'B', m)),
        KeyCode::Home => out.extend_from_slice(&csi_letter(b'H', m)),
        KeyCode::End => out.extend_from_slice(&csi_letter(b'F', m)),

        KeyCode::Insert => out.extend_from_slice(&csi_tilde(2, m)),
        KeyCode::Delete => out.extend_from_slice(&csi_tilde(3, m)),
        KeyCode::PageUp => out.extend_from_slice(&csi_tilde(5, m)),
        KeyCode::PageDown => out.extend_from_slice(&csi_tilde(6, m)),

        KeyCode::F(n) => out.extend_from_slice(&function_key(n)?),

        _ => return None,
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// xterm modifier parameter: 1 + bitmask(shift=1, alt=2, ctrl=4).
fn mod_code(m: KeyModifiers) -> u8 {
    1 + (m.contains(KeyModifiers::SHIFT) as u8)
        + ((m.contains(KeyModifiers::ALT) as u8) << 1)
        + ((m.contains(KeyModifiers::CONTROL) as u8) << 2)
}

/// `CSI <final>` (e.g. arrows, Home/End), with `CSI 1 ; <mod> <final>` when
/// modifiers are held.
fn csi_letter(final_byte: u8, m: KeyModifiers) -> Vec<u8> {
    let code = mod_code(m);
    if code == 1 {
        vec![0x1b, b'[', final_byte]
    } else {
        format!("\x1b[1;{}{}", code, final_byte as char).into_bytes()
    }
}

/// `CSI <num> ~` (e.g. PageUp/Delete), with `CSI <num> ; <mod> ~` when held.
fn csi_tilde(num: u8, m: KeyModifiers) -> Vec<u8> {
    let code = mod_code(m);
    if code == 1 {
        format!("\x1b[{}~", num).into_bytes()
    } else {
        format!("\x1b[{};{}~", num, code).into_bytes()
    }
}

fn function_key(n: u8) -> Option<Vec<u8>> {
    let seq: &[u8] = match n {
        1 => b"\x1bOP",
        2 => b"\x1bOQ",
        3 => b"\x1bOR",
        4 => b"\x1bOS",
        5 => b"\x1b[15~",
        6 => b"\x1b[17~",
        7 => b"\x1b[18~",
        8 => b"\x1b[19~",
        9 => b"\x1b[20~",
        10 => b"\x1b[21~",
        11 => b"\x1b[23~",
        12 => b"\x1b[24~",
        _ => return None,
    };
    Some(seq.to_vec())
}

/// Control-byte for a Ctrl+<char> combination.
fn ctrl_byte(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    match c {
        'a'..='z' => Some((c as u8) - b'a' + 1),
        ' ' | '@' => Some(0),
        '[' => Some(27),
        '\\' => Some(28),
        ']' => Some(29),
        '^' => Some(30),
        '_' => Some(31),
        '?' => Some(127),
        _ => None,
    }
}
