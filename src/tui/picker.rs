//! A list of choices filtered by typing, as menus show it.
//!
//! A list is laid out as a table. Each column is as wide as its widest cell,
//! measured when the items are set, so rows line up however the list is
//! filtered or scrolled. One column takes the room the others leave: the text
//! column when the list has one, such as a description, or else the labels.
//! When the room runs out that column is cut first, down to half the row for
//! labels, and then value columns are left out from the right.

use std::borrow::Cow;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::text::{self, Line, Style, fit};

/// Rows moved by Page Up and Page Down.
const PAGE: isize = 8;
/// Columns between two columns of a list.
const GAP: usize = 2;
/// The columns of a list that gives none: labels, then a description.
const LABELED: &[Column] = &[Column::Label(""), Column::Text];

pub(crate) struct Item {
    label: String,
    /// What queries match against, in lowercase: the label, unless set.
    key: String,
    /// The item's cells in the list's columns other than the labels, in order.
    cells: Vec<String>,
    /// A short note at the end of the row, such as `current`.
    tag: String,
    /// The heading the item is listed under while nothing is typed.
    group: &'static str,
}

impl Item {
    pub(crate) fn new(label: impl Into<String>) -> Self {
        let label = label.into();
        Self { key: label.to_lowercase(), label, cells: Vec::new(), tag: String::new(), group: "" }
    }

    /// Adds the item's cell in the next column.
    pub(crate) fn cell(mut self, cell: impl Into<String>) -> Self {
        self.cells.push(cell.into());
        self
    }

    pub(crate) fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = tag.into();
        self
    }

    pub(crate) fn group(mut self, group: &'static str) -> Self {
        self.group = group;
        self
    }

    /// Matches queries against `key` rather than the label, as a command
    /// matches its name without the `/` its label shows.
    pub(crate) fn matched_by(mut self, key: &str) -> Self {
        self.key = key.to_lowercase();
        self
    }
}

