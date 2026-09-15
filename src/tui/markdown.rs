//! Markdown as the transcript shows it: CommonMark with GitHub's tables,
//! strikethrough and task lists, laid out in styled rows.
//!
//! The source's line breaks are kept, as chat interfaces keep them, since
//! models write lines such as `**Status:** passed` one under another. A blank
//! row separates blocks where the source has a blank line between them. Links
//! to web addresses, and web addresses in text, are underlined, and their
//! styles number the addresses a click on them opens.
//!
//! Replies stream in, so source is laid out in chunks. A chunk ends before a
//! line that starts in the first column after a blank line, outside a code
//! fence, since no line after that one changes how the lines before it read.
//! A finished chunk keeps its rows, and as source arrives only the last chunk
//! is laid out again. Until the source is complete, its end shows as it will
//! read once its line is: a line that is only a marker so far, or a table's
//! row, waits for the rest of it, and open code, strong emphasis,
//! strikethrough and link addresses are closed.

use std::borrow::Cow;
use std::mem;
use std::ops::Range;

use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
    TextMergeWithOffset,
};

use super::text::{
    self, Attribute, Color, Glyph, Joins, Line, Row, Style, glyphs, printable, wrap,
};

const OPTIONS: Options =
    Options::ENABLE_TABLES.union(Options::ENABLE_STRIKETHROUGH).union(Options::ENABLE_TASKLISTS);
/// The widest a rule is drawn, above code or across a thematic break.
const RULE_WIDTH: usize = 60;
/// The bullets of lists nested in one another, from the outermost.
const BULLETS: [char; 3] = ['•', '◦', '▪'];
/// In a table too wide for the columns there are, a column shrinks to its
/// longest word, but to no fewer columns than this unless its text is
/// narrower.
const COLUMN_LEAST: usize = 6;
/// A word longer than this is split to shrink its column further.
const COLUMN_WORD: usize = 20;

/// Where Markdown is laid out: in `width` columns after `indent` columns, with
/// plain text in `style`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    pub(crate) indent: usize,
    pub(crate) width: usize,
    pub(crate) style: Style,
}

/// Appends the rows `source` lays out to `rows`, and the addresses its links
/// open to `links`.
pub(crate) fn render(source: &str, layout: Layout, rows: &mut Vec<Row>, links: &mut Vec<String>) {
    let mut scanner = Scanner::default();
    let mut start = 0;
    while let Some(end) = scanner.chunk_end(source, start) {
        chunk(&source[start..end], layout, start > 0, rows, links);
        start = end;
    }
    chunk(&source[start..], layout, start > 0, rows, links);
}

/// Markdown laid out as it streams in.
#[derive(Default)]
pub(crate) struct Stream {
    /// The layout the rows are for.
    layout: Option<Layout>,
    /// Where the finished chunks end in the source, and the rows and links
    /// they made.
    settled: usize,
    rows: usize,
    links: usize,
    scanner: Scanner,
    /// The source's length when last laid out, and whether it was complete.
    laid: Option<(usize, bool)>,
}

impl Stream {
    /// Brings `rows` and `links` up to date with `source`, which is complete
    /// once `done`. Nothing is laid out again unless the source or the layout
    /// changed, and then only the chunks that could have.
    pub(crate) fn lay_out(
        &mut self,
        source: &str,
        done: bool,
        layout: Layout,
        rows: &mut Vec<Row>,
        links: &mut Vec<String>,
    ) {
        if self.layout != Some(layout) || source.len() < self.settled {
            *self = Self { layout: Some(layout), ..Self::default() };
            rows.clear();
            links.clear();
        }
        if self.laid == Some((source.len(), done)) {
            return;
        }
        self.laid = Some((source.len(), done));
        rows.truncate(self.rows);
        links.truncate(self.links);
        while let Some(end) = self.scanner.chunk_end(source, self.settled) {
            chunk(&source[self.settled..end], layout, self.settled > 0, rows, links);
            (self.settled, self.rows, self.links) = (end, rows.len(), links.len());
        }
        let rest = &source[self.settled..];
        let rest =
            if done { Cow::Borrowed(rest) } else { unfinished(rest, self.scanner.fence.is_some()) };
        chunk(&rest, layout, self.settled > 0, rows, links);
    }
}

/// Reads source a complete line at a time for where its chunks end.
#[derive(Clone, Copy, Default)]
struct Scanner {
    /// The end of the complete lines read.
    read: usize,
    /// The code fence open after them, as its marker and length.
    fence: Option<(u8, usize)>,
    /// Whether the last of them was blank.
    blank: bool,
}

impl Scanner {
    /// Where the chunk starting at `start` ends, once a line after it shows
    /// that it has.
    fn chunk_end(&mut self, source: &str, start: usize) -> Option<usize> {
        // Completing the source may trim the blank lines read at its end.
        self.read = self.read.min(source.len());
        while let Some(length) = source[self.read..].find('\n') {
            let at = self.read;
            let line = &source[at..at + length];
            let ends = self.ends_before(line) && at > start;
            self.fence = fence_after(line, self.fence);
            self.blank = line.trim().is_empty();
            self.read = at + length + 1;
            if ends {
                return Some(at);
            }
        }
        // A line still arriving ends the chunk as soon as its first
        // character does.
        (self.ends_before(&source[self.read..]) && self.read > start).then_some(self.read)
    }

