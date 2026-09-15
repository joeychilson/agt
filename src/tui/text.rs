//! Styled, wrapped text for the terminal UI.
//!
//! Text becomes rows of styled characters that the screen places in its
//! cells. A row records what copying it needs: the columns of layout before
//! its text, and how it follows the row above.

use std::mem;
use std::ops::Range;

use unicode_width::UnicodeWidthChar;

/// A color of the terminal's 16-color palette, so the UI follows the
/// terminal's own light or dark theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Color {
    Default,
    Red,
    Green,
    Yellow,
    Cyan,
}

impl Color {
    /// The SGR parameter that selects the color for text.
    fn code(self) -> Option<u8> {
        match self {
            Self::Default => None,
            Self::Red => Some(31),
            Self::Green => Some(32),
            Self::Yellow => Some(33),
            Self::Cyan => Some(36),
        }
    }
}

/// A way text is drawn besides its color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Attribute {
    Bold,
    Dim,
    Italic,
    Underline,
    Reverse,
    Strike,
}

impl Attribute {
    /// Every attribute, in the order escape sequences select them.
    const ALL: [Self; 6] =
        [Self::Bold, Self::Dim, Self::Italic, Self::Underline, Self::Reverse, Self::Strike];

    /// The SGR parameter that selects the attribute.
    fn parameter(self) -> &'static [u8] {
        match self {
            Self::Bold => b";1",
            Self::Dim => b";2",
            Self::Italic => b";3",
            Self::Underline => b";4",
            Self::Reverse => b";7",
            Self::Strike => b";9",
        }
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// How text is drawn: a color and attributes, and the link a click on it
/// opens. Every character laid out carries one, so it is kept to four bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Style {
    color: Color,
    /// The bits of the attributes.
    attributes: u8,
    /// One more than the index of the link the text opens, or zero.
    link: u16,
}

impl Style {
    pub(crate) const PLAIN: Self = Self { color: Color::Default, attributes: 0, link: 0 };
    pub(crate) const BOLD: Self = Self::PLAIN.with(Attribute::Bold);
    /// Secondary text.
    pub(crate) const DIM: Self = Self::PLAIN.with(Attribute::Dim);
    /// The model's reasoning.
    pub(crate) const THINKING: Self = Self::DIM.with(Attribute::Italic);
    pub(crate) const CODE: Self = Self::PLAIN.in_color(Color::Cyan);
    /// The user's marks and what has focus.
    pub(crate) const ACCENT: Self = Self::CODE.with(Attribute::Bold);
    pub(crate) const RED: Self = Self::PLAIN.in_color(Color::Red);
    pub(crate) const GREEN: Self = Self::PLAIN.in_color(Color::Green);
    pub(crate) const YELLOW: Self = Self::PLAIN.in_color(Color::Yellow);

    pub(crate) const fn with(self, attribute: Attribute) -> Self {
        Self { attributes: self.attributes | attribute.bit(), ..self }
    }

    pub(crate) const fn in_color(self, color: Color) -> Self {
        Self { color, ..self }
    }

    /// The style in reverse video, as the selected row of a list is drawn.
    pub(crate) const fn reversed(self) -> Self {
        self.with(Attribute::Reverse)
    }

    /// The style of text that opens link `index`, underlined. Past the links
    /// a style can number, the text opens nothing and looks as it did.
    pub(crate) fn linked(self, index: usize) -> Self {
        match index.checked_add(1).map(u16::try_from) {
            Some(Ok(link)) => Self { link, ..self.with(Attribute::Underline) },
            _ => self,
        }
    }

    /// The index of the link the text opens.
    pub(crate) fn link(self) -> Option<usize> {
        usize::from(self.link).checked_sub(1)
    }

    fn has(self, attribute: Attribute) -> bool {
        self.attributes & attribute.bit() != 0
    }

    /// Whether a space in this style looks different from a plain space.
    pub(crate) fn shows_on_space(self) -> bool {
        [Attribute::Underline, Attribute::Reverse, Attribute::Strike]
            .into_iter()
            .any(|attribute| self.has(attribute))
    }