/// A column of a list, with its heading. A list has one column of labels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Column {
    Label(&'static str),
    /// Values lined up on their left, such as paths.
    Left(&'static str),
    /// Values lined up on their right, such as prices.
    Right(&'static str),
    /// Text that takes the room left, such as a description.
    Text,
}

impl Column {
    fn heading(self) -> &'static str {
        match self {
            Self::Label(heading) | Self::Left(heading) | Self::Right(heading) => heading,
            Self::Text => "",
        }
    }
}

/// What Enter picks.
#[derive(Debug, PartialEq)]
pub(crate) enum Choice {
    /// The index of an item.
    Item(usize),
    /// The query itself, in lists that accept values they do not show.
    Typed(String),
}

/// A list's rows as drawn, with the position in the list each row shows.
pub(crate) struct Shown {
    pub(crate) rows: Vec<Line>,
    pub(crate) positions: Vec<Option<usize>>,
}

/// A row of a list: a heading, or the choice at a position.
#[derive(Clone, Copy, PartialEq)]
enum Row {
    Heading(&'static str),
    Choice(usize),
}

pub(crate) struct Picker {
    items: Vec<Item>,
    columns: &'static [Column],
    /// The index of the column of labels.
    labels: usize,
    /// The width of each column's widest cell and heading. A value column
    /// whose cells are all empty is zero wide and left out.
    widths: Vec<usize>,
    tag_width: usize,
    /// What the list says when nothing matches.
    empty: Cow<'static, str>,
    query: String,
    /// Indexes of the items matching the query, best match first.
    matches: Vec<usize>,
    /// A position in `matches`; one past its end is the typed query.
    selected: usize,
    /// The first row shown, which moves only to keep the selection in view.
    top: usize,
    accepts_typed: bool,
    /// Whether the items have groups, which headings name.
    grouped: bool,
    /// Whether the query is offered after the matches, as a value of its own.
    typed: bool,
}

impl Picker {
    pub(crate) fn new(items: Vec<Item>) -> Self {
        let mut picker = Self {
            items: Vec::new(),
            columns: LABELED,
            labels: 0,
            widths: Vec::new(),
            tag_width: 0,
            empty: Cow::Borrowed("nothing matches"),
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
            top: 0,
            accepts_typed: false,
            grouped: false,
            typed: false,
        };
        picker.set_items(items);
        picker
    }

    /// Lays the items out in `columns`, which hold one column of labels; the
    /// items' cells fill the others in order.
    pub(crate) fn columns(mut self, columns: &'static [Column]) -> Self {
        self.columns = columns;
        self.labels =
            columns.iter().position(|column| matches!(column, Column::Label(_))).unwrap_or(0);
        self.measure();
        self
    }

    /// Offers a typed query no item is labeled with as a choice of its own.
    pub(crate) fn accepting_typed(mut self) -> Self {
        self.accepts_typed = true;
        self.filter();
        self
    }

    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) fn label(&self, index: usize) -> &str {
        &self.items[index].label
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The rows the list takes while nothing is typed: its items, the
    /// headings of their groups and a row of column headings.
    pub(crate) fn height(&self) -> usize {
        let groups = if self.grouped {
            1 + self.items.windows(2).filter(|pair| pair[0].group != pair[1].group).count()
        } else {
            0
        };
        let headed = self
            .columns
            .iter()
            .zip(&self.widths)
            .any(|(column, width)| *width > 0 && !column.heading().is_empty());
        (self.items.len() + groups + usize::from(headed)).max(1)
    }

    /// The selected position, counted from one, and how many there are.
    pub(crate) fn count(&self) -> (usize, usize) {
        (self.selected + 1, self.len())
    }

    /// Replaces the items, keeping the query.
    pub(crate) fn set_items(&mut self, items: Vec<Item>) {
        self.grouped = items.iter().any(|item| !item.group.is_empty());
        self.items = items;
        self.measure();
        self.filter();
    }

    /// Sets what the list says when nothing matches.
    pub(crate) fn set_empty(&mut self, empty: impl Into<Cow<'static, str>>) {
        self.empty = empty.into();
    }

    pub(crate) fn set_query(&mut self, query: &str) {
        if self.query != query {
            query.clone_into(&mut self.query);
            self.filter();
        }
    }

    /// Moves the selection to the item labeled `label`, if it is shown.
    pub(crate) fn select(&mut self, label: &str) {
        self.select_where(|_, item| item.label == label);
    }

    /// Moves the selection to the item at `index`, if it is shown.
    pub(crate) fn select_item(&mut self, index: usize) {
        self.select_where(|shown, _| shown == index);
    }

    /// Moves the selection to `position`, as a click on its row does.
    pub(crate) fn choose(&mut self, position: usize) {
        self.selected = position.min(self.len().saturating_sub(1));
    }

    pub(crate) fn choice(&self) -> Option<Choice> {
        match self.matches.get(self.selected) {
            Some(&index) => Some(Choice::Item(index)),
            None => self.typed.then(|| Choice::Typed(self.query.trim().to_owned())),
        }
    }

    /// Applies a key that moves the selection or edits the query; other keys
    /// are ignored.
    pub(crate) fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::Char('p') if ctrl => self.step(-1),
            KeyCode::Char('n') if ctrl => self.step(1),
            KeyCode::PageUp => self.step(-PAGE),
            KeyCode::PageDown => self.step(PAGE),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.len().saturating_sub(1),
            KeyCode::Char('u') if ctrl => self.set_query(""),
            KeyCode::Backspace => {
                let mut query = self.query.clone();
                query.pop();
                self.set_query(&query);
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                let query = format!("{}{c}", self.query);
                self.set_query(&query);
            }
            _ => {}
        }
    }

    /// Appends pasted text to the query, without line breaks or other control
    /// characters.
    pub(crate) fn paste(&mut self, text: &str) {
        let pasted: String = text.chars().filter(|c| !c.is_control()).collect();
        let query = format!("{}{pasted}", self.query);
        self.set_query(&query);
    }

    /// Moves the selection by `delta` rows, stopping at either end.
    pub(crate) fn step(&mut self, delta: isize) {
        self.selected =
            self.selected.saturating_add_signed(delta).min(self.len().saturating_sub(1));
    }

    /// At most `limit` rows `width` columns wide that keep the selection in
    /// view: a row of column headings when the list has them, the selected
    /// row in reverse video, and group headings while nothing is typed. Only
    /// a grouped list, which is short, is walked whole; any other costs what
    /// it shows.
    pub(crate) fn show(&mut self, width: usize, limit: usize) -> Shown {
        let widths = self.fit(width);
        let mut rows = Vec::new();
        let headed = !self.is_empty()
            && self
                .columns
                .iter()
                .zip(&widths)
                .any(|(column, width)| width.is_some() && !column.heading().is_empty());
        if headed {
            let heading = |column: usize| (self.columns[column].heading(), Style::DIM);
            rows.push(self.line(&widths, width, heading, "", Style::PLAIN));
        }
        let mut positions = vec![None; rows.len()];
        if self.is_empty() {
            rows.push(fit(&[(" ", Style::PLAIN), (&self.empty, Style::DIM)], width));
            positions.push(None);
            return Shown { rows, positions };
        }
        let limit = limit.saturating_sub(rows.len()).max(1);
        let len = self.len();
        let window: Vec<Row> = if self.grouped && self.query.trim().is_empty() {
            let mut rows = Vec::with_capacity(len + 2);
            let mut group = "";
            for position in 0..len {
                if let Some(&index) = self.matches.get(position)
                    && self.items[index].group != group
                {
                    group = self.items[index].group;
                    rows.push(Row::Heading(group));
                }
                rows.push(Row::Choice(position));
            }
            let selected =
                rows.iter().position(|row| *row == Row::Choice(self.selected)).unwrap_or(0);
            // The heading just above the selection comes into view with it.
            let heading = selected > 0 && matches!(rows[selected - 1], Row::Heading(_));
            self.scroll(selected - usize::from(heading), selected, rows.len(), limit);
            rows[self.top..(self.top + limit).min(rows.len())].to_vec()
        } else {
            self.scroll(self.selected, self.selected, len, limit);
            (self.top..(self.top + limit).min(len)).map(Row::Choice).collect()
        };
        for row in window {
            match row {
                Row::Heading(group) => {
                    rows.push(fit(&[(" ", Style::PLAIN), (group, Style::DIM)], width));
                    positions.push(None);
                }
                Row::Choice(position) => {
                    rows.push(self.row(position, &widths, width));
                    positions.push(Some(position));
                }
            }
        }
        Shown { rows, positions }
    }

    /// Moves the first row shown just enough that rows `first` to `last` of
    /// `total` are in a view of `limit` rows.
    fn scroll(&mut self, first: usize, last: usize, total: usize, limit: usize) {
        if first < self.top {
            self.top = first;
        } else if last >= self.top + limit {
            self.top = last + 1 - limit;
        }
        self.top = self.top.min(total.saturating_sub(limit));
    }

    /// The row of the choice at `position`: an item's cells and tag, or the
    /// typed query, which spans the row since no column was measured for it.
    fn row(&self, position: usize, widths: &[Option<usize>], width: usize) -> Line {
        // The selected row is one solid bar.
        let base = if position == self.selected { Style::PLAIN.reversed() } else { Style::PLAIN };
        let Some(&index) = self.matches.get(position) else {
            let typed = format!("use \"{}\"", self.query.trim());
            let mut row = fit(&[(" ", base), (&typed, base)], width);
            let used = text::width(&row);
            row.extend(std::iter::repeat_n((base, ' '), width - used));
            return row;
        };
        let item = &self.items[index];
        let cell = |column: usize| {
            if column == self.labels {
                (item.label.as_str(), Style::PLAIN)
            } else {
                (item.cells.get(self.cell_index(column)).map_or("", String::as_str), Style::DIM)
            }
        };
        self.line(widths, width, cell, &item.tag, base)
    }

    /// A row of the table `width` columns wide: each column's cell at the
    /// width `widths` gives it, then `tag` at the end. `base` is the style
    /// of the row, which a cell's own style shows in unless the row is
    /// reversed.
    fn line<'a>(
        &self,
        widths: &[Option<usize>],
        width: usize,
        cell: impl Fn(usize) -> (&'a str, Style),
        tag: &str,
        base: Style,
    ) -> Line {
        let style = |style: Style| if base == Style::PLAIN { style } else { base };
        let blank = |count: usize| std::iter::repeat_n((base, ' '), count);
        let mut row: Line = vec![(base, ' ')];
        let mut at = 1;
        for (index, (&column, &column_width)) in self.columns.iter().zip(widths).enumerate() {
            let Some(column_width) = column_width else { continue };
            if at > 1 {
                row.extend(blank(GAP));
                at += GAP;
            }
            let (text, text_style) = cell(index);
            let glyphs = fit(&[(text, style(text_style))], column_width);
            let space = column_width - text::width(&glyphs);
            if matches!(column, Column::Right(_)) {
                row.extend(blank(space));
                row.extend(glyphs);
            } else {
                row.extend(glyphs);
                row.extend(blank(space));
            }
            at += column_width;
        }
        if self.tag_width > 0 {
            let start = width.saturating_sub(1 + self.tag_width);
            row.extend(blank(start.saturating_sub(at)));
            let glyphs = fit(&[(tag, style(Style::DIM))], self.tag_width);
            let space = self.tag_width - text::width(&glyphs);
            row.extend(glyphs);
            row.extend(blank(space));
        }
        text::truncate(&mut row, width);
        let used = text::width(&row);
        row.extend(blank(width.saturating_sub(used)));
        row
    }

    /// The width each column is drawn at in rows `width` columns wide, or
    /// `None` for a column left out.
    fn fit(&self, width: usize) -> Vec<Option<usize>> {
        let tail = if self.tag_width > 0 { GAP + self.tag_width + 1 } else { 1 };
        let room = width.saturating_sub(1 + tail);
        let labels = self.widths[self.labels];
        let mut widths: Vec<Option<usize>> = self
            .columns
            .iter()
            .zip(&self.widths)
            .map(|(column, &width)| {
                let value = matches!(column, Column::Left(_) | Column::Right(_));
                (value && width > 0).then_some(width)
            })
            .collect();
        // Value columns keep their width while the labels keep half the row;
        // past that they are left out from the right.
        loop {
            let values: usize = widths.iter().flatten().map(|width| width + GAP).sum();
            let left = room.saturating_sub(values);
            let last = widths.iter().rposition(Option::is_some);
            match last {
                Some(index) if left < labels.min(room / 2) => widths[index] = None,
                _ => {
                    let label = labels.min(left);
                    widths[self.labels] = Some(label);
                    if let Some(text) =
                        self.columns.iter().position(|column| *column == Column::Text)
                    {
                        let rest = left.saturating_sub(label + GAP);
                        widths[text] = (rest > 0).then_some(rest);
                    }
                    return widths;
                }
            }
        }
    }

    /// Measures each column's widest cell and heading, and the widest tag.
    fn measure(&mut self) {
        let widest = |cell: &dyn Fn(&Item) -> &str| {
            self.items.iter().map(|item| text::str_width(cell(item))).max().unwrap_or(0)
        };
        self.widths = self
            .columns
            .iter()
            .enumerate()
            .map(|(index, &column)| {
                let heading = text::str_width(column.heading());
                match column {
                    Column::Label(_) => widest(&|item| &item.label).max(heading),
                    Column::Left(_) | Column::Right(_) => {
                        let cell = self.cell_index(index);
                        let cells = widest(&|item| item.cells.get(cell).map_or("", String::as_str));
                        if cells == 0 { 0 } else { cells.max(heading) }
                    }
                    Column::Text => 0,
                }
            })
            .collect();
        self.tag_width = widest(&|item| &item.tag);
    }

    /// Which of an item's cells column `column` shows.
    fn cell_index(&self, column: usize) -> usize {
        if column > self.labels { column - 1 } else { column }
    }

    fn len(&self) -> usize {
        self.matches.len() + usize::from(self.typed)
    }

    fn select_where(&mut self, chosen: impl Fn(usize, &Item) -> bool) {
        if let Some(position) =
            self.matches.iter().position(|&index| chosen(index, &self.items[index]))
        {
            self.selected = position;
        }
    }

    fn filter(&mut self) {
        let query = self.query.trim();
        let lowercase = query.to_lowercase();
        let mut ranked: Vec<(u8, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| rank(&item.key, &lowercase).map(|rank| (rank, index)))
            .collect();
        ranked.sort_by_key(|&(rank, _)| rank);
        self.matches = ranked.into_iter().map(|(_, index)| index).collect();
        self.typed = self.accepts_typed
            && !query.is_empty()
            && !self.items.iter().any(|item| item.label == query);
        self.selected = 0;
        self.top = 0;
    }
}

