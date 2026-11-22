use std::io::{self, Stdout};

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::Terminal;
use std::io::Write;
use unicode_width::UnicodeWidthStr;

use crate::config::{Cursor, CursorShape};

pub type Tui = Terminal<WidthAwareBackend<CrosstermBackend<Stdout>>>;

/// Wraps a backend whose cursor prediction advances one column per printed
/// cell. A wide glyph advances the real terminal cursor by two, so within one
/// frame every changed cell printed after it lands one column to the right —
/// on Windows that was the gapped sidebar divider and the detached glyphs
/// mid-transcript (#3216). The frame's changed cells are emitted in runs that
/// end after a wide glyph; each new run makes the inner backend start with its
/// own cursor move, which re-syncs the prediction at the true cell.
pub struct WidthAwareBackend<B> {
    inner: B,
}

impl<B> WidthAwareBackend<B> {
    pub fn new(inner: B) -> Self {
        Self { inner }
    }

    /// Hands the wrapped backend back, so a caller can reach the writer it
    /// owns.
    pub fn into_inner(self) -> B {
        self.inner
    }
}

impl<B: Backend> Backend for WidthAwareBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut run = Vec::new();
        for cell in content {
            let wide = cell.2.symbol().width() > 1;
            run.push(cell);
            if wide {
                self.inner.draw(run.drain(..))?;
            }
        }
        if !run.is_empty() {
            self.inner.draw(run.drain(..))?;
        }
        Ok(())
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        Backend::flush(&mut self.inner)
    }
}

/// `draw_frame` writes the synchronized-update markers straight through the
/// backend's writer, so the wrapper passes `io::Write` through too.
impl<W: io::Write> io::Write for WidthAwareBackend<CrosstermBackend<W>> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.inner)
    }
}

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
///
/// The screen is handed back at most once. A second `LeaveAlternateScreen`
/// after the exit epilogue put the cursor back where it stood before the
/// alternate screen, so the shell's next prompt was written over the epilogue
/// and the line it replaced instead of after them.
pub struct RestoreGuard {
    handed_back: bool,
}

impl RestoreGuard {
    pub fn new() -> Self {
        Self { handed_back: false }
    }

    /// Hand the screen and input back. Calling it again — including from the
    /// drop that covers early returns — writes nothing.
    pub fn hand_back(&mut self) {
        self.hand_back_to(&mut io::stdout());
        let _ = disable_raw_mode();
    }

    /// Whether this call was the one that handed the screen back.
    fn hand_back_to(&mut self, writer: &mut impl Write) -> bool {
        if self.handed_back {
            return false;
        }
        self.handed_back = true;
        // Leave the screen on its own and flush it first: `execute!` stops at
        // the first failing command, so a Windows mode restore queued after the
        // leave could abort before the leave reached the terminal.
        let _ = leave_alternate(writer);
        let _ = execute!(
            writer,
            DisableBracketedPaste,
            DisableFocusChange,
            DisableMouseCapture,
            SetCursorStyle::DefaultUserShape
        );
        true
    }
}

impl Default for RestoreGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        self.hand_back();
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
    Terminal::new(WidthAwareBackend::new(CrosstermBackend::new(io::stdout())))
}

/// Mouse capture and the cursor shape are terminal state, so they are turned
/// off on the way out whether or not they were on.
pub fn restore() {
    RestoreGuard::new().hand_back();
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
    use std::sync::{Arc, Mutex};

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

    /// The exit path hands the screen back once and then writes the epilogue.
    /// A second hand-back re-restores the pre-TUI cursor, which is what made the
    /// shell paint its prompt over the epilogue after quitting.
    #[test]
    fn handing_the_screen_back_is_a_one_shot() {
        let mut guard = RestoreGuard::new();
        let mut first = Vec::new();
        assert!(guard.hand_back_to(&mut first));
        let mut second = Vec::new();
        assert!(!guard.hand_back_to(&mut second));
        assert!(second.is_empty(), "the second hand-back wrote {second:?}");
        assert_eq!(
            first
                .windows(LEAVE_ALTERNATE.len())
                .filter(|window| *window == LEAVE_ALTERNATE)
                .count(),
            1,
            "{first:?}"
        );
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

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("shared buffer").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SharedBuf {
        fn data(&self) -> Vec<u8> {
            self.0.lock().expect("shared buffer").clone()
        }
    }

    /// One frame whose diff is `中 a b` on one row. The real terminal advances
    /// two columns for the glyph; with the runs split after it, `a` and `b`
    /// print on their own columns instead of drifting one to the right. An
    /// emulator, not the buffer, is what judges the byte stream — this is the
    /// failure class `TestBackend` cannot see.
    #[test]
    fn the_cell_after_a_wide_glyph_prints_at_its_own_column() {
        let shared = SharedBuf::default();
        let mut backend = WidthAwareBackend::new(CrosstermBackend::new(shared.clone()));
        let mut cells = [Cell::default(), Cell::default(), Cell::default()];
        cells[0].set_symbol("中");
        cells[1].set_symbol("a");
        cells[2].set_symbol("b");
        let content = [
            (0u16, 0u16, &cells[0]),
            (1u16, 0u16, &cells[1]),
            (2u16, 0u16, &cells[2]),
        ];
        backend.draw(content.into_iter()).expect("draw one row");
        let bytes = shared.data();

        assert!(
            bytes.windows(6).any(|window| window == b"\x1b[1;2H"),
            "a cursor move must separate the glyph from the next cell: {bytes:?}"
        );
        let mut parser = vt100::Parser::new(1, 10, 0);
        parser.process(&bytes);
        let screen = parser.screen();
        // The emulator owns the overlap at the glyph itself: printing into the
        // glyph's second half may erase it, and `cell(0, 0)` then reads empty.
        // What the fix guarantees is that nothing drifts — `a` and `b` land on
        // the columns the app set, not one to the right of the glyph's
        // two-column advance.
        assert_eq!(screen.cell(0, 1).expect("cell").contents(), "a");
        assert_eq!(screen.cell(0, 2).expect("cell").contents(), "b");
        assert_eq!(screen.cell(0, 3).expect("cell").contents(), "");
    }
}
