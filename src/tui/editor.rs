//! The prompt editor: a multi-line buffer with a cursor, attachments and
//! input history.
//!
//! A large paste and an attached image are chips: a label in the text that
//! moves and deletes as one unit and stands for what it names, which the
//! message carries once it is sent.

use std::ops::Range;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

use super::text::{Line, Style};

/// Pastes longer than this become a chip, so a large paste does not fill
/// the input.
const PASTE_LINES: usize = 10;
const PASTE_CHARS: usize = 2000;
const HISTORY_ENTRIES: usize = 100;
const HISTORY_BYTES: usize = 256 * 1024;

/// The editor wrapped to a width, with the cursor's row and column.
pub(crate) struct Layout {
    pub(crate) rows: Vec<Line>,
    pub(crate) cursor: (usize, usize),
    starts: Vec<usize>,
}

/// Where an attached image comes from.
pub(crate) enum Source {
    File(PathBuf),
    /// PNG data read from the clipboard.
    Clipboard(Vec<u8>),
}

/// A part of a message: text, or an image still to be prepared.
pub(crate) enum Part {
    Text(String),
    Image(Source),
}

/// A message ready to send: the text the user wrote, without its images, and
/// the parts it is made of.
pub(crate) struct Draft {
    pub(crate) typed: String,
    pub(crate) parts: Vec<Part>,
}

impl Draft {
    pub(crate) fn text(text: String) -> Self {
        Self { parts: vec![Part::Text(text.clone())], typed: text }
    }

    pub(crate) fn has_images(&self) -> bool {
        self.parts.iter().any(|part| matches!(part, Part::Image(_)))
    }
}

/// What a chip stands for.
enum Chip {
    Paste(String),
    Image(Source),
}

/// A chip's label in the text, by its byte range.
struct Token {
    range: Range<usize>,
    chip: Chip,
}

#[derive(Default)]
pub(crate) struct Editor {
    text: String,
    /// Byte offset, always on a character boundary.
    cursor: usize,
    /// Chips in the text, in order.
    tokens: Vec<Token>,
    history: Vec<String>,
    browsing: Option<usize>,
    draft: Option<(String, usize, Vec<Token>)>,
}