    /// Appends the escape sequence that selects this style, whatever was
    /// selected before.
    pub(crate) fn select(self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[0");
        for attribute in Attribute::ALL {
            if self.has(attribute) {
                out.extend_from_slice(attribute.parameter());
            }
        }
        if let Some(code) = self.color.code() {
            out.extend_from_slice(&[b';', b'0' + code / 10, b'0' + code % 10]);
        }
        out.push(b'm');
    }
}

/// A character in a style. Characters of width zero never appear in a line,
/// so a line's glyphs fill exactly the columns their widths add up to.
pub(crate) type Glyph = (Style, char);

/// A row of text.
pub(crate) type Line = Vec<Glyph>;

/// A row of laid-out text, with what copying it needs: the columns of layout
/// before its text, such as an indent, a gutter or a mark, and how it follows
/// the row above.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Row {
    pub(crate) line: Line,
    pub(crate) indent: usize,
    pub(crate) joins: Joins,
}

/// How copied text goes from the row above to a row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Joins {
    /// The row starts a line of its own.
    Line,
    /// The row continues a line its wrap broke, taking out this many spaces.
    Wrap(usize),
    /// The row is a table's. It starts a line of its own, and its borders copy
    /// as Markdown's, so a copied table is still one.
    Table,
    /// The row is only layout, such as a rule, and copying leaves it out.
    Layout,
}

impl From<Line> for Row {
    /// A row of text from its first column.
    fn from(line: Line) -> Self {
        Self { line, indent: 0, joins: Joins::Line }
    }
}

impl Row {
    /// A row that is only layout.
    pub(crate) fn layout(line: Line) -> Self {
        Self { line, indent: 0, joins: Joins::Layout }
    }

    /// The row after `columns` plain spaces of layout.
    pub(crate) fn indent(mut self, columns: usize) -> Self {
        self.line = indent(self.line, columns);
        self.indent += columns;
        self
    }

    /// The row after `mark`, such as a gutter, which is layout.
    pub(crate) fn after(mut self, mark: impl IntoIterator<Item = Glyph>) -> Self {
        let mut line: Line = mark.into_iter().collect();
        self.indent += width(&line);
        line.append(&mut self.line);
        self.line = line;
        self
    }

    /// The row's text in `columns`, without its layout or trailing spaces.
    pub(crate) fn text(&self, columns: Range<usize>) -> String {
        let from = columns.start.max(self.indent);
        let mut text = String::new();
        let mut at = 0;
        for glyph in &self.line {
            if at >= columns.end {
                break;
            }
            if at >= from {
                text.push(match (self.joins, glyph.1) {
                    (Joins::Table, '│' | '├' | '┼' | '┤') => '|',
                    (Joins::Table, '─') => '-',
                    (_, c) => c,
                });
            }
            at += glyph_width(glyph);
        }
        text.truncate(text.trim_end().len());
        text
    }

    /// The style of the character at `column`.
    pub(crate) fn style_at(&self, column: usize) -> Option<Style> {
        let mut end = 0;
        let glyph = self.line.iter().find(|glyph| {
            end += glyph_width(glyph);
            end > column
        });
        glyph.map(|glyph| glyph.0)
    }
}

/// Wraps plain text in one style, indenting the rows a wrap starts by `hang`.
pub(crate) fn wrap_text(text: &str, style: Style, width: usize, hang: usize) -> Vec<Row> {
    let hang = vec![(Style::PLAIN, ' '); hang];
    text.split('\n')
        .flat_map(|line| wrap(&printable(line, style).collect::<Line>(), width, &[], &hang))
        .collect()
}

/// Styled pieces of one row, cut with an ellipsis to fit `width` columns.
pub(crate) fn fit(spans: &[(&str, Style)], width: usize) -> Line {
    let mut line: Line = spans.iter().flat_map(|&(text, style)| glyphs(text, style)).collect();
    truncate(&mut line, width);
    line
}