/// How closely `key` matches `query`, both lowercase: 0 for a prefix, 1 for
/// a substring, 2 for the query's characters in order.
fn rank(key: &str, query: &str) -> Option<u8> {
    if key.starts_with(query) {
        Some(0)
    } else if key.contains(query) {
        Some(1)
    } else {
        let mut rest = key.chars();
        query.chars().all(|c| rest.any(|candidate| candidate == c)).then_some(2)
    }
}

#[cfg(test)]
mod tests {
    use super::text::{plain, runs};
    use super::*;

    fn picker(labels: &[&str]) -> Picker {
        Picker::new(labels.iter().map(|label| Item::new(*label)).collect())
    }

    fn shown(picker: &Picker) -> Vec<&str> {
        picker.matches.iter().map(|&index| picker.label(index)).collect()
    }

    fn rows(shown: &Shown) -> Vec<String> {
        shown.rows.iter().map(|row| plain(row).trim_end().to_owned()).collect()
    }

    #[test]
    fn matches_rank_prefixes_then_substrings_then_scattered_letters() {
        let mut picker = picker(&["openai/gpt-5-mini", "claude-sonnet", "gpt-5", "GPT-4o"]);
        picker.set_query("gpt");
        assert_eq!(shown(&picker), ["gpt-5", "GPT-4o", "openai/gpt-5-mini"]);
        picker.set_query("cst");
        assert_eq!(shown(&picker), ["claude-sonnet"]);
        picker.set_query("zzz");
        assert!(picker.is_empty());
        assert_eq!(picker.choice(), None);
    }

