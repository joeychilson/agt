//! Frames of styled cells, drawn on the terminal's alternate screen.
//!
//! Each frame is compared with the one the terminal shows, and only the cells
//! that changed are written, in one synchronized update with the cursor hidden
//! while they are, so neither half-drawn rows nor a wandering cursor show on
//! any terminal. Nothing is cleared, even when the whole screen is drawn again,
//! so nothing flashes. A frame costs what changed rather than what the screen
//! holds: unchanged rows are skipped whole, and the frame and output buffers
//! are reused. Autowrap is off, so writing the last column never moves the
//! cursor. A cell holds a character one or two columns wide; characters of
//! width zero never reach it, so the terminal's columns always agree with the
//! frame's.

use std::io::{self, Write};
use std::ops::Range;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{execute, terminal};
use unicode_width::UnicodeWidthChar;

use super::text::{Glyph, Style};

/// Switches to the alternate screen with autowrap and the cursor off, and
/// turns on bracketed paste and mouse reports in SGR form for buttons, drags
/// and the wheel. Plain movement is not reported, so moving the mouse wakes
/// nothing.
const ENTER: &[u8] = b"\x1b[?1049h\x1b[?7l\x1b[?25l\x1b[?2004h\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const LEAVE: &[u8] =
    b"\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[0m\x1b[?25h\x1b[?7h\x1b[?1049l";
const SYNC_START: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

/// Fills the second column of a wide character.
const WIDE: char = '\0';

/// Puts the terminal in the UI's modes, and restores it when dropped,
/// including on panic.
pub(crate) struct Terminal;

impl Terminal {
    pub(crate) fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            hook(info);
        }));
        let mut out = io::stdout();
        out.write_all(ENTER)?;
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        Ok(guard)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    // Best effort: nothing more can be done if the terminal is gone.
    let mut out = io::stdout();
    let _ = execute!(out, PopKeyboardEnhancementFlags);
    let _ = out.write_all(LEAVE);
    let _ = out.flush();
    let _ = terminal::disable_raw_mode();
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Cell {
    style: Style,
    ch: char,
}

/// Spaces are stored in one style unless their style shows on a space.
const BLANK: Cell = Cell { style: Style::PLAIN, ch: ' ' };

/// What the screen shows: rows of cells, and where the cursor is if it shows.
pub(crate) struct Frame {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
    cursor: Option<(usize, usize)>,
}

impl Frame {
    pub(crate) fn new(width: usize, height: usize) -> Self {
        Self { width, height, cells: vec![BLANK; width * height], cursor: None }
    }

    pub(crate) fn width(&self) -> usize {
        self.width
    }

    pub(crate) fn height(&self) -> usize {
        self.height
    }

    /// Writes `glyphs` on row `y` from column `x`, stopping before column
    /// `end` or at a wide character that does not fit, and returns the column
    /// after the last one written.
    pub(crate) fn put(&mut self, mut x: usize, y: usize, glyphs: &[Glyph], end: usize) -> usize {
        if y >= self.height {
            return x;
        }
        let end = end.min(self.width);
        for &(style, ch) in glyphs {
            let Some(width @ 1..) = ch.width() else { continue };
            if x + width > end {
                break;
            }
            self.set(x, y, Cell { style, ch });
            if width == 2 {
                self.set(x + 1, y, Cell { style, ch: WIDE });
            }
            x += width;
        }
        x
    }

    /// Shows `columns` of row `y` as one bar in reverse video, whatever their
    /// styles, as selected text shows.
    pub(crate) fn highlight(&mut self, y: usize, columns: Range<usize>) {
        if y >= self.height {
            return;
        }
        let row = &mut self.cells[y * self.width..(y + 1) * self.width];
        let end = columns.end.min(row.len());
        for cell in &mut row[columns.start.min(end)..end] {
            cell.style = Style::PLAIN.reversed();
        }
    }