/// Cuts `line` to `width` columns, ending it with an ellipsis when anything
/// was cut.
pub(crate) fn truncate(line: &mut Line, width: usize) {
    if self::width(line) <= width {
        return;
    }
    let mut used = 0;
    let keep = line
        .iter()
        .take_while(|glyph| {
            used += glyph_width(glyph);
            used < width
        })
        .count();
    let style = line[keep].0;
    line.truncate(keep);
    if width > 0 {
        line.push((style, '…'));
    }
}

/// Pads `line` with spaces in `style` to `width` columns.
pub(crate) fn pad(line: &mut Line, width: usize, style: Style) {
    let used = self::width(line);
    line.extend(std::iter::repeat_n((style, ' '), width.saturating_sub(used)));
}

/// `line` after `columns` plain spaces.
pub(crate) fn indent(line: Line, columns: usize) -> Line {
    if line.is_empty() || columns == 0 {
        return line;
    }
    std::iter::repeat_n((Style::PLAIN, ' '), columns).chain(line).collect()
}

/// The columns a line fills.
pub(crate) fn width(line: &[Glyph]) -> usize {
    line.iter().map(glyph_width).sum()
}

/// The columns text fills once it is a line.
pub(crate) fn str_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(1)).sum()
}

/// The characters of `text` in `style`: control characters become spaces, and
/// characters of width zero are left out.
pub(crate) fn glyphs(text: &str, style: Style) -> impl Iterator<Item = Glyph> + '_ {
    text.chars().filter_map(move |c| match c.width() {
        None => Some((style, ' ')),
        Some(0) => None,
        Some(_) => Some((style, c)),
    })
}

/// The characters of `text` in `style` with tabs expanded, carriage returns
/// and characters of width zero left out, and other control characters made
/// visible, since a line from a model or a process may contain anything.
pub(crate) fn printable(text: &str, style: Style) -> impl Iterator<Item = Glyph> + '_ {
    text.chars().flat_map(move |c| {
        let (c, count) = match c {
            '\t' => (' ', 4),
            '\r' => (c, 0),
            c if c.is_control() => ('\u{fffd}', 1),
            c => (c, usize::from(c.width() != Some(0))),
        };
        std::iter::repeat_n((style, c), count)
    })
}

/// Greedy word wrap by display width into rows at most `width` columns wide.
/// The first row starts with `first` and the rows a wrap starts with `rest`,
/// both layout, cut to leave a column for text. Words longer than a row are
/// split, and a row a wrap starts records the spaces the wrap took out.
pub(crate) fn wrap(glyphs: &[Glyph], width: usize, first: &[Glyph], rest: &[Glyph]) -> Vec<Row> {
    let width = width.max(1);
    let first = within(first, width - 1);
    let rest = within(rest, width - 1);
    let mut rows =
        vec![Row { line: first.to_vec(), indent: self::width(first), joins: Joins::Line }];
    // The column the text of the row being filled starts at, the column it
    // has reached, and the spaces left out at its end.
    let (mut start, mut col, mut skipped) = (rows[0].indent, rows[0].indent, 0);
    let mut word = 0;
    while word < glyphs.len() {
        let word_end = glyphs[word..]
            .iter()
            .position(|glyph| glyph.1 == ' ')
            .map_or(glyphs.len(), |offset| word + offset);
        let end = glyphs[word_end..]
            .iter()
            .position(|glyph| glyph.1 != ' ')
            .map_or(glyphs.len(), |offset| word_end + offset);
        if col > start && col + self::width(&glyphs[word..word_end]) > width {
            start = break_row(&mut rows, first.len(), rest, &mut skipped);
            col = start;
        }
        for &glyph in &glyphs[word..end] {
            let glyph =
                if glyph_width(&glyph) > width - start { (glyph.0, '\u{fffd}') } else { glyph };
            let char_width = glyph_width(&glyph);
            if col + char_width > width && col > start {
                if glyph.1 == ' ' {
                    skipped += 1;
                    continue;
                }
                start = break_row(&mut rows, first.len(), rest, &mut skipped);
                col = start;
            }
            rows.last_mut().expect("rows start non-empty").line.push(glyph);
            col += char_width;
        }
        word = end;
    }
    for row in &mut rows {
        let len = row.line.iter().rposition(|glyph| glyph.1 != ' ').map_or(0, |last| last + 1);
        row.line.truncate(len);
        // Rows are kept while they are shown, so they hold no spare room.
        row.line.shrink_to_fit();
    }
    rows
}

