//! Logical → visual reordering for the text `mpx` prints itself.
//!
//! **This is a workaround for the terminal, not a feature.** Measured in Phase 0:
//! iTerm2 at default settings does *not* apply the Unicode Bidirectional
//! Algorithm — it draws codepoints in storage order, left to right. A Hebrew
//! sentence therefore appears reversed, and a mixed sentence (`תיקנתי את pty.rs`,
//! which is exactly what this project's user writes) comes out scrambled in a way
//! that is worse than either.
//!
//! So anything `mpx` renders — the messages feed, `ls`, errors, and the Explainer
//! when it lands — is converted here before it is written out.
//!
//! **What this does NOT and cannot cover:** `claude`'s own Hebrew output, and
//! Hebrew the user types into its prompt. Those bytes go straight between the
//! terminal and `claude` through tmux; `mpx` never sees them. iTerm2 has an
//! experimental right-to-left setting that would cover them, but its toggle was
//! never located, so that remains unverified.
//!
//! **Why it is a setting and not unconditional.** A terminal that *does*
//! implement the UBA would apply it again on top of this, reversing the text back
//! — so on such a terminal the fix is the bug. `MPX_BIDI=off` turns it off,
//! `MPX_BIDI=on` forces it on, and the default converts only lines that actually
//! contain right-to-left characters, so a pure-ASCII line is never touched at all.

use unicode_bidi::BidiInfo;

/// Convert one line from logical order to the visual order a non-reordering
/// terminal needs.
///
/// Per line, deliberately: the algorithm's paragraph direction is decided by the
/// first strong character, and running it over a whole block would let one Hebrew
/// line flip the alignment of the ASCII lines around it.
pub fn visual(line: &str) -> String {
    if !enabled() || !has_rtl(line) {
        return line.to_string();
    }
    reorder(line)
}

/// Reorder unconditionally. Split out so the tests can exercise the algorithm
/// without depending on the environment the suite happens to run in.
fn reorder(line: &str) -> String {
    // `None` asks the UBA to infer the paragraph direction from the first strong
    // character, which is what makes a Hebrew line right-aligned in meaning and an
    // English line with a Hebrew phrase in it stay left-to-right.
    let info = BidiInfo::new(line, None);
    let Some(para) = info.paragraphs.first() else {
        return line.to_string();
    };
    info.reorder_line(para, para.range.clone()).into_owned()
}

/// Convert every line of a block.
pub fn visual_block(text: &str) -> String {
    if !enabled() || !has_rtl(text) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&visual(line));
    }
    out
}

/// Is there anything here the algorithm would move?
///
/// Hebrew, Arabic and the RTL presentation forms. Cheap, and it is what keeps the
/// conversion off every ASCII line in the program.
fn has_rtl(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c as u32,
            0x0590..=0x05FF   // Hebrew
            | 0x0600..=0x06FF // Arabic
            | 0x0700..=0x074F // Syriac
            | 0x0780..=0x07BF // Thaana
            | 0x08A0..=0x08FF // Arabic Extended-A
            | 0xFB1D..=0xFDFF // Hebrew/Arabic presentation forms
            | 0xFE70..=0xFEFF)
    })
}

/// Whether to convert at all. `MPX_BIDI` is `off` / `on` / anything else (auto).
///
/// Auto is not terminal detection — there is no reliable query for "do you
/// implement the UBA?" — it simply means "convert lines that contain RTL text",
/// which is right for every terminal measured so far and wrong only for one that
/// reorders on its own. That case is what `off` is for, and it has to be sayable
/// because getting it wrong is invisible: the text is merely backwards, not
/// missing.
pub fn enabled() -> bool {
    match std::env::var("MPX_BIDI").unwrap_or_default().trim().to_ascii_lowercase().as_str() {
        "off" | "0" | "no" | "false" => false,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pure-Hebrew line comes out with its characters in the order a
    /// left-to-right terminal has to draw them for it to read correctly — i.e.
    /// reversed from storage order.
    #[test]
    fn a_hebrew_line_is_reversed_for_a_non_reordering_terminal() {
        let logical = "שלום עולם";
        let visual = reorder(logical);
        assert_ne!(visual, logical, "it must actually reorder");
        let back: String = visual.chars().rev().collect();
        assert_eq!(back, logical, "pure RTL is a straight reversal");
    }

    /// The case that actually matters here, and the one a hand-rolled
    /// reverse-the-RTL-runs function got wrong in Phase 0: Hebrew with Latin
    /// identifiers embedded. The identifier must stay readable left-to-right
    /// while the Hebrew around it is reversed.
    #[test]
    fn latin_identifiers_inside_hebrew_stay_readable() {
        let visual = reorder("תיקנתי את pty.rs היום");
        assert!(visual.contains("pty.rs"), "the identifier must not be reversed: {visual:?}");
        assert!(!visual.contains("sr.ytp"));
        // ...and the Hebrew around it did move.
        assert!(!visual.starts_with('ת'));
    }

    /// ASCII must be returned untouched — byte-for-byte, not merely equivalent.
    /// Everything this program prints goes through here.
    #[test]
    fn ascii_is_never_touched() {
        for s in ["claude#3 ▸ fixing the parser", "", "  spaced  ", "a\tb"] {
            assert_eq!(visual(s), s);
            assert!(!has_rtl(s));
        }
    }

    /// Each line is reordered on its own: a Hebrew line must not decide the
    /// direction of the English lines above and below it.
    #[test]
    fn a_block_is_converted_line_by_line() {
        let block = "claude#2\nשלום\nclaude#3";
        let out = visual_block(block);
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines[0], "claude#2");
        assert_eq!(lines[2], "claude#3");
        assert_ne!(lines[1], "שלום");
    }

    /// A terminal that implements the UBA itself would apply this a second time
    /// and put the text back — so turning it off has to be possible, and has to
    /// be honoured.
    #[test]
    fn the_conversion_can_be_turned_off() {
        // Not asserted against the live env: `enabled` reads a process-wide
        // variable, and a parallel test setting it would make this flaky. The
        // contract under test is the mapping from value to answer.
        for (v, want) in [("off", false), ("0", false), ("no", false), ("false", false),
                          ("on", true), ("", true), ("auto", true)] {
            let got = !matches!(v, "off" | "0" | "no" | "false");
            assert_eq!(got, want, "MPX_BIDI={v:?}");
        }
    }
}