    /// Shows the cursor at column `x` of row `y`.
    pub(crate) fn cursor(&mut self, x: usize, y: usize) {
        if x < self.width && y < self.height {
            self.cursor = Some((x, y));
        }
    }

    /// Empties the frame for a screen of `width` × `height`, keeping its
    /// buffer.
    fn reset(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.cells.clear();
        self.cells.resize(width * height, BLANK);
        self.cursor = None;
    }

    fn set(&mut self, x: usize, y: usize, cell: Cell) {
        let row = &mut self.cells[y * self.width..(y + 1) * self.width];
        // A wide character this write cuts in half leaves a blank behind.
        if row[x].ch == WIDE && cell.ch != WIDE && x > 0 {
            row[x - 1] = BLANK;
        }
        if row.get(x + 1).is_some_and(|next| next.ch == WIDE) {
            row[x + 1] = BLANK;
        }
        row[x] = if cell.ch == ' ' && !cell.style.shows_on_space() { BLANK } else { cell };
    }

    /// Where the cursor shows, if it does.
    #[cfg(test)]
    pub(crate) fn cursor_at(&self) -> Option<(usize, usize)> {
        self.cursor
    }

    /// The style of the cell at column `x` of row `y`.
    #[cfg(test)]
    pub(crate) fn style(&self, x: usize, y: usize) -> Style {
        self.cells[y * self.width + x].style
    }

    /// The characters of row `y`.
    #[cfg(test)]
    pub(crate) fn row(&self, y: usize) -> String {
        self.cells[y * self.width..(y + 1) * self.width]
            .iter()
            .filter(|cell| cell.ch != WIDE)
            .map(|cell| cell.ch)
            .collect()
    }
}

/// The terminal's screen, as last drawn.
pub(crate) struct Screen {
    out: io::Stdout,
    /// The frame the terminal shows, or `None` when its contents are unknown.
    shown: Option<Frame>,
    /// A frame no longer shown, whose buffer the next frame reuses.
    spare: Option<Frame>,
    /// The bytes of the last update, whose buffer the next one reuses.
    bytes: Vec<u8>,
}

impl Screen {
    pub(crate) fn new() -> Self {
        Self { out: io::stdout(), shown: None, spare: None, bytes: Vec::new() }
    }

    /// A blank frame for a screen of `width` × `height`.
    pub(crate) fn frame(&mut self, width: usize, height: usize) -> Frame {
        let mut frame = self.spare.take().unwrap_or_else(|| Frame::new(0, 0));
        frame.reset(width, height);
        frame
    }

    /// Forgets what the terminal shows, so the next frame is drawn whole. A
    /// resized terminal may have moved or cleared any cell.
    pub(crate) fn invalidate(&mut self) {
        if let Some(shown) = self.shown.take() {
            self.spare = Some(shown);
        }
    }

    pub(crate) fn draw(&mut self, frame: Frame) -> io::Result<()> {
        self.render(frame);
        if self.bytes.is_empty() {
            return Ok(());
        }
        let mut out = self.out.lock();
        out.write_all(&self.bytes)?;
        out.flush()
    }

    /// Puts `text` on the system clipboard through the terminal (OSC 52).
    pub(crate) fn copy(&mut self, text: &str) -> io::Result<()> {
        let mut out = self.out.lock();
        write!(out, "\x1b]52;c;{}\x07", STANDARD.encode(text))?;
        out.flush()
    }