    /// Whether a chunk ends before `line`, the line after those read.
    fn ends_before(self, line: &str) -> bool {
        self.blank && self.fence.is_none() && line.starts_with(|c: char| !c.is_whitespace())
    }
}

/// The code fence open after `line`, given the one open before it.
fn fence_after(line: &str, open: Option<(u8, usize)>) -> Option<(u8, usize)> {
    let line = line.trim_start();
    let marker = match line.bytes().next() {
        Some(marker @ (b'`' | b'~')) => marker,
        _ => return open,
    };
    let length = line.bytes().take_while(|&byte| byte == marker).count();
    let rest = &line[length..];
    match open {
        _ if length < 3 => open,
        Some((active, count)) if marker == active && length >= count && rest.trim().is_empty() => {
            None
        }
        Some(_) => open,
        // A backtick fence's info string has no backticks, so "```a```" is code
        // in a line of text.
        None if marker == b'`' && rest.contains('`') => None,
        None => Some((marker, length)),
    }
}

/// Streamed `source` as it will read once the line it ends with is complete.
/// `fenced` says the line is in a code block, where only a fence waits.
fn unfinished(source: &str, fenced: bool) -> Cow<'_, str> {
    let start = source.rfind('\n').map_or(0, |at| at + 1);
    let content = content(&source[start..]);
    let fence = content.starts_with("```") || content.starts_with("~~~");
    if fenced {
        return Cow::Borrowed(if fence { &source[..start] } else { source });
    }
    let marker = content.chars().all(|c| c.is_ascii_digit() || " \t#*+-=_.)`~".contains(c));
    if fence || marker || content.starts_with('|') {
        return Cow::Borrowed(&source[..without_first_table_row(source, start)]);
    }
    let (end, closers) = closers(&source[start..]);
    if closers.is_empty() {
        Cow::Borrowed(&source[..start + end])
    } else {
        Cow::Owned([&source[..start + end], &closers].concat())
    }
}

/// Where the complete lines of `source` before `end` end, less a table's
/// first row, which waits for the row under it that makes it a table.
fn without_first_table_row(source: &str, end: usize) -> usize {
    let row = |line: &str| content(line).starts_with('|');
    let lines = source[..end].strip_suffix('\n').unwrap_or(&source[..end]);
    let last = lines.rfind('\n').map_or(0, |at| at + 1);
    let above = lines[..last.saturating_sub(1)].rsplit('\n').next().unwrap_or_default();
    if row(&lines[last..]) && !(last > 0 && row(above)) { last } else { end }
}

/// Where `line` ends without its trailing spaces and any opener with nothing
/// after it, and what closes the code, link address, strikethrough and strong
/// emphasis it leaves open, innermost first.
fn closers(line: &str) -> (usize, String) {
    // Each opener's position, length and closer.
    let mut open: Vec<(usize, usize, &str)> = Vec::new();
    let mut at = 0;
    while let Some(c) = line[at..].chars().next() {
        let rest = &line[at..];
        if c == '`' {
            let ticks = &rest[..rest.bytes().take_while(|&byte| byte == b'`').count()];
            match rest[ticks.len()..].find(ticks) {
                Some(offset) => at += 2 * ticks.len() + offset,
                None => {
                    open.push((at, ticks.len(), ticks));
                    break;
                }
            }
        } else if let Some(marker) =
            ["**", "~~"].into_iter().find(|&marker| rest.starts_with(marker))
        {
            match open.iter().rposition(|opener| opener.2 == marker) {
                Some(index) => {
                    open.remove(index);
                }
                None => open.push((at, 2, marker)),
            }
            at += 2;
        } else if rest.starts_with("](") {
            open.push((at, 2, ")"));
            at += 2;
        } else {
            if c == ')'
                && let Some(index) = open.iter().rposition(|opener| opener.2 == ")")
            {
                open.remove(index);
            }
            at += c.len_utf8();
        }
    }
    let mut end = line.trim_end().len();
    while let Some(&(at, length, _)) = open.last()
        && at + length >= end
    {
        open.pop();
        end = line[..at].trim_end().len();
    }
    (end, open.iter().rev().map(|opener| opener.2).collect())
}

/// Lays out a chunk of source, after a blank row when `after` another.
fn chunk(source: &str, layout: Layout, after: bool, rows: &mut Vec<Row>, links: &mut Vec<String>) {
    let mut renderer = Renderer {
        source,
        layout,
        rows,
        links,
        containers: Vec::new(),
        lists: Vec::new(),
        styles: Vec::new(),
        line: Vec::new(),
        end: None,
        gap: after.then_some(0),
        in_code: false,
        table: None,
    };
    let events = Parser::new_ext(source, OPTIONS).into_offset_iter();
    for (event, range) in TextMergeWithOffset::new(events) {
        renderer.event(event, range);
    }
}

/// Lays out a chunk's events in rows.
struct Renderer<'a> {
    source: &'a str,
    layout: Layout,
    rows: &'a mut Vec<Row>,
    links: &'a mut Vec<String>,
    /// The quotes and list items the text is in, outermost first.
    containers: Vec<Container>,
    /// The lists open, innermost last: the number of the next item, or
    /// `None` for bullets.
    lists: Vec<Option<u64>>,
    /// The styles of the spans open, innermost last.
    styles: Vec<Style>,
    /// The text of the line being built.
    line: Line,
    /// Where the source of the last text or block ended, until a block after
    /// it decides whether a blank row separates them.
    end: Option<usize>,
    /// When a blank row goes before the next row, how many of the containers
    /// it is in: those open before the block after it began.
    gap: Option<usize>,
    in_code: bool,
    table: Option<Table>,
}