    #[test]
    fn typed_values_are_offered_when_not_listed() {
        let mut picker = picker(&["gpt-5", "gpt-5-mini"]).accepting_typed();
        picker.set_query("gpt-5");
        assert_eq!(picker.choice(), Some(Choice::Item(0)));
        picker.step(5);
        assert_eq!(picker.choice(), Some(Choice::Item(1)));
        picker.set_query("gpt-6 ");
        assert_eq!(picker.choice(), Some(Choice::Typed("gpt-6".into())));
        picker.set_query("");
        picker.select("gpt-5-mini");
        assert_eq!(picker.choice(), Some(Choice::Item(1)));
        picker.select_item(0);
        assert_eq!(picker.choice(), Some(Choice::Item(0)));
    }

    #[test]
    fn rows_follow_the_selection_fit_the_width_and_highlight_it() {
        let items = (0..20)
            .map(|n| {
                let tag = if n == 3 { "current" } else { "" };
                Item::new(format!("model-{n:02}")).cell("128k context").tag(tag)
            })
            .collect();
        let mut picker = Picker::new(items);
        picker.step(12);
        let list = picker.show(40, 5);
        assert_eq!(plain(&list.rows[0]), format!("{:<40}", " model-08  128k context"));
        assert_eq!(list.positions, [Some(8), Some(9), Some(10), Some(11), Some(12)]);
        assert!(
            runs(&list.rows[4]).iter().all(|(style, _)| *style == Style::PLAIN.reversed()),
            "the selection is one reversed bar"
        );
        picker.step(-9);
        let list = picker.show(40, 5);
        assert_eq!(list.positions[0], Some(3), "the view moves only to keep the selection in view");
        assert_eq!(plain(&list.rows[0]), format!("{:<32}current ", " model-03  128k context"));
        for width in 1..30 {
            let list = picker.show(width, 5);
            assert!(list.rows.iter().all(|row| text::width(row) == width), "{width}");
        }
    }