/// Starts a row after `rest`, recording the spaces the wrap took out: those
/// ending the text of the row above, which starts after `first` glyphs of
/// layout when it is the first row, and `skipped`. Returns the column the new
/// row's text starts at.
fn break_row(rows: &mut Vec<Row>, first: usize, rest: &[Glyph], skipped: &mut usize) -> usize {
    let layout = if rows.len() == 1 { first } else { rest.len() };
    let above = &rows[rows.len() - 1].line[layout..];
    let spaces = above.iter().rev().take_while(|glyph| glyph.1 == ' ').count() + mem::take(skipped);
    let indent = width(rest);
    rows.push(Row { line: rest.to_vec(), indent, joins: Joins::Wrap(spaces) });
    indent
}

/// The glyphs `line` starts with that fit in `columns`.
fn within(line: &[Glyph], columns: usize) -> &[Glyph] {
    let mut used = 0;
    let count = line
        .iter()
        .take_while(|glyph| {
            used += glyph_width(glyph);
            used <= columns
        })
        .count();
    &line[..count]
}

fn glyph_width(glyph: &Glyph) -> usize {
    glyph.1.width().unwrap_or(0)
}

/// A line's characters without their styles.
#[cfg(test)]
pub(crate) fn plain(line: &[Glyph]) -> String {
    line.iter().map(|glyph| glyph.1).collect()
}