/// A quote or list item that text is in, which marks the rows it takes.
enum Container {
    Quote,
    /// A list item: the columns its marker takes, and the marker until the
    /// item's first row shows it.
    Item {
        width: usize,
        marker: Option<Line>,
    },
}

impl Container {
    fn width(&self) -> usize {
        match self {
            Self::Quote => 2,
            Self::Item { width, .. } => *width,
        }
    }
}

/// A table cell's text, as the lines its breaks make.
type Cell = Vec<Line>;

/// A table being read: its columns' alignments, its rows of cells with the
/// header first, and the cell being read.
struct Table {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<Cell>>,
    cell: Cell,
}

impl Renderer<'_> {
    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
        match event {
            Event::Start(tag) => return self.start(tag, range.start),
            Event::End(tag) => self.end(tag, &range),
            Event::Text(text) if self.in_code => self.code(&text),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.push(&code, self.style().in_color(Color::Cyan)),
            Event::InlineHtml(html) if is_break(&html) => self.break_line(),
            Event::Html(html) | Event::InlineHtml(html) => {
                for (index, line) in html.split('\n').enumerate() {
                    if index > 0 {
                        self.break_line();
                    }
                    self.push(line, self.style());
                }
            }
            Event::SoftBreak | Event::HardBreak => self.break_line(),
            Event::Rule => {
                self.block(range.start);
                self.rule("");
            }
            Event::TaskListMarker(done) => self.check(done),
            Event::FootnoteReference(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.push(&text, self.style());
            }
        }
        self.end = Some(range.end);
    }

    fn start(&mut self, tag: Tag<'_>, start: usize) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => self.block(start),
            Tag::Heading { level, .. } => {
                self.block(start);
                let style = self.style().with(Attribute::Bold);
                let minor = !matches!(level, HeadingLevel::H1 | HeadingLevel::H2);
                self.styles.push(if minor { style.with(Attribute::Italic) } else { style });
            }
            Tag::BlockQuote(_) => {
                self.block(start);
                self.containers.push(Container::Quote);
                self.span(Attribute::Dim);
            }
            Tag::CodeBlock(kind) => {
                self.block(start);
                let label = match &kind {
                    CodeBlockKind::Fenced(info) => info.split([' ', ',', '{']).next(),
                    CodeBlockKind::Indented => None,
                };
                self.rule(label.unwrap_or_default());
                self.in_code = true;
            }
            Tag::List(first) => {
                self.block(start);
                self.lists.push(first);
            }
            Tag::Item => {
                self.block(start);
                let depth = self.lists.iter().filter(|list| list.is_none()).count();
                let marker = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        *number += 1;
                        format!("{}. ", *number - 1)
                    }
                    _ => format!("{} ", BULLETS[depth.saturating_sub(1) % BULLETS.len()]),
                };
                let marker: Line = glyphs(&marker, self.style().with(Attribute::Dim)).collect();
                let width = text::width(&marker);
                self.containers.push(Container::Item { width, marker: Some(marker) });
            }
            Tag::Table(alignments) => {
                self.block(start);
                self.table = Some(Table { alignments, rows: Vec::new(), cell: Vec::new() });
            }
            Tag::TableHead => {
                self.table_row();
                self.span(Attribute::Bold);
            }
            Tag::TableRow => self.table_row(),
            Tag::Emphasis => self.span(Attribute::Italic),
            Tag::Strong => self.span(Attribute::Bold),
            Tag::Strikethrough => self.span(Attribute::Strike),
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                let style = self.style();
                let style = if is_web(&dest_url) {
                    self.link(dest_url.into_string(), style)
                } else {
                    style
                };
                self.styles.push(style);
            }
            // A cell's text is taken when it ends. The rest belong to
            // extensions left off.
            Tag::TableCell
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd, range: &Range<usize>) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.containers.pop();
                self.styles.pop();
            }
            TagEnd::CodeBlock => {
                self.flush();
                self.in_code = false;
                // A fence still streaming has no closing rule yet.
                if closed(&self.source[range.clone()]) {
                    self.rule("");
                }
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush();
                // An item with nothing in it still shows its marker.
                if let Some(Container::Item { marker: Some(_), .. }) = self.containers.last() {
                    self.push_rows(&[], 0);
                }
                self.containers.pop();
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    table.cell.push(mem::take(&mut self.line));
                    let cell = mem::take(&mut table.cell);
                    if let Some(row) = table.rows.last_mut() {
                        row.push(cell);
                    }
                }
            }
            TagEnd::TableHead
            | TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Link
            | TagEnd::Image => {
                self.styles.pop();
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.table(table);
                }
            }
            TagEnd::TableRow
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    /// The style plain text in the spans open has.
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or(self.layout.style)
    }

    /// Opens a span that adds `attribute` to the style.
    fn span(&mut self, attribute: Attribute) {
        let style = self.style().with(attribute);
        self.styles.push(style);
    }

    /// `style` as the text of a link to `address`.
    fn link(&mut self, address: String, style: Style) -> Style {
        self.links.push(address);
        style.linked(self.links.len() - 1)
    }

    /// The columns of the rows, layout included.
    fn width(&self) -> usize {
        self.layout.indent + self.layout.width
    }

    /// The columns after the containers' marks.
    fn available(&self) -> usize {
        let marks: usize = self.containers.iter().map(Container::width).sum();
        self.layout.width.saturating_sub(marks)
    }

    fn push(&mut self, text: &str, style: Style) {
        self.line.extend(printable(text, style));
    }

    /// Adds text, making the web addresses in it links.
    fn text(&mut self, text: &str) {
        let style = self.style();
        if style.link().is_some() {
            return self.push(text, style);
        }
        let (mut rest, mut from) = (text, 0);
        while let Some(offset) = rest[from..].find("http") {
            let at = from + offset;
            let address = address(&rest[at..]);
            let starts_word = rest[..at].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
            if address.is_empty() || !starts_word {
                from = at + "http".len();
                continue;
            }
            self.push(&rest[..at], style);
            let linked = self.link(address.to_owned(), style);
            self.push(address, linked);
            (rest, from) = (&rest[at + address.len()..], 0);
        }
        self.push(rest, style);
    }

    /// Adds a code block's text, each of its lines a line of rows.
    fn code(&mut self, text: &str) {
        let style = self.style().in_color(Color::Cyan);
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                self.break_line();
            }
            self.push(line, style);
        }
    }

    /// Marks the list item as a task, done or not. The mark takes a bullet's
    /// place and follows a number.
    fn check(&mut self, done: bool) {
        let bullet = self.lists.last() == Some(&None);
        if let Some(Container::Item { width, marker: Some(marker) }) = self.containers.last_mut() {
            if bullet {
                marker.clear();
            }
            let mark = if done { (Style::GREEN, '✓') } else { (Style::DIM, '☐') };
            marker.extend([mark, (Style::PLAIN, ' ')]);
            *width = text::width(marker);
        }
    }

    /// Starts a block at `start` in the source: ends the line of text before
    /// it, and puts a blank row before it where the source has a blank line.
    fn block(&mut self, start: usize) {
        self.flush();
        if let Some(end) = self.end.take() {
            let end = self.source[..end.min(start)].trim_end().len();
            if self.source[end..start].matches('\n').count() > 1 {
                self.gap = self.gap.or(Some(self.containers.len()));
            }
        }
    }

    /// Ends the line being built, if it has text.
    fn flush(&mut self) {
        if !self.line.is_empty() {
            self.break_line();
        }
    }

    /// Ends the line being built: in a table as a line of its cell, and
    /// otherwise in rows.
    fn break_line(&mut self) {
        let line = mem::take(&mut self.line);
        match &mut self.table {
            Some(table) => table.cell.push(line),
            None => self.push_rows(&line, 0),
        }
    }

    /// Wraps `line` in rows after the containers' marks, with the rows a
    /// wrap starts `hang` columns further in.
    fn push_rows(&mut self, line: &[Glyph], hang: usize) {
        self.open_gap();
        let all = self.containers.len();
        let (first, indent) = self.marks(true, all);
        let (mut rest, _) = self.marks(false, all);
        rest.extend(std::iter::repeat_n((Style::PLAIN, ' '), hang));
        let mut rows = wrap(line, self.width(), &first, &rest);
        rows[0].indent = rows[0].indent.min(indent);
        self.rows.append(&mut rows);
    }

    /// Pushes `line` after the containers' marks, cut to the columns there
    /// are.
    fn push_line(&mut self, line: Line, joins: Joins) {
        let (mut row, indent) = self.marks(true, self.containers.len());
        row.extend(line);
        text::truncate(&mut row, self.width());
        self.rows.push(Row { line: row, indent, joins });
    }

    /// Puts the blank row that waits for the next row.
    fn open_gap(&mut self) {
        if let Some(containers) = self.gap.take() {
            let (marks, _) = self.marks(false, containers);
            self.rows.extend(wrap(&[], self.width(), &marks, &marks));
        }
    }

    /// The marks a row in the first `containers` containers starts with, and
    /// the columns of layout before the first marker, which copies as text.
    /// The marks are the layout's indent, then a bar for each quote and each
    /// list item's marker, which only the item's first row shows, and spaces
    /// as wide on its other rows.
    fn marks(&mut self, first: bool, containers: usize) -> (Line, usize) {
        let mut line = vec![(Style::PLAIN, ' '); self.layout.indent];
        let mut layout = None;
        for container in self.containers.iter_mut().take(containers) {
            match container {
                Container::Quote => line.extend([(Style::DIM, '│'), (Style::PLAIN, ' ')]),
                Container::Item { width, marker } => match marker.take_if(|_| first) {
                    Some(mut marker) => {
                        layout = layout.or(Some(text::width(&line)));
                        line.append(&mut marker);
                    }
                    None => line.extend(std::iter::repeat_n((Style::PLAIN, ' '), *width)),
                },
            }
        }
        let layout = layout.unwrap_or_else(|| text::width(&line));
        (line, layout)
    }

    /// A dim rule across the text's columns, labeled when there is a label,
    /// as `─ rust ───` is above code in Rust.
    fn rule(&mut self, label: &str) {
        self.open_gap();
        let columns = self.available().min(RULE_WIDTH);
        let head = if label.is_empty() { String::new() } else { format!("─ {label} ") };
        let fill = "─".repeat(columns.saturating_sub(text::str_width(&head)));
        self.push_line(
            text::fit(&[(&head, Style::DIM), (&fill, Style::DIM)], columns),
            Joins::Layout,
        );
    }

    fn table_row(&mut self) {
        if let Some(table) = &mut self.table {
            table.rows.push(Vec::new());
        }
    }

    /// Lays out a table in borders. Its columns are as wide as their text
    /// when they fit, and otherwise shrink, sharing the columns there are,
    /// and wrap their text. A table whose columns cannot shrink enough shows
    /// a record for each row instead.
    fn table(&mut self, table: Table) {
        let Table { alignments, rows, .. } = table;
        let count = alignments.len();
        let mut natural = vec![0; count];
        let mut least = vec![0; count];
        for row in &rows {
            for ((natural, least), cell) in natural.iter_mut().zip(&mut least).zip(row) {
                for line in cell {
                    *natural = text::width(line).max(*natural);
                    *least = longest_word(line).max(*least);
                }
            }
        }
        for (least, natural) in least.iter_mut().zip(&natural) {
            *least = (*least).clamp(COLUMN_LEAST, COLUMN_WORD).min(*natural);
        }
        let available = self.available();
        let borders = 3 * count + 1;
        let widths = if natural.iter().sum::<usize>() + borders <= available {
            natural
        } else {
            match available.checked_sub(least.iter().sum::<usize>() + borders) {
                Some(spare) => share(spare, &least, &natural),
                None => return self.records(&rows),
            }
        };
        // Each row's cells as the lines they wrap to.
        let laid: Vec<Vec<Cell>> = rows
            .iter()
            .map(|row| {
                (0..count)
                    .map(|column| {
                        let lines = row.get(column).into_iter().flatten();
                        let rows = lines.flat_map(|line| wrap(line, widths[column], &[], &[]));
                        rows.map(|row| row.line).collect()
                    })
                    .collect()
            })
            .collect();
        // Rows of more than one line are told apart by rules between rows.
        let ruled = laid.iter().skip(1).any(|row| row.iter().any(|cell| cell.len() > 1));
        self.open_gap();
        self.push_line(border(&widths, ['┌', '┬', '┐']), Joins::Layout);
        for (index, row) in laid.iter().enumerate() {
            match index {
                0 => {}
                1 => self.push_line(border(&widths, ['├', '┼', '┤']), Joins::Table),
                _ if ruled => self.push_line(border(&widths, ['├', '┼', '┤']), Joins::Layout),
                _ => {}
            }
            let height = row.iter().map(Vec::len).max().unwrap_or(0).max(1);
            for line in 0..height {
                self.push_line(table_line(&widths, &alignments, row, line), Joins::Table);
            }
        }
        self.push_line(border(&widths, ['└', '┴', '┘']), Joins::Layout);
    }

    /// Lays out a table as a record for each row after its header: each
    /// column's heading, then the row's text in that column.
    fn records(&mut self, rows: &[Vec<Cell>]) {
        let Some((head, body)) = rows.split_first() else { return };
        let labels: Vec<&[Glyph]> =
            head.iter().map(|cell| cell.first().map_or(&[][..], Vec::as_slice)).collect();
        let width = labels.iter().map(|label| text::width(label)).max().unwrap_or(0);
        let width = width.min(self.available() / 3);
        let hang = if width > 0 { width + 2 } else { 0 };
        for (index, row) in body.iter().enumerate() {
            if index > 0 {
                self.gap = self.gap.or(Some(self.containers.len()));
            }
            for (column, cell) in row.iter().enumerate() {
                let mut label = labels.get(column).map_or_else(Vec::new, |label| label.to_vec());
                text::truncate(&mut label, width);
                text::pad(&mut label, hang, Style::PLAIN);
                for (number, text) in cell.iter().enumerate() {
                    let mut line =
                        if number == 0 { label.clone() } else { vec![(Style::PLAIN, ' '); hang] };
                    line.extend_from_slice(text);
                    self.push_rows(&line, hang);
                }
            }
        }
    }
}

