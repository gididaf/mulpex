//! Mulpex — a 3-pane terminal shell wrapping a live Claude Code session.
//!
//! Run it from inside a project directory:
//!
//! ```text
//! cd /path/to/project
//! mulpex
//! ```
//!
//! Layout: [ instances sidebar | live Claude Code | info sidebar ].
//! Ctrl+Q quits Mulpex; every other key forwards to Claude.

mod app;
mod keymap;
mod pane;
mod persist;
mod term_session;
mod ui;

use std::io::stdout;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::supports_keyboard_enhancement;
use ratatui::layout::Rect;

fn main() -> anyhow::Result<()> {
    let project_dir = std::env::current_dir()?;

    // ratatui::init() enables raw mode + alternate screen and installs a panic
    // hook that restores the terminal, so a crash never leaves it corrupted.
    let mut terminal = ratatui::init();

    // Enable the Kitty keyboard protocol where supported (e.g. iTerm2) so that
    // modified keys like Alt+arrows are delivered reliably regardless of the
    // terminal's Option-key configuration. Harmless to skip where unsupported.
    let kitty = matches!(supports_keyboard_enhancement(), Ok(true));
    if kitty {
        let _ = execute!(
            stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let _ = execute!(stdout(), EnableBracketedPaste);
    // Capture mouse events so we can forward them to Claude (scroll, clicks)
    // instead of letting the outer terminal translate the wheel into arrow keys.
    let _ = execute!(stdout(), EnableMouseCapture);

    let size = terminal.size()?;
    let result = app::App::new(project_dir, Rect::new(0, 0, size.width, size.height), kitty)
        .and_then(|mut app| app.run(&mut terminal));

    let _ = execute!(stdout(), DisableMouseCapture);
    let _ = execute!(stdout(), DisableBracketedPaste);
    if kitty {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    result
}