/// A line as runs of one style each.
#[cfg(test)]
pub(crate) fn runs(line: &[Glyph]) -> Vec<(Style, String)> {
    let mut runs: Vec<(Style, String)> = Vec::new();
    for &(style, c) in line {
        match runs.last_mut() {
            Some((last, text)) if *last == style => text.push(c),
            _ => runs.push((style, c.to_string())),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_rows(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|row| plain(&row.line)).collect()
    }

    #[test]
    fn styles_select_their_attributes_from_any_other() {
        let select = |style: Style| {
            let mut out = Vec::new();
            style.select(&mut out);
            String::from_utf8(out).expect("ASCII")
        };
        assert_eq!(select(Style::PLAIN), "\x1b[0m");
        assert_eq!(select(Style::ACCENT), "\x1b[0;1;36m");
        assert_eq!(select(Style::THINKING), "\x1b[0;2;3m");
        assert_eq!(select(Style::DIM.reversed()), "\x1b[0;2;7m");
        assert_eq!(select(Style::CODE.with(Attribute::Strike).linked(0)), "\x1b[0;4;9;36m");
        assert!(Style::PLAIN.reversed().shows_on_space() && !Style::RED.shows_on_space());
        assert!(Style::PLAIN.linked(0).shows_on_space(), "a link's spaces are underlined");
        assert_eq!(
            (Style::BOLD.linked(2).link(), Style::BOLD.link(), Style::BOLD.linked(70_000)),
            (Some(2), None, Style::BOLD),
            "past the links a style numbers, text opens nothing"
        );
    }

    #[test]
    fn wraps_words_after_the_marks_rows_start_with() {
        let rows = wrap_text("alpha beta gamma delta", Style::PLAIN, 12, 2);
        assert_eq!(plain_rows(&rows), ["alpha beta", "  gamma", "  delta"]);
        let rows = wrap_text("supercalifragilistic", Style::PLAIN, 8, 0);
        assert_eq!(plain_rows(&rows), ["supercal", "ifragili", "stic"]);
        assert!(rows.iter().all(|row| width(&row.line) <= 8));

        let text: Line = glyphs("one two three", Style::PLAIN).collect();
        let bar = [(Style::DIM, '│'), (Style::PLAIN, ' ')];
        let rows = wrap(&text, 9, &[(Style::DIM, '•'), (Style::PLAIN, ' ')], &bar);
        assert_eq!(plain_rows(&rows), ["• one two", "│ three"]);
        assert_eq!((rows[1].indent, rows[1].joins), (2, Joins::Wrap(1)));
        for width in [1, 2, 3] {
            let rows = wrap(&text, width, &bar, &bar);
            assert!(rows.iter().all(|row| self::width(&row.line) <= width), "{rows:?}");
        }
    }

    #[test]
    fn wrapped_rows_record_their_layout_and_the_spaces_they_took_out() {
        let rows = wrap_text("alpha beta  gamma", Style::PLAIN, 10, 2);
        assert_eq!(plain_rows(&rows), ["alpha beta", "  gamma"]);
        assert_eq!((rows[1].indent, rows[1].joins), (2, Joins::Wrap(2)));
        assert_eq!(
            (rows[1].text(0..usize::MAX), rows[0].text(6..8)),
            ("gamma".into(), "be".into())
        );
        let split = wrap_text("abcdefghij", Style::PLAIN, 4, 0);
        let joins: Vec<Joins> = split.iter().map(|row| row.joins).collect();
        assert_eq!(joins, [Joins::Line, Joins::Wrap(0), Joins::Wrap(0)], "a word split in two");
        let marked =
            Row::from(fit(&[("text", Style::PLAIN)], 10)).after([(Style::DIM, '│')]).indent(2);
        assert_eq!((plain(&marked.line), marked.indent), ("  │text".into(), 3));
        assert_eq!(
            (marked.style_at(2), marked.style_at(3), marked.style_at(7)),
            (Some(Style::DIM), Some(Style::PLAIN), None)
        );
    }

    #[test]
    fn wide_characters_count_double() {
        let rows = wrap_text("日本語テキスト", Style::PLAIN, 6, 0);
        assert_eq!(plain_rows(&rows), ["日本語", "テキス", "ト"]);
        assert_eq!(plain(&fit(&[("abcdef", Style::PLAIN)], 4)), "abc…");
        assert_eq!(plain(&fit(&[("abcd", Style::PLAIN)], 4)), "abcd");
        assert_eq!(plain(&fit(&[("日本語", Style::PLAIN)], 5)), "日本…");
        assert_eq!(plain(&fit(&[("日日", Style::PLAIN)], 2)), "…");
    }

    #[test]
    fn spans_share_one_row() {
        let row = fit(&[("$ ", Style::RED), ("cargo test --workspace", Style::PLAIN)], 10);
        assert_eq!(runs(&row), [(Style::RED, "$ ".into()), (Style::PLAIN, "cargo t…".into())]);
        assert_eq!(width(&row), 10);
        assert!(fit(&[("", Style::DIM)], 10).is_empty());
        let mut padded = fit(&[("ab", Style::PLAIN)], 10);
        pad(&mut padded, 4, Style::PLAIN.reversed());
        assert_eq!(
            runs(&padded),
            [(Style::PLAIN, "ab".into()), (Style::PLAIN.reversed(), "  ".into())]
        );
    }

    #[test]
    fn control_and_zero_width_characters_never_reach_the_screen() {
        let rows = wrap_text("a\r\x1b[2Jb\tc", Style::PLAIN, 20, 0);
        assert_eq!(plain_rows(&rows), ["a\u{fffd}[2Jb    c"]);
        assert_eq!(plain_rows(&wrap_text("one\ntwo", Style::PLAIN, 20, 0)), ["one", "two"]);
        assert_eq!(plain(&fit(&[("x\x07y", Style::PLAIN)], 10)), "x y");
        let combining = fit(&[("e\u{301}\u{200b}!", Style::PLAIN)], 10);
        assert_eq!((plain(&combining), width(&combining)), ("e!".to_owned(), 2));
    }
}
