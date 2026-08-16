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
use crossterm::style::{Attribute, ResetColor, SetAttribute};
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
    reset_style(&mut out);
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

/// Runs a command with the terminal handed back to it, then restores the TUI.
///
/// Editors need the real terminal: they set their own raw mode, draw their own
/// screen, and read keys directly. Trying to proxy that through the TUI would be
/// reimplementing a terminal emulator. Leaving and returning is what every other
/// program does for this, and it is exactly right here.
pub fn suspended<T>(f: impl FnOnce() -> T) -> T {
    restore();
    let result = f();
    // Re-entering can fail if the terminal went away; the caller is exiting in
    // that case anyway.
    let _ = enable_raw_mode();
    let _ = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture);
    // The child's colours are still in effect, and the caller clears the screen
    // next. Erasing is *background-colour sensitive*: `ESC[2J` fills with
    // whatever background is currently set, so an editor that exits without
    // resetting leaves the whole screen painted its colour. Ratatui then
    // diffs the new frame against an empty buffer, so every cell that stays
    // blank is considered unchanged and never repainted — the stale colour
    // survives indefinitely, retreating only where text happens to land. That
    // is the black smear behind the panes. Reset before anyone erases.
    reset_style(&mut io::stdout());
    result
}

/// Clears colour and attribute state.
///
/// Generic over the writer purely so the emitted sequence can be asserted in a
/// test — the bug it prevents is invisible in any other way.
fn reset_style(out: &mut impl io::Write) {
    let _ = execute!(out, ResetColor, SetAttribute(Attribute::Reset));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_style_reset_actually_emits_a_reset() {
        // Guards the fix for the smear: after an external editor exits, its
        // colours are still set, and `ESC[2J` erases to the *current*
        // background. Ratatui then diffs against an empty buffer, so blank
        // cells count as unchanged and never get repainted — the wrong colour
        // stays until text happens to cover it. One reproduction left 206
        // black cells behind on a 170x41 screen.
        let mut buf: Vec<u8> = Vec::new();
        reset_style(&mut buf);
        assert!(
            String::from_utf8(buf).unwrap().contains("\x1b[0m"),
            "reset_style must emit SGR 0 before anything erases the screen"
        );
    }
}
