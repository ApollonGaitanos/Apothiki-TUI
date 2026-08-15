//! Terminal setup and teardown.
//!
//! **Restoring the terminal is not optional.** A TUI that leaves raw mode on
//! after a crash gives the user a shell with no echo and no line editing, and
//! the only fix they will know is closing the window (spec §13.9). So teardown
//! runs from three places: normal exit, a `Drop` guard for early returns and
//! `?`, and a panic hook installed before the terminal is ever touched.

use std::io::{self, Stdout};

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Restores the terminal to a usable state. Safe to call more than once.
pub fn restore() {
    let mut out = io::stdout();
    // Popping the enhancement flags is harmless if they were never pushed.
    let _ = execute!(out, PopKeyboardEnhancementFlags);
    let _ = execute!(out, DisableMouseCapture, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

/// Restores the terminal when it goes out of scope, including on `?` and panic
/// unwind. Held by `run` for the lifetime of the UI.
pub struct Guard {
    /// Whether the Kitty keyboard protocol was successfully enabled.
    pub enhanced_keys: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

/// Enters the alternate screen and raw mode.
///
/// Reports whether the Kitty keyboard protocol is available. It is not on the
/// primary target: Konsole does not implement it, so the fallback binding set is
/// the *normal* path here rather than a degraded one (spec §8.1). Never assume
/// support — probe and adapt.
pub fn init() -> anyhow::Result<(Tui, Guard)> {
    // Installed before raw mode so a panic during setup still restores.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));

    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;

    // Disambiguates Ctrl+I from Tab and Ctrl+H from Backspace where supported.
    let enhanced_keys = execute!(
        out,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok()
        && crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);

    let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    Ok((terminal, Guard { enhanced_keys }))
}
