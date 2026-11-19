use std::io::{self, Stdout};

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::Terminal;
use std::io::Write;

use crate::config::{Cursor, CursorShape};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// The xterm sequences for the alternate screen.
///
/// Windows Terminal hands the app a ConPTY pipe, where crossterm decides ANSI
/// is unsupported and implements `EnterAlternateScreen` as a console-buffer
/// switch. That switch is not the screen the operator sees: the TUI keeps
/// painting the visible buffer, and after exit the shell and the epilogue land
/// on top of the leftover frame. Unix writes exactly these bytes through
/// crossterm, so both platforms use the pair directly and keep the same exit
/// ordering: leave first, flush, then the mode restores that can fail.
const ENTER_ALTERNATE: &[u8] = b"\x1b[?1049h";
const LEAVE_ALTERNATE: &[u8] = b"\x1b[?1049l";

/// Present one frame atomically. Streaming turns repaint the transcript many
/// times a second, and terminals that apply partial updates mid-frame show
/// that as full-screen flicker; wrapping the frame in synchronized-update
/// markers lets capable terminals buffer and present it in one piece.
/// Terminals without support ignore the markers and behave as before.
///
/// Both markers are written through the *backend's* stdout, the same handle
/// `terminal.draw` writes to. Writing them on a second handle reordered them
/// against the frame: the closing marker could reach the terminal before the
/// cells did, which left a capable terminal holding an update it had been told
/// to begin and never told to end — the screen stopped repainting while the
/// status clock kept ticking. One handle, one order.
pub fn draw_frame<F>(terminal: &mut Tui, render: F) -> io::Result<()>
where
    F: FnOnce(&mut ratatui::Frame),
{
    terminal.backend_mut().write_all(b"\x1b[?2026h")?;
    let drawn = terminal.draw(render);
    let result = drawn.map(|_frame| ());
    terminal.backend_mut().write_all(b"\x1b[?2026l")?;
    Write::flush(terminal.backend_mut())?;
    result
}

/// Covers normal exit and every error after raw-mode initialization begins.
pub struct RestoreGuard;

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Terminal settings that must be re-applied after the external editor gives
/// the screen back.
#[derive(Debug, Clone, Copy, Default)]
pub struct Preferences {
    pub mouse: bool,
    pub cursor: Option<Cursor>,
    /// Whether the terminal is asked for focus-change reports at all. Off
    /// keeps the escapes out of terminals that turn them into garbage input;
    /// [`restore`] still disables reporting unconditionally, so a flipped
    /// preference takes effect without a stale terminal mode.
    pub focus_reporting: bool,
}

pub fn init(preferences: Preferences) -> io::Result<Tui> {
    enter(preferences)?;
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

/// Mouse capture and the cursor shape are terminal state, so they are turned
/// off on the way out whether or not they were on.
pub fn restore() {
    let mut stdout = io::stdout();
    // Leave the screen on its own and flush it first: `execute!` stops at the
    // first failing command, so a Windows mode restore queued after the leave
    // could abort before the leave reached the terminal.
    let _ = leave_alternate(&mut stdout);
    let _ = execute!(
        stdout,
        DisableBracketedPaste,
        DisableFocusChange,
        DisableMouseCapture,
        SetCursorStyle::DefaultUserShape
    );
    let _ = disable_raw_mode();
}

/// Hand the terminal to an external editor: leave the alternate screen and
/// stop raw mode so the child owns cooked input and output.
pub fn suspend() {
    restore();
}

/// Take the terminal back after [`suspend`] and repaint from scratch.
pub fn resume(terminal: &mut Tui, preferences: Preferences) -> io::Result<()> {
    enter(preferences)?;
    redraw(terminal)
}

/// Raw mode, the alternate screen, bracketed paste, and — unless the
/// configuration turned focus reporting off — focus changes, in the order both
/// entry points used to spell out.
fn enter(preferences: Preferences) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    enter_alternate(&mut stdout)?;
    execute!(stdout, EnableBracketedPaste)?;
    if preferences.focus_reporting {
        execute!(io::stdout(), EnableFocusChange)?;
    }
    apply(preferences);
    Ok(())
}

fn enter_alternate(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(ENTER_ALTERNATE)?;
    writer.flush()
}

fn leave_alternate(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(LEAVE_ALTERNATE)?;
    writer.flush()
}

/// Fullscreen repaint without a cursor-position query. `Terminal::clear`
/// snapshots the cursor through stdin, which races the dedicated input reader.
/// Resizing the fullscreen viewport also resets its backing buffers. Generic
/// over the backend so tests can drive the same path against a captured
/// writer or ratatui's `TestBackend`.
pub fn redraw<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    let size = terminal.size()?;
    terminal.resize(ratatui::layout::Rect::new(0, 0, size.width, size.height))
}

fn apply(preferences: Preferences) {
    if preferences.mouse {
        let _ = execute!(io::stdout(), EnableMouseCapture);
    }
    if let Some(cursor) = preferences.cursor {
        let _ = execute!(io::stdout(), cursor_style(cursor));
    }
}

fn cursor_style(cursor: Cursor) -> SetCursorStyle {
    match (cursor.shape(), cursor.blinking()) {
        (CursorShape::Default, _) => SetCursorStyle::DefaultUserShape,
        (CursorShape::Block, true) => SetCursorStyle::BlinkingBlock,
        (CursorShape::Block, false) => SetCursorStyle::SteadyBlock,
        (CursorShape::Underline, true) => SetCursorStyle::BlinkingUnderScore,
        (CursorShape::Underline, false) => SetCursorStyle::SteadyUnderScore,
        (CursorShape::Line, true) => SetCursorStyle::BlinkingBar,
        (CursorShape::Line, false) => SetCursorStyle::SteadyBar,
    }
}

pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cursor_style_follows_the_configuration() {
        let cursor = |style, blinking| Cursor {
            style: Some(style),
            blinking: Some(blinking),
        };
        assert!(matches!(
            cursor_style(cursor(CursorShape::Block, true)),
            SetCursorStyle::BlinkingBlock
        ));
        assert!(matches!(
            cursor_style(cursor(CursorShape::Block, false)),
            SetCursorStyle::SteadyBlock
        ));
        assert!(matches!(
            cursor_style(cursor(CursorShape::Underline, false)),
            SetCursorStyle::SteadyUnderScore
        ));
        assert!(matches!(
            cursor_style(cursor(CursorShape::Line, true)),
            SetCursorStyle::BlinkingBar
        ));
        // A configured cursor with the `default` shape preserves the terminal
        // setting, which is what `DefaultUserShape` restores.
        assert!(matches!(
            cursor_style(cursor(CursorShape::Default, true)),
            SetCursorStyle::DefaultUserShape
        ));
    }

    /// The alternate screen is what hides the frame from the shell. The pair
    /// has to be written explicitly on Windows, because crossterm's console
    /// buffer fallback is not the screen the terminal shows; the exit half is
    /// flushed before the mode restores that can fail.
    #[test]
    fn the_alternate_screen_is_written_and_flushed_as_the_xterm_pair() {
        let mut entered = Vec::new();
        enter_alternate(&mut entered).expect("enter the alternate screen");
        assert_eq!(entered, ENTER_ALTERNATE);

        let mut left = Vec::new();
        leave_alternate(&mut left).expect("leave the alternate screen");
        assert_eq!(left, LEAVE_ALTERNATE);
    }
}