/// Column widths from each column's least, sharing `spare` columns in
/// proportion to how much narrower than its text each column is.
fn share(spare: usize, least: &[usize], natural: &[usize]) -> Vec<usize> {
    let wants: Vec<usize> =
        natural.iter().zip(least).map(|(natural, least)| natural - least).collect();
    let total = wants.iter().sum::<usize>().max(1);
    let mut widths: Vec<usize> =
        least.iter().zip(&wants).map(|(least, want)| least + want * spare / total).collect();
    // What rounding down left over goes to the columns still short, in order.
    let mut left = spare - wants.iter().map(|want| want * spare / total).sum::<usize>();
    for (width, natural) in widths.iter_mut().zip(natural) {
        if left > 0 && *width < *natural {
            *width += 1;
            left -= 1;
        }
    }
    widths
}

/// A table's border across columns `widths` wide, from its left, middle and
/// right pieces.
fn border(widths: &[usize], [left, middle, right]: [char; 3]) -> Line {
    let mut line = vec![(Style::DIM, left)];
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            line.push((Style::DIM, middle));
        }
        line.extend(std::iter::repeat_n((Style::DIM, '─'), width + 2));
    }
    line.push((Style::DIM, right));
    line
}

/// Line `line` of a table row's cells, aligned in columns `widths` wide.
fn table_line(widths: &[usize], alignments: &[Alignment], cells: &[Cell], line: usize) -> Line {
    let mut out = vec![(Style::DIM, '│')];
    let space = |count| std::iter::repeat_n((Style::PLAIN, ' '), count);
    for ((width, alignment), cell) in widths.iter().zip(alignments).zip(cells) {
        let text = cell.get(line).map_or(&[][..], Vec::as_slice);
        let pad = width.saturating_sub(text::width(text));
        let (before, after) = match alignment {
            Alignment::Right => (pad, 0),
            Alignment::Center => (pad / 2, pad - pad / 2),
            Alignment::Left | Alignment::None => (0, pad),
        };
        out.extend(space(before + 1));
        out.extend_from_slice(text);
        out.extend(space(after + 1));
        out.push((Style::DIM, '│'));
    }
    out
}