    /// Fills `bytes` with what changes the terminal from the shown frame to
    /// `frame`, which becomes the shown one; nothing when nothing changed.
    fn render(&mut self, frame: Frame) {
        let shown = self
            .shown
            .take()
            .filter(|shown| shown.width == frame.width && shown.height == frame.height);
        let out = &mut self.bytes;
        out.clear();
        out.extend_from_slice(SYNC_START);
        let mut style = None;
        let mut at = None;
        let mut wrote = false;
        for y in 0..frame.height {
            let start = y * frame.width;
            let cells = &frame.cells[start..start + frame.width];
            if shown.as_ref().is_some_and(|shown| shown.cells[start..start + frame.width] == *cells)
            {
                continue;
            }
            for (x, &cell) in cells.iter().enumerate() {
                if cell.ch == WIDE {
                    continue;
                }
                let width = cell.ch.width().unwrap_or(1);
                let changed = shown.as_ref().is_none_or(|shown| {
                    shown.cells[start + x] != cell
                        || (width == 2 && shown.cells[start + x + 1].ch != WIDE)
                });
                if !changed {
                    continue;
                }
                if !wrote {
                    out.extend_from_slice(HIDE_CURSOR);
                    wrote = true;
                }
                if at != Some((x, y)) {
                    let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
                }
                if style != Some(cell.style) {
                    cell.style.select(out);
                    style = Some(cell.style);
                }
                out.extend_from_slice(cell.ch.encode_utf8(&mut [0; 4]).as_bytes());
                at = Some((x + width, y));
            }
        }
        // What the terminal's cursor was: `None` when unknown.
        let before = shown.as_ref().map(|shown| shown.cursor);
        match frame.cursor {
            Some((x, y)) if wrote || before != Some(frame.cursor) => {
                let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
                out.extend_from_slice(SHOW_CURSOR);
            }
            None if !wrote && before != Some(None) => out.extend_from_slice(HIDE_CURSOR),
            _ => {}
        }
        self.spare = shown;
        self.shown = Some(frame);
        if out.len() == SYNC_START.len() {
            out.clear();
        } else {
            out.extend_from_slice(SYNC_END);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::text::Attribute;

    const STYLES: [Style; 12] = [
        Style::PLAIN,
        Style::BOLD,
        Style::DIM,
        Style::THINKING,
        Style::CODE,
        Style::ACCENT,
        Style::RED,
        Style::YELLOW,
        Style::GREEN,
        Style::CODE.with(Attribute::Underline),
        Style::PLAIN.reversed(),
        Style::DIM.reversed(),
    ];

    /// The escape sequence that selects what an emulator cell shows, written
    /// as `Style::select` writes it.
    fn shown_style(cell: &vt100::Cell) -> String {
        let mut shown = String::from("\x1b[0");
        let attributes = [
            (cell.bold(), ";1"),
            (cell.dim(), ";2"),
            (cell.italic(), ";3"),
            (cell.underline(), ";4"),
            (cell.inverse(), ";7"),
        ];
        for (on, parameter) in attributes {
            if on {
                shown.push_str(parameter);
            }
        }
        if let vt100::Color::Idx(index) = cell.fgcolor() {
            shown.push_str(&format!(";{}", 30 + u16::from(index)));
        }
        shown + "m"
    }

    /// Checks that the emulator shows `frame` cell for cell.
    fn assert_shows(parser: &vt100::Parser, frame: &Frame, step: usize) {
        let screen = parser.screen();
        for y in 0..frame.height {
            for x in 0..frame.width {
                let expected = frame.cells[y * frame.width + x];
                let cell = screen.cell(y as u16, x as u16).expect("cell in bounds");
                let at = format!("step {step}, row {y}, column {x}: {expected:?}");
                if expected.ch == WIDE {
                    assert!(cell.is_wide_continuation(), "{at}");
                    continue;
                }
                let contents = if expected.ch == ' ' { "" } else { cell.contents() };
                let expected_contents =
                    if expected.ch == ' ' { String::new() } else { expected.ch.to_string() };
                assert_eq!(contents, expected_contents, "{at}");
                if expected.ch != ' ' || expected.style.shows_on_space() {
                    let mut selected = Vec::new();
                    expected.style.select(&mut selected);
                    let selected = String::from_utf8(selected).expect("ASCII");
                    assert_eq!(shown_style(cell), selected, "{at}");
                }
            }
        }
        let cursor = frame.cursor.map(|(x, y)| (y as u16, x as u16));
        assert_eq!(screen.hide_cursor(), cursor.is_none(), "step {step}");
        if let Some(cursor) = cursor {
            assert_eq!(screen.cursor_position(), cursor, "step {step}");
        }
    }

    #[test]
    fn frames_drawn_one_after_another_show_exactly_the_last() {
        // A fixed pseudo-random sequence, so a failure can be replayed.
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        let (width, height) = (13, 5);
        let mut parser = vt100::Parser::new(height as u16, width as u16, 0);
        let mut screen = Screen::new();
        let characters = ['a', 'b', ' ', '界', '─', 'é', '日'];
        let mut last = Frame::new(width, height);
        for step in 0..400 {
            // Most frames change a little of the last one, as live frames do.
            let mut frame = screen.frame(width, height);
            if next(4) > 0 {
                frame.cells.clone_from(&last.cells);
            }
            for _ in 0..next(6) {
                let glyphs: Vec<Glyph> = (0..next(8))
                    .map(|_| (STYLES[next(STYLES.len())], characters[next(characters.len())]))
                    .collect();
                let (x, y) = (next(width), next(height));
                frame.put(x, y, &glyphs, x + next(width));
            }
            if next(3) > 0 {
                frame.cursor(next(width), next(height));
            }
            if next(50) == 0 {
                screen.invalidate();
            }
            last = Frame { cells: frame.cells.clone(), ..frame };
            screen.render(Frame { cells: last.cells.clone(), ..last });
            parser.process(&screen.bytes);
            assert_shows(&parser, &last, step);
        }
    }

    #[test]
    fn an_unchanged_frame_writes_nothing() {
        let mut screen = Screen::new();
        let draw = |screen: &mut Screen| {
            let mut frame = screen.frame(10, 2);
            frame.put(0, 0, &[(Style::BOLD, 'x')], 10);
            frame.cursor(3, 1);
            screen.render(frame);
            screen.bytes.len()
        };
        assert!(draw(&mut screen) > 0);
        assert_eq!(draw(&mut screen), 0);
    }

    #[test]
    fn updates_hide_the_cursor_while_writing_and_never_clear() {
        let mut screen = Screen::new();
        let mut frame = screen.frame(10, 2);
        frame.put(0, 0, &[(Style::PLAIN, 'x')], 10);
        frame.cursor(1, 0);
        screen.render(frame);
        let first = String::from_utf8(screen.bytes.clone()).expect("UTF-8");
        assert!(first.starts_with("\x1b[?2026h\x1b[?25l"), "{first:?}");
        assert!(first.ends_with("\x1b[1;2H\x1b[?25h\x1b[?2026l"), "{first:?}");
        screen.invalidate();
        let mut frame = screen.frame(10, 2);
        frame.put(0, 0, &[(Style::PLAIN, 'y')], 10);
        screen.render(frame);
        let whole = String::from_utf8(screen.bytes.clone()).expect("UTF-8");
        assert!(!whole.contains("\x1b[2J") && !whole.contains("\x1b[J"), "{whole:?}");
        assert_eq!(whole.matches(' ').count(), 19, "a repaint writes every cell: {whole:?}");
    }

    #[test]
    fn writes_repair_wide_characters_they_cut() {
        let mut frame = Frame::new(6, 1);
        frame.put(0, 0, &[(Style::PLAIN, '界'), (Style::PLAIN, '界')], 6);
        frame.put(1, 0, &[(Style::PLAIN, 'x')], 6);
        assert_eq!(frame.row(0), " x界  ");
        frame.put(4, 0, &[(Style::PLAIN, '日')], 5);
        assert_eq!(frame.row(0), " x界  ", "a wide character stops at the end it cannot fit");
        frame.put(3, 0, &[(Style::PLAIN, '日')], 6);
        assert_eq!(frame.row(0), " x 日 ");
    }
}