    #[test]
    fn tables_line_up_their_columns_and_leave_out_what_does_not_fit() {
        const COLUMNS: &[Column] = &[
            Column::Label("model"),
            Column::Right("context"),
            Column::Right("in"),
            Column::Left(""),
        ];
        let items = vec![
            Item::new("anthropic/claude-opus-5").cell("1M").cell("$5.00").cell("").tag("current"),
            Item::new("z-ai/glm-5.3").cell("200k").cell("$0.60").cell("text only"),
            Item::new("openrouter/auto").cell("2M").cell("").cell(""),
        ];
        let mut picker = Picker::new(items).columns(COLUMNS);
        assert_eq!(
            rows(&picker.show(80, 10)),
            [
                " model                    context     in",
                " anthropic/claude-opus-5       1M  $5.00                                current",
                " z-ai/glm-5.3                200k  $0.60  text only",
                " openrouter/auto               2M",
            ]
        );
        picker.set_query("glm");
        assert_eq!(
            rows(&picker.show(80, 10))[1],
            " z-ai/glm-5.3                200k  $0.60  text only",
            "filtering keeps the columns where they were"
        );
        picker.set_query("");
        assert_eq!(
            rows(&picker.show(56, 10))[..3],
            [
                " model                    context     in",
                " anthropic/claude-opus-5       1M  $5.00        current",
                " z-ai/glm-5.3                200k  $0.60",
            ],
            "values are left out from the right while the labels keep half the row"
        );
        assert_eq!(
            rows(&picker.show(40, 10))[..2],
            [" model                 context", " anthropic/claude-op…       1M  current"]
        );
        assert_eq!(rows(&picker.show(24, 10))[..2], [" model", " anthropic/cl…  current"]);

        let unpriced = Picker::new(vec![Item::new("gpt-6").cell("").cell("")]).columns(COLUMNS);
        assert_eq!(
            rows(&unpriced.columns(COLUMNS).show(40, 5)),
            [" model", " gpt-6"],
            "value columns with no values are left out with their headings"
        );

        const SESSIONS: &[Column] = &[Column::Label(""), Column::Right(""), Column::Text];
        let item = Item::new("Fix the flaky reconnect test").cell("4m ago").cell("~/work/agt");
        let mut sessions = Picker::new(vec![item]).columns(SESSIONS);
        assert_eq!(
            rows(&sessions.show(60, 5)),
            [" Fix the flaky reconnect test  4m ago  ~/work/agt"]
        );
        assert_eq!(
            rows(&sessions.show(30, 5)),
            [" Fix the flaky recon…  4m ago"],
            "text goes first, then labels are cut, before a value is left out"
        );
    }