impl Editor {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
        self.browsing = None;
        self.draft = None;
        self.tokens.clear();
    }

    pub(crate) fn clear(&mut self) {
        self.set(String::new());
    }

    /// Puts `text` on lines of its own before the current text, keeping the
    /// cursor where it was in that text and its chips intact.
    pub(crate) fn prepend(&mut self, text: &str) {
        let mut text = clean(text);
        if !self.text.is_empty() {
            text.push('\n');
        }
        let cursor = self.cursor;
        self.cursor = 0;
        self.insert_clean(&text);
        self.cursor = cursor + text.len();
    }

    /// Replaces the text in `range` with `text`, leaving the cursor after it.
    /// Chips the range touches are removed.
    pub(crate) fn replace(&mut self, range: Range<usize>, text: &str) {
        self.delete_range(range.start, range.end);
        self.insert(text);
    }

    /// Takes the message for sending, with pastes expanded and images as parts
    /// of their own, and records its text in the history.
    pub(crate) fn submit(&mut self) -> Draft {
        let text = std::mem::take(&mut self.text);
        let tokens = std::mem::take(&mut self.tokens);
        self.cursor = 0;
        self.browsing = None;
        self.draft = None;
        let mut typed = String::with_capacity(text.len());
        let mut parts = Vec::new();
        let mut segment = String::new();
        let mut last = 0;
        for Token { range, chip } in tokens {
            let before = &text[last..range.start];
            typed.push_str(before);
            segment.push_str(before);
            match chip {
                Chip::Paste(paste) => {
                    typed.push_str(&paste);
                    segment.push_str(&paste);
                }
                Chip::Image(source) => {
                    if !segment.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut segment)));
                    }
                    parts.push(Part::Image(source));
                }
            }
            last = range.end;
        }
        typed.push_str(&text[last..]);
        segment.push_str(&text[last..]);
        if !segment.is_empty() {
            parts.push(Part::Text(segment));
        }
        if !typed.trim().is_empty()
            && typed.len() <= HISTORY_BYTES
            && self.history.last() != Some(&typed)
        {
            self.history.push(typed.clone());
            let mut bytes: usize = self.history.iter().map(String::len).sum();
            while self.history.len() > HISTORY_ENTRIES || bytes > HISTORY_BYTES {
                bytes -= self.history.remove(0).len();
            }
        }
        Draft { typed, parts }
    }

    /// Inserts typed text.
    pub(crate) fn insert(&mut self, text: &str) {
        self.insert_clean(&clean(text));
    }

    /// Inserts pasted text, as a chip when it is large.
    pub(crate) fn paste(&mut self, text: &str) {
        let text = clean(text);
        let lines = text.lines().count();
        let chars = text.chars().count();
        if lines <= PASTE_LINES && chars <= PASTE_CHARS {
            return self.insert_clean(&text);
        }
        let size = if lines > 1 { format!("{lines} lines") } else { format!("{chars} characters") };
        self.chip(&format!("[pasted {size}]"), Chip::Paste(text));
    }

    /// Attaches an image, shown as a chip labeled `label`.
    pub(crate) fn attach(&mut self, label: &str, source: Source) {
        let before = self.text[..self.cursor].chars().next_back();
        if before.is_some_and(|c| !c.is_whitespace()) {
            self.insert_clean(" ");
        }
        self.chip(&format!("[{}]", clean(label)), Chip::Image(source));
        if self.text[self.cursor..].chars().next().is_none_or(|c| !c.is_whitespace()) {
            self.insert_clean(" ");
        }
    }

    /// Applies an editing key; other keys are ignored.
    pub(crate) fn key(&mut self, key: KeyEvent, width: usize) {
        let before = self.cursor;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert("\n");
            }
            KeyCode::Char('j') if ctrl => self.insert("\n"),
            KeyCode::Char('a') if ctrl => self.cursor = self.line_start(),
            KeyCode::Char('e') if ctrl => self.cursor = self.line_end(),
            KeyCode::Char('u') if ctrl => self.delete_range(self.line_start(), self.cursor),
            KeyCode::Char('k') if ctrl => self.delete_range(self.cursor, self.line_end()),
            KeyCode::Char('w') if ctrl => self.delete_range(self.word_start(), self.cursor),
            KeyCode::Backspace if alt || ctrl => self.delete_range(self.word_start(), self.cursor),
            KeyCode::Char(c) if !ctrl && !alt => self.insert(c.encode_utf8(&mut [0; 4])),
            KeyCode::Backspace => self.delete_range(self.prev(self.cursor), self.cursor),
            KeyCode::Delete => self.delete_range(self.cursor, self.next(self.cursor)),
            KeyCode::Left if alt || ctrl => self.cursor = self.word_start(),
            KeyCode::Right if alt || ctrl => self.cursor = self.word_end(),
            KeyCode::Left => self.cursor = self.prev(self.cursor),
            KeyCode::Right => self.cursor = self.next(self.cursor),
            KeyCode::Home => self.cursor = self.line_start(),
            KeyCode::End => self.cursor = self.line_end(),
            KeyCode::Up => self.vertical(false, width),
            KeyCode::Down => self.vertical(true, width),
            _ => {}
        }
        // A chip is one editing unit, even when it wraps on screen.
        if let Some(token) = self
            .tokens
            .iter()
            .find(|token| token.range.start < self.cursor && self.cursor < token.range.end)
        {
            self.cursor = if self.cursor < before { token.range.start } else { token.range.end };
        }
    }

    /// Wraps the text to `width` columns, which excludes the prompt prefix.
    /// Chips are drawn in cyan.
    pub(crate) fn layout(&self, width: usize) -> Layout {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut starts = vec![0];
        let mut cursor = (0, 0);
        let mut offset = 0;
        let mut tokens = self.tokens.iter().peekable();
        for line in self.text.split('\n') {
            let mut row = Vec::new();
            let mut col = 0;
            for (index, mut c) in line.char_indices() {
                let at = offset + index;
                while tokens.next_if(|token| token.range.end <= at).is_some() {}
                let style = match tokens.peek() {
                    Some(token) if token.range.contains(&at) => Style::CODE,
                    _ => Style::PLAIN,
                };
                if c.width().unwrap_or(0) > width {
                    c = '\u{fffd}';
                }
                let char_width = c.width().unwrap_or(0);
                if col + char_width > width {
                    rows.push(std::mem::take(&mut row));
                    starts.push(at);
                    col = 0;
                }
                if at == self.cursor {
                    cursor = (rows.len(), col);
                }
                row.push((style, c));
                col += char_width;
            }
            if offset + line.len() == self.cursor {
                // A cursor after a full row starts the next one.
                if col >= width {
                    rows.push(std::mem::take(&mut row));
                    starts.push(offset + line.len());
                    col = 0;
                }
                cursor = (rows.len(), col);
            }
            rows.push(row);
            offset += line.len() + 1;
            starts.push(offset);
        }
        starts.pop();
        Layout { rows, cursor, starts }
    }

    fn chip(&mut self, label: &str, chip: Chip) {
        let start = self.cursor;
        self.insert_clean(label);
        self.tokens.push(Token { range: start..self.cursor, chip });
        self.tokens.sort_by_key(|token| token.range.start);
    }

    fn insert_clean(&mut self, text: &str) {
        for token in &mut self.tokens {
            if token.range.start >= self.cursor {
                token.range.start += text.len();
                token.range.end += text.len();
            }
        }
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.browsing = None;
        self.draft = None;
    }

    fn delete_range(&mut self, mut start: usize, mut end: usize) {
        if start < end {
            for token in &self.tokens {
                if start < token.range.end && end > token.range.start {
                    start = start.min(token.range.start);
                    end = end.max(token.range.end);
                }
            }
            self.tokens.retain(|token| token.range.end <= start || token.range.start >= end);
            for token in &mut self.tokens {
                if token.range.start >= end {
                    token.range.start -= end - start;
                    token.range.end -= end - start;
                }
            }
            self.text.replace_range(start..end, "");
            self.cursor = start;
            self.browsing = None;
            self.draft = None;
        }
    }

    fn prev(&self, at: usize) -> usize {
        self.text[..at].char_indices().next_back().map_or(0, |(index, _)| index)
    }

    fn next(&self, at: usize) -> usize {
        self.text[at..].chars().next().map_or(at, |c| at + c.len_utf8())
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..].find('\n').map_or(self.text.len(), |index| self.cursor + index)
    }

    fn word_start(&self) -> usize {
        let before = self.text[..self.cursor].trim_end();
        before
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(index, c)| index + c.len_utf8())
    }

    fn word_end(&self) -> usize {
        let after = &self.text[self.cursor..];
        let skipped = after.len() - after.trim_start().len();
        after[skipped..]
            .find(char::is_whitespace)
            .map_or(self.text.len(), |index| self.cursor + skipped + index)
    }

    /// Arrow keys follow display rows before browsing history at the edges.
    fn vertical(&mut self, down: bool, width: usize) {
        let layout = self.layout(width);
        let (row, column) = layout.cursor;
        let target = if down {
            row.checked_add(1).filter(|row| *row < layout.rows.len())
        } else {
            row.checked_sub(1)
        };
        if let Some(target) = target {
            let start = layout.starts[target];
            let end = layout.starts.get(target + 1).copied().unwrap_or(self.text.len());
            self.cursor =
                start + column_offset(self.text[start..end].trim_end_matches('\n'), column, width);
        } else if !down {
            let index = match self.browsing {
                None if !self.history.is_empty() => {
                    self.draft = Some((
                        std::mem::take(&mut self.text),
                        self.cursor,
                        std::mem::take(&mut self.tokens),
                    ));
                    self.history.len() - 1
                }
                Some(index) if index > 0 => index - 1,
                _ => return,
            };
            self.text = self.history[index].clone();
            self.cursor = self.text.len();
            self.browsing = Some(index);
        } else {
            match self.browsing {
                Some(index) if index + 1 < self.history.len() => {
                    self.text = self.history[index + 1].clone();
                    self.browsing = Some(index + 1);
                }
                Some(_) => {
                    if let Some((text, cursor, tokens)) = self.draft.take() {
                        self.text = text;
                        self.cursor = cursor;
                        self.tokens = tokens;
                    }
                    self.browsing = None;
                    return;
                }
                None => return,
            }
            self.cursor = self.text.len();
        }
    }
}