/// The columns the longest word of `line` takes.
fn longest_word(line: &[Glyph]) -> usize {
    line.split(|glyph| glyph.1 == ' ').map(text::width).max().unwrap_or(0)
}

/// Whether a code block's source has the fence that closes it, or needs none.
fn closed(block: &str) -> bool {
    let opening = content(block.lines().next().unwrap_or_default());
    if !opening.starts_with("```") && !opening.starts_with("~~~") {
        return true;
    }
    block.trim_end().rsplit_once('\n').is_some_and(|(_, last)| {
        let last = content(last).trim_end();
        last.len() >= 3 && last.bytes().all(|byte| byte == opening.as_bytes()[0])
    })
}

/// A line of source after its indent and the markers of the quotes it is in.
fn content(line: &str) -> &str {
    line.trim_start_matches([' ', '\t', '>'])
}

/// The web address `text` starts with, or nothing: up to a space, less the
/// punctuation that follows it in a sentence and a closing parenthesis it
/// did not open.
fn address(text: &str) -> &str {
    let Some(scheme) = ["https://", "http://"].into_iter().find(|&scheme| text.starts_with(scheme))
    else {
        return "";
    };
    let end = text
        .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`'))
        .unwrap_or(text.len());
    let mut address = &text[..end];
    loop {
        let trimmed = address.trim_end_matches(['.', ',', ':', ';', '!', '?', '\'', '*', '_', '~']);
        let unopened =
            trimmed.ends_with(')') && trimmed.matches(')').count() > trimmed.matches('(').count();
        let trimmed = if unopened { &trimmed[..trimmed.len() - 1] } else { trimmed };
        if trimmed.len() == address.len() {
            break;
        }
        address = trimmed;
    }
    if address.len() > scheme.len() { address } else { "" }
}

/// Whether a link's address opens in a browser or mail app, rather than being
/// a path or anything else a click should not run.
fn is_web(address: &str) -> bool {
    ["https://", "http://", "mailto:"].into_iter().any(|scheme| {
        address.get(..scheme.len()).is_some_and(|start| start.eq_ignore_ascii_case(scheme))
    })
}

/// Whether inline HTML is a line break, as tables' cells use.
fn is_break(html: &str) -> bool {
    let tag = html.trim().trim_start_matches('<').trim_end_matches('>');
    tag.trim_end_matches('/').trim().eq_ignore_ascii_case("br")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::text::{plain, runs};

    /// A reply with each kind of block and span, as models write them.
    const REPLY: &str = "## Summary\n\nThe build *failed*: `cargo` couldn't find [the manifest](https://doc.rust-lang.org/cargo). See ~~old~~ notes at https://example.com/notes.\n**Status:** failed\n**Time:** 4.2s\n\n| Crate | Size | Notes |\n|:------|-----:|:-----:|\n| `ring` | 1.2 MB | crypto, **needed** |\n| image | 900 KB | decoders for png, jpeg, webp and gif<br>and more |\n\n1. First step\n   - nested bullet with `code`\n     - [ ] task item 界\n2. Second step\n\n   ```sh\n   cargo test --workspace\n   ```\n\n> **Note:** quoted *emphasis*\n>\n> - quoted item\n\nEscaped \\*stars\\*, snake_case_name, 2 * 3 * 4 and &amp; an entity.\n\n---\n\n```rust\nfn main() {\n\tprintln!(\"hi\");\n}\n```\n";

    fn layout(width: usize) -> Layout {
        Layout { indent: 0, width, style: Style::PLAIN }
    }

    /// The rows `source` lays out in `width` columns, and the addresses its
    /// links open.
    fn lay_out(source: &str, width: usize) -> (Vec<Row>, Vec<String>) {
        let (mut rows, mut links) = (Vec::new(), Vec::new());
        render(source, layout(width), &mut rows, &mut links);
        (rows, links)
    }

    /// The rows streamed `source` shows in `width` columns before more
    /// arrives.
    fn streamed(source: &str, width: usize) -> Vec<Row> {
        let (mut rows, mut links) = (Vec::new(), Vec::new());
        Stream::default().lay_out(source, false, layout(width), &mut rows, &mut links);
        rows
    }

    fn shown(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|row| plain(&row.line)).collect()
    }

    #[test]
    fn blocks_keep_their_line_breaks_with_a_blank_row_where_the_source_has_one() {
        let source = "## Result\nThe **header** uses `flex`:\n- one\n- two\n\n\n\n**Status:** done\n**Time:** 4s\r\n\n---\n### Minor";
        let (rows, _) = lay_out(source, 20);
        assert_eq!(
            shown(&rows),
            [
                "Result",
                "The header uses",
                "flex:",
                "• one",
                "• two",
                "",
                "Status: done",
                "Time: 4s",
                "",
                "────────────────────",
                "Minor"
            ]
        );
        let joins: Vec<Joins> = rows.iter().map(|row| row.joins).collect();
        assert_eq!(joins[1..3], [Joins::Line, Joins::Wrap(1)]);
        assert_eq!(joins[9], Joins::Layout, "a rule is layout");
        assert_eq!(runs(&rows[0].line), [(Style::BOLD, "Result".into())]);
        assert_eq!(runs(&rows[10].line), [(Style::BOLD.with(Attribute::Italic), "Minor".into())]);
    }

    #[test]
    fn spans_take_their_styles_and_escapes_read_as_text() {
        let (rows, _) = lay_out("*em* **strong** ~~gone~~ `code` \\*literal\\* &amp;", 80);
        assert_eq!(
            runs(&rows[0].line),
            [
                (Style::PLAIN.with(Attribute::Italic), "em".into()),
                (Style::PLAIN, " ".into()),
                (Style::BOLD, "strong".into()),
                (Style::PLAIN, " ".into()),
                (Style::PLAIN.with(Attribute::Strike), "gone".into()),
                (Style::PLAIN, " ".into()),
                (Style::CODE, "code".into()),
                (Style::PLAIN, " *literal* &".into()),
            ]
        );
        let (rows, _) = lay_out("a **bold\nline**, 2 * 3 and snake_case_name", 80);
        assert_eq!(shown(&rows), ["a bold", "line, 2 * 3 and snake_case_name"]);
        assert_eq!(rows[1].style_at(0), Some(Style::BOLD), "emphasis spans a line break");
    }

    #[test]
    fn links_to_web_addresses_and_addresses_in_text_open_on_click() {
        let source = "See [the docs](https://docs.rs/x), [a file](src/main.rs) or https://example.com/a_(b).";
        let (rows, links) = lay_out(source, 80);
        assert_eq!(shown(&rows), ["See the docs, a file or https://example.com/a_(b)."]);
        assert_eq!(links, ["https://docs.rs/x", "https://example.com/a_(b)"]);
        let link = |column| rows[0].style_at(column).and_then(Style::link);
        assert_eq!(
            [4, 11, 14, 24, 48, 49].map(link),
            [Some(0), Some(0), None, Some(1), Some(1), None],
            "the docs, a file, the address and the period after it"
        );
        assert_eq!(rows[0].style_at(4), Some(Style::PLAIN.linked(0)));
        let (_, links) = lay_out("`https://in.code` and xhttps://no.word and http:// alone", 80);
        assert!(links.is_empty(), "{links:?}");
    }

    #[test]
    fn lists_nest_number_and_check_their_items() {
        let source =
            "1. first\n   - nested `code`\n     - deeper\n2. [ ] task\n\n- [x] done\n- plain";
        let (rows, _) = lay_out(source, 40);
        assert_eq!(
            shown(&rows),
            ["1. first", "   • nested code", "     ◦ deeper", "2. ☐ task", "", "✓ done", "• plain"]
        );
        assert_eq!(runs(&rows[5].line)[0], (Style::GREEN, "✓".into()));
        let (rows, _) = lay_out("- one two three", 8);
        assert_eq!(shown(&rows), ["• one", "  two", "  three"]);
        assert_eq!(
            (rows[0].text(0..usize::MAX), rows[2].text(0..usize::MAX)),
            ("• one".into(), "three".into()),
            "a marker copies as text and the hang under it as layout"
        );
    }

    #[test]
    fn quotes_bar_their_rows_and_keep_what_is_in_them() {
        let (rows, _) = lay_out("> **Note:** quoted\n>\n> - item", 40);
        assert_eq!(shown(&rows), ["│ Note: quoted", "│", "│ • item"]);
        assert_eq!(
            runs(&rows[0].line),
            [
                (Style::DIM, "│".into()),
                (Style::PLAIN, " ".into()),
                (Style::DIM.with(Attribute::Bold), "Note:".into()),
                (Style::DIM, " quoted".into()),
            ]
        );
        assert_eq!(rows[0].text(0..usize::MAX), "Note: quoted");
        let (rows, _) = lay_out("text\n\n> quoted\n\n- item\n\n  > nested", 40);
        assert_eq!(
            shown(&rows),
            ["text", "", "│ quoted", "", "• item", "", "  │ nested"],
            "a blank row before a quote is outside it"
        );
    }

    #[test]
    fn code_blocks_draw_rules_and_close_only_with_their_fence() {
        let (rows, _) = lay_out("```rust\nfn main() {\n\tlet x;\n\n}\n```\nafter", 20);
        assert_eq!(
            shown(&rows),
            [
                "─ rust ─────────────",
                "fn main() {",
                "    let x;",
                "",
                "}",
                "────────────────────",
                "after"
            ]
        );
        let joins: Vec<Joins> = rows.iter().map(|row| row.joins).collect();
        assert_eq!(joins[..2], [Joins::Layout, Joins::Line]);
        assert_eq!(runs(&rows[1].line), [(Style::CODE, "fn main() {".into())]);
        let (rows, _) = lay_out("````markdown\n```rust\n**still code**\n````\n```inline```", 20);
        assert_eq!(
            shown(&rows),
            ["─ markdown ─────────", "```rust", "**still code**", "────────────────────", "inline"]
        );
        let (rows, _) = lay_out("```sh\ncargo", 20);
        assert_eq!(shown(&rows), ["─ sh ───────────────", "cargo"], "an open fence has no end");
    }

    #[test]
    fn tables_align_columns_in_borders_and_copy_as_markdown() {
        let source = "| Crate | Size |\n|:--|--:|\n| ring | 1.2 MB |\n| image | 900 KB |";
        let (rows, _) = lay_out(source, 40);
        assert_eq!(
            shown(&rows),
            [
                "┌───────┬────────┐",
                "│ Crate │   Size │",
                "├───────┼────────┤",
                "│ ring  │ 1.2 MB │",
                "│ image │ 900 KB │",
                "└───────┴────────┘"
            ]
        );
        assert_eq!(rows[1].style_at(2), Some(Style::BOLD), "the header is bold");
        let copied: Vec<String> = rows
            .iter()
            .filter(|row| row.joins != Joins::Layout)
            .map(|row| row.text(0..usize::MAX))
            .collect();
        assert_eq!(
            copied,
            [
                "| Crate |   Size |",
                "|-------|--------|",
                "| ring  | 1.2 MB |",
                "| image | 900 KB |"
            ]
        );
    }

    #[test]
    fn tables_too_wide_wrap_their_cells_or_show_a_record_for_each_row() {
        let source =
            "| Key | Description |\n|---|---|\n| a | one two three four five six |\n| b | short |";
        let (rows, _) = lay_out(source, 24);
        assert_eq!(
            shown(&rows),
            [
                "┌─────┬────────────────┐",
                "│ Key │ Description    │",
                "├─────┼────────────────┤",
                "│ a   │ one two three  │",
                "│     │ four five six  │",
                "├─────┼────────────────┤",
                "│ b   │ short          │",
                "└─────┴────────────────┘"
            ]
        );
        let (rows, _) = lay_out(source, 12);
        assert_eq!(
            shown(&rows),
            [
                "Key   a",
                "Des…  one",
                "      two",
                "      three",
                "      four",
                "      five",
                "      six",
                "",
                "Key   b",
                "Des…  short"
            ]
        );
    }

    #[test]
    fn chunks_end_before_a_first_column_line_after_a_blank_line_outside_a_fence() {
        let source = "one\n\n- two\n\n  continued\n```\n\nfenced\n```\n\nthree";
        let mut scanner = Scanner::default();
        let (mut starts, mut start) = (Vec::new(), 0);
        while let Some(end) = scanner.chunk_end(source, start) {
            starts.push(&source[end..]);
            start = end;
        }
        assert_eq!(starts, ["- two\n\n  continued\n```\n\nfenced\n```\n\nthree", "three"]);
    }

    #[test]
    fn the_end_of_a_stream_shows_as_it_will_read() {
        assert_eq!(
            runs(&streamed("Use **bol", 40)[0].line),
            [(Style::PLAIN, "Use ".into()), (Style::BOLD, "bol".into())]
        );
        assert_eq!(runs(&streamed("`cargo te", 40)[0].line), [(Style::CODE, "cargo te".into())]);
        assert_eq!(shown(&streamed("Use ** ", 40)), ["Use"], "an opener waits for its text");
        assert_eq!(shown(&streamed("text\n- ", 40)), ["text"], "a marker waits for its text");
        let rows = streamed("Read [docs](https://docs.rs", 40);
        assert_eq!(
            (shown(&rows), rows[0].style_at(5)),
            (vec!["Read docs".into()], Some(Style::PLAIN.linked(0)))
        );
        assert!(streamed("| a | b |\n|--", 40).is_empty(), "a table waits for its header's rule");
        assert_eq!(
            shown(&streamed("| a | b |\n|---|---|\n| 1 |", 40)),
            ["┌───┬───┐", "│ a │ b │", "└───┴───┘"],
            "a row waits for its end"
        );
        assert_eq!(
            shown(&streamed("```sh\nlet a = `b", 40))[1],
            "let a = `b",
            "code closes nothing"
        );
    }

    #[test]
    fn a_stream_fits_its_width_and_ends_as_the_whole_source_lays_out() {
        for width in [1, 4, 13, 40, 100] {
            let layout = Layout { indent: 2, width, style: Style::PLAIN };
            let (mut rows, mut links, mut stream) = (Vec::new(), Vec::new(), Stream::default());
            for (end, _) in REPLY.char_indices().skip(1) {
                stream.lay_out(&REPLY[..end], false, layout, &mut rows, &mut links);
                if let Some(row) = rows.iter().find(|row| text::width(&row.line) > width + 2) {
                    panic!("{width} columns at byte {end}: {:?}", plain(&row.line));
                }
            }
            let reply = REPLY.trim_end();
            stream.lay_out(reply, true, layout, &mut rows, &mut links);
            assert!(reply[stream.settled..].starts_with("```rust"), "{width}: laid out in chunks");
            let (mut whole, mut addresses) = (Vec::new(), Vec::new());
            render(reply, layout, &mut whole, &mut addresses);
            assert_eq!((&rows, &links), (&whole, &addresses), "{width} columns");
        }
        for width in 1..60 {
            let (rows, _) = lay_out(REPLY, width);
            let wide = rows.iter().find(|row| text::width(&row.line) > width);
            assert!(wide.is_none(), "{width} columns: {:?}", wide.map(|row| plain(&row.line)));
        }
    }
}