    #[test]
    fn groups_head_their_items_until_a_query_is_typed() {
        let items = vec![
            Item::new("/model").cell("choose a model").matched_by("model").group("Commands"),
            Item::new("/new").cell("start a new session").matched_by("new").group("Commands"),
            Item::new("/deploy").cell("ship it").matched_by("deploy").group("Skills"),
        ];
        let mut picker = Picker::new(items);
        let list = picker.show(30, 10);
        assert_eq!(
            rows(&list),
            [
                " Commands",
                " /model   choose a model",
                " /new     start a new session",
                " Skills",
                " /deploy  ship it"
            ]
        );
        assert_eq!(list.positions, [None, Some(0), Some(1), None, Some(2)]);
        picker.step(2);
        assert_eq!(rows(&picker.show(30, 2)), [" Skills", " /deploy  ship it"]);
        picker.set_query("n");
        assert_eq!(
            rows(&picker.show(30, 10)),
            [" /new     start a new session"],
            "a query matches names, not the slash"
        );
        picker.set_query("zzz");
        picker.set_empty("loading the models…");
        assert_eq!(rows(&picker.show(30, 10)), [" loading the models…"]);
    }

    #[test]
    fn a_long_ungrouped_list_shows_only_its_window() {
        let items = (0..50_000).map(|n| Item::new(format!("src/file{n}.rs"))).collect();
        let mut picker = Picker::new(items);
        picker.step(49_998);
        let list = picker.show(30, 3);
        assert_eq!(list.positions, [Some(49_996), Some(49_997), Some(49_998)]);
        assert_eq!(picker.count(), (49_999, 50_000));
    }
}