/// `text` with line endings and tabs normalized and other control characters
/// dropped, since they would corrupt the display.
fn clean(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect()
}

/// Byte offset at a display column, clamped before a wide character.
fn column_offset(line: &str, column: usize, width: usize) -> usize {
    let mut used = 0;
    for (index, c) in line.char_indices() {
        used += c.width().unwrap_or(0).min(width.max(1));
        if used > column {
            return index;
        }
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::super::text::{self, plain};
    use super::*;

    fn press(editor: &mut Editor, code: KeyCode, modifiers: KeyModifiers) {
        editor.key(KeyEvent::new(code, modifiers), 80);
    }

    fn rows(layout: &Layout) -> Vec<String> {
        layout.rows.iter().map(|row| plain(row)).collect()
    }

    /// A draft's parts as text, with images as `<path>`.
    fn parts(draft: &Draft) -> Vec<String> {
        draft
            .parts
            .iter()
            .map(|part| match part {
                Part::Text(text) => text.clone(),
                Part::Image(Source::File(path)) => format!("<{}>", path.display()),
                Part::Image(Source::Clipboard(bytes)) => format!("<{} bytes>", bytes.len()),
            })
            .collect()
    }

    #[test]
    fn editing_respects_character_boundaries() {
        let mut editor = Editor::default();
        editor.insert("héllo wörld");
        press(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
        press(&mut editor, KeyCode::Left, KeyModifiers::ALT);
        press(&mut editor, KeyCode::Left, KeyModifiers::NONE);
        press(&mut editor, KeyCode::Char('!'), KeyModifiers::NONE);
        assert_eq!(editor.text(), "héllo! wörl");
        press(&mut editor, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(editor.text(), " wörl");
        press(&mut editor, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn line_endings_tabs_and_control_characters_are_cleaned() {
        let mut editor = Editor::default();
        editor.insert("a\r\nb\tc\rd");
        assert_eq!(editor.text(), "a\nb    c\nd");
        editor.clear();
        editor.paste("x\x1b[31my\u{7}");
        assert_eq!(editor.text(), "x[31my");
    }

    #[test]
    fn layout_wraps_by_display_width_and_tracks_the_cursor() {
        let mut editor = Editor::default();
        editor.insert("ab日本\nxyz");
        let layout = editor.layout(4);
        assert_eq!(rows(&layout), ["ab日", "本", "xyz"]);
        assert_eq!(layout.cursor, (2, 3));
        editor.set("abcd".into());
        let layout = editor.layout(4);
        assert_eq!(rows(&layout), ["abcd", ""]);
        assert_eq!(layout.cursor, (1, 0));
        editor.set("abcd\nxy".into());
        assert_eq!(rows(&editor.layout(4)), ["abcd", "xy"]);
    }

    #[test]
    fn vertical_moves_travel_lines_then_history() {
        let mut editor = Editor::default();
        editor.insert("first");
        editor.submit();
        editor.insert("one\ntwo");
        press(&mut editor, KeyCode::Up, KeyModifiers::NONE);
        press(&mut editor, KeyCode::Char('X'), KeyModifiers::NONE);
        assert_eq!(editor.text(), "oneX\ntwo");
        press(&mut editor, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(editor.text(), "first");
        press(&mut editor, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(editor.text(), "oneX\ntwo");
    }

    #[test]
    fn large_pastes_become_chips_until_sent() {
        let mut editor = Editor::default();
        editor.insert("see ");
        let pasted: String = (0..12).map(|n| format!("line {n}\r\n")).collect();
        editor.paste(&pasted);
        assert_eq!(editor.text(), "see [pasted 12 lines]");
        let layout = editor.layout(80);
        assert_eq!(
            text::runs(&layout.rows[0]),
            [(Style::PLAIN, "see ".into()), (Style::CODE, "[pasted 12 lines]".into())]
        );
        editor.paste("short");
        for _ in 0.."short".len() {
            press(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
        }
        assert_eq!(editor.text(), "see [pasted 12 lines]");
        assert_eq!(editor.submit().typed, format!("see {}", pasted.replace('\r', "")));

        editor.paste(&"x".repeat(3000));
        assert_eq!(editor.text(), "[pasted 3000 characters]");
        press(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(editor.text(), "");
        assert!(editor.tokens.is_empty());
    }

    #[test]
    fn chips_are_atomic_and_expand_by_position() {
        let mut editor = Editor::default();
        editor.insert("literal [pasted 3000 characters] ");
        editor.paste(&"x".repeat(3000));
        press(&mut editor, KeyCode::Left, KeyModifiers::NONE);
        editor.paste(&"y".repeat(3000));
        assert_eq!(
            editor.submit().typed,
            format!("literal [pasted 3000 characters] {}{}", "y".repeat(3000), "x".repeat(3000))
        );

        editor.paste(&"x".repeat(3000));
        press(&mut editor, KeyCode::Home, KeyModifiers::NONE);
        press(&mut editor, KeyCode::Delete, KeyModifiers::NONE);
        assert!(editor.text().is_empty() && editor.tokens.is_empty());
        editor.paste(&"x".repeat(3000));
        press(&mut editor, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert!(editor.text().is_empty() && editor.tokens.is_empty());
    }

    #[test]
    fn images_split_a_message_into_parts_in_order() {
        let mut editor = Editor::default();
        editor.insert("compare");
        editor.attach("a.png", Source::File(PathBuf::from("/tmp/a.png")));
        editor.insert("with");
        editor.attach("clipboard image", Source::Clipboard(vec![1, 2, 3]));
        assert_eq!(editor.text(), "compare [a.png] with [clipboard image] ");
        let draft = editor.submit();
        assert!(draft.has_images());
        assert_eq!(draft.typed, "compare  with  ", "the images are parts, not text");
        assert_eq!(parts(&draft), ["compare ", "</tmp/a.png>", " with ", "<3 bytes>", " "]);
        press(&mut editor, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(editor.text(), "compare  with  ", "history keeps the text without images");

        editor.clear();
        editor.attach("a.png", Source::File(PathBuf::from("/tmp/a.png")));
        press(&mut editor, KeyCode::Left, KeyModifiers::NONE);
        press(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(editor.text(), " ");
        assert!(!editor.submit().has_images());
    }

    #[test]
    fn replacing_a_range_leaves_the_cursor_after_the_new_text() {
        let mut editor = Editor::default();
        editor.insert("read @src/tu now");
        editor.cursor = "read @src/tu".len();
        editor.replace(5..12, "src/tui.rs");
        assert_eq!(editor.text(), "read src/tui.rs now");
        assert_eq!(editor.cursor(), "read src/tui.rs".len());
    }

    #[test]
    fn history_restores_a_draft_with_its_chips() {
        let mut editor = Editor::default();
        editor.insert("earlier");
        editor.submit();
        editor.paste(&"x".repeat(3000));
        press(&mut editor, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(editor.text(), "earlier");
        assert!(editor.tokens.is_empty());
        press(&mut editor, KeyCode::Down, KeyModifiers::NONE);
        assert!(editor.text().starts_with("[pasted"));
        assert_eq!(editor.submit().typed, "x".repeat(3000));
        assert!(editor.draft.is_none());
    }

    #[test]
    fn history_is_bounded_by_count_and_bytes() {
        let mut editor = Editor::default();
        for n in 0..200 {
            editor.set(format!("{n}"));
            editor.submit();
        }
        assert_eq!(editor.history.len(), HISTORY_ENTRIES);
        assert_eq!(editor.history[0], "100");
        for n in 0..30 {
            editor.set(format!("{n}{}", "x".repeat(20_000)));
            editor.submit();
        }
        assert!(editor.history.iter().map(String::len).sum::<usize>() <= HISTORY_BYTES);
        editor.paste(&"z".repeat(HISTORY_BYTES + 1));
        assert_eq!(editor.submit().typed.len(), HISTORY_BYTES + 1, "sending never clips input");
        assert!(editor.history.iter().map(String::len).sum::<usize>() <= HISTORY_BYTES);
    }

    #[test]
    fn arrows_follow_wrapped_rows_and_display_columns() {
        let mut editor = Editor::default();
        editor.insert("ab日本cd");
        editor.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), 4);
        assert_eq!(editor.cursor, 5);
        editor.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), 4);
        assert_eq!(editor.cursor, 0);
        editor.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), 4);
        assert_eq!(editor.cursor, 5);
        editor.set("界\na".into());
        editor.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), 4);
        assert_eq!(editor.cursor, 0, "one column cannot split a wide character");
        assert!(editor.layout(1).rows.iter().all(|row| text::width(row) <= 1));
    }

    #[test]
    fn prepended_text_goes_before_the_draft_and_keeps_its_chips() {
        let mut editor = Editor::default();
        editor.prepend("waiting");
        assert_eq!((editor.text(), editor.cursor), ("waiting", 7));
        editor.clear();
        editor.insert("draft ");
        editor.paste(&"x".repeat(3000));
        let cursor = editor.cursor;
        editor.prepend("first\nsecond");
        assert_eq!(editor.text(), "first\nsecond\ndraft [pasted 3000 characters]");
        assert_eq!(editor.cursor, cursor + "first\nsecond\n".len());
        assert_eq!(editor.submit().typed, format!("first\nsecond\ndraft {}", "x".repeat(3000)));
    }
}
