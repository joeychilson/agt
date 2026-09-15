//! The transcript: the session as entries the user reads, scrolls and opens.
//!
//! Entries are built from the agent's updates, live and in replays alike: the
//! user's messages, the agent's work, its replies, and notices. Work is what
//! the agent does between speaking: reasoning and calls. While the agent
//! works, each step shows as it happens; once it speaks, or its turn ends,
//! the work folds into one line that a click opens again.
//!
//! Entries are laid out for the width they are drawn at. A finished entry
//! keeps its rows until the width changes or it is opened or closed, so a
//! frame costs what changes rather than the session's length. The view shows
//! the entries from the top, follows the newest rows once they fill it, and
//! stays where the user scrolled while more arrives. The text kept is
//! bounded: past a budget the oldest entries are dropped, and the session log
//! still holds them.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::markdown::{self, Layout, Stream};
use super::screen::Frame;
use super::text::{self, Joins, Line, Row, Style, fit, wrap_text};
use crate::agent::{Origin, Stop, Update};
use crate::bash::{self, CANCELLED, Header, Outcome, Tool, ToolCall};
use crate::models::token_count;
use crate::{image, item};

/// Last output lines a failed command shows while closed, where the error
/// usually is.
const FAILURE_LINES: usize = 5;
/// Output lines a running command shows.
const PROGRESS_LINES: usize = 3;
/// Rows of reasoning shown while the model thinks, and the bytes they are
/// wrapped from.
const THINKING_ROWS: usize = 2;
const THINKING_BYTES: usize = 2048;
/// Steps of the work in progress shown; earlier ones are counted.
const LIVE_STEPS: usize = 6;
/// Rows of a user message shown before it folds, and while it is folded.
const USER_ROWS: usize = 8;
const USER_PREVIEW: usize = 6;
/// Durations shorter than this are not worth showing.
const NOTABLE_TIME: Duration = Duration::from_secs(1);
/// Bytes of text the transcript keeps before dropping its oldest entries.
const RETAINED_BYTES: usize = 8 * 1024 * 1024;
const DROPPED: &str = "earlier entries were dropped from view; the session log keeps them";

/// What a click on the transcript opens in the system's default application.
#[derive(Debug, PartialEq)]
pub(crate) enum Target {
    /// A saved image the model was sent or shown.
    Image(PathBuf),
    /// The web address of a link in a reply.
    Link(String),
}

/// A move of the selection while browsing.
#[derive(Clone, Copy)]
pub(crate) enum Move {
    Previous,
    Next,
    PreviousMessage,
    NextMessage,
    First,
    Last,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum View {
    /// The newest rows, following what arrives.
    Latest,
    /// Rows from `row` rows into entry `entry`, counting its blank row.
    At { entry: usize, row: usize },
}

pub(crate) struct Transcript {
    entries: Vec<Entry>,
    /// The session's working directory, which command titles leave out.
    cwd: PathBuf,
    /// The session's directory, where the images the model was sent are saved.
    session: PathBuf,
    /// Where the response being streamed began, which a reset returns to.
    response: Option<Mark>,
    /// Whether the updates applied replay a logged session, whose work took
    /// no time the transcript can know.
    replaying: bool,
    view: View,
    selected: Option<usize>,
    /// Bytes of text the entries hold.
    bytes: usize,
    /// How the transcript was last drawn: its size, the screen row it started
    /// on, the first row shown and the rows there are.
    width: usize,
    height: usize,
    origin: usize,
    top: usize,
    total: usize,
    /// The rows each entry took when last laid out, with the blank row that
    /// separates it from the one before.
    heights: Vec<usize>,
    /// Text selected with the mouse, and whether its button is still down.
    selection: Option<Selection>,
    dragging: bool,
}

/// A place in the transcript's text: a column of a row of an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Spot {
    entry: usize,
    row: usize,
    column: usize,
}

/// Text selected by dragging, from where the drag started to where it is.
#[derive(Clone, Copy, Debug)]
struct Selection {
    anchor: Spot,
    head: Spot,
}

impl Selection {
    /// The first and last places selected, or `None` when nothing is.
    fn bounds(self) -> Option<(Spot, Spot)> {
        (self.anchor != self.head).then(|| (self.anchor.min(self.head), self.anchor.max(self.head)))
    }

    /// The columns selected in row `row` of entry `entry`, layout included.
    fn columns(self, entry: usize, row: usize) -> Option<Range<usize>> {
        let (start, end) = self.bounds()?;
        let here = (entry, row);
        if here < (start.entry, start.row) || here > (end.entry, end.row) {
            return None;
        }
        let from = if here == (start.entry, start.row) { start.column } else { 0 };
        let to = if here == (end.entry, end.row) { end.column + 1 } else { usize::MAX };
        Some(from..to)
    }
}

/// The entries before a response began, and the steps the work in progress
/// had then.
#[derive(Clone, Copy)]
struct Mark {
    entries: usize,
    work: Option<(usize, usize)>,
}

struct Entry {
    kind: Kind,
    rows: Vec<Row>,
    /// What a click on each row opens.
    hits: Vec<Option<Hit>>,
    /// The width `rows` were laid out for, or zero when they are stale.
    width: usize,
}

/// What a click on a row opens.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Hit {
    /// A step of work, which opens or closes.
    Step(usize),
    /// An image of a message, or of a step's output, which shows outside the
    /// terminal.
    Image { step: Option<usize>, image: usize },
}

enum Kind {
    Notice(String),
    Error(String),
    /// A compaction outside any work, such as one the user asked for.
    Compacted(Compacted),
    User(User),
    Work(Work),
    Reply(Reply),
}

struct User {
    text: String,
    /// The saved images the message carries.
    images: Vec<String>,
    origin: Origin,
    open: bool,
}

struct Work {
    steps: Vec<Step>,
    started: Instant,
    /// How long the work took, once it ended.
    took: Option<Duration>,
    open: bool,
}

enum Step {
    Thinking(Thinking),
    Call(Call),
    Notice(String),
    Compacted(Compacted),
}

/// A compaction that finished, as the row that says what it left.
#[derive(Clone, Copy)]
struct Compacted {
    /// Tokens of context before and after.
    before: u64,
    after: u64,
}

struct Reply {
    source: String,
    done: bool,
    /// The web addresses of the reply's links, which its rows' styles number.
    links: Vec<String>,
    /// How the rows keep up with the source as it streams.
    stream: Stream,
}

struct Thinking {
    text: String,
    started: Instant,
    /// How long the model thought, once it stopped.
    took: Option<Duration>,
    open: bool,
}

struct Call {
    id: String,
    title: String,
    /// The whole command, when the title shows only its first line.
    command: Option<String>,
    started: Instant,
    /// The newest output lines while the call runs.
    progress: Vec<String>,
    end: Option<End>,
    open: bool,
}

/// How a call ended, as the transcript shows it.
struct End {
    outcome: Outcome,
    marker: (&'static str, Style),
    /// What the result adds to a clean exit.
    details: Vec<(String, Style)>,
    lines: Vec<String>,
    style: Style,
    /// Output lines shown while closed, counted from the end.
    shown: usize,
    /// The images the output showed, each after the number of `lines`
    /// before it.
    images: Vec<(usize, Chip)>,
}

/// An image a call's output showed, as the row that opens it.
struct Chip {
    label: String,
    path: PathBuf,
}

impl Transcript {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            cwd: PathBuf::new(),
            session: PathBuf::new(),
            response: None,
            replaying: false,
            view: View::Latest,
            selected: None,
            bytes: 0,
            width: 80,
            height: 24,
            origin: 0,
            top: 0,
            total: 0,
            heights: Vec::new(),
            selection: None,
            dragging: false,
        }
    }

    /// Starts the transcript of a session in `cwd`, kept in `session`.
    pub(crate) fn start(&mut self, cwd: &Path, session: &Path) {
        *self = Self { width: self.width, height: self.height, ..Self::new() };
        self.cwd = cwd.to_path_buf();
        self.session = session.to_path_buf();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn notice(&mut self, text: &str) {
        self.push(Kind::Notice(text.to_owned()));
    }

    pub(crate) fn error(&mut self, text: &str) {
        self.push(Kind::Error(text.to_owned()));
    }

    /// A message the user sent that the agent does not show, such as a
    /// command.
    pub(crate) fn user(&mut self, text: &str) {
        self.settle();
        let text = text.trim_end().to_owned();
        let user = User { text, images: Vec::new(), origin: Origin::User, open: false };
        self.push(Kind::User(user));
    }

    /// Shows a resumed session as it looked when its updates happened.
    pub(crate) fn replay(&mut self, updates: impl IntoIterator<Item = Update>) {
        self.replaying = true;
        for update in updates {
            self.update(update);
        }
        self.update(Update::TurnEnd(Stop::EndTurn));
        self.replaying = false;
    }

    pub(crate) fn update(&mut self, update: Update) {
        match update {
            Update::Text(delta) => self.text(&delta),
            Update::Thinking(delta) => self.thinking(&delta),
            Update::Reset => self.discard_response(),
            Update::ResponseEnd => {
                self.end_thinking();
                self.end_reply();
                self.response = None;
            }
            Update::User { text, images, origin } => {
                self.settle();
                let user = User { text: text.trim_end().to_owned(), images, origin, open: false };
                self.push(Kind::User(user));
            }
            // The status line shows a compaction while it runs.
            Update::Compacting => {}
            // A compaction during work is one of its steps.
            Update::Compacted { before, after } => {
                self.end_thinking();
                self.end_reply();
                let compacted = Compacted { before, after };
                match self.live_work() {
                    Some(work) => work.steps.push(Step::Compacted(compacted)),
                    None => self.push(Kind::Compacted(compacted)),
                }
            }
            Update::ToolStart(call) => {
                self.end_thinking();
                self.end_reply();
                let call = Call::new(&call, &self.cwd);
                self.bytes += call.title.len();
                self.work().steps.push(Step::Call(call));
            }
            Update::ToolProgress { call_id, lines } => {
                if let Some(call) = self.call(&call_id) {
                    call.progress = lines;
                }
            }
            Update::ToolEnd { call_id, output, outcome } => {
                let session = self.session.clone();
                // A call the transcript never showed, such as one a resumed
                // session closes after its replay, stays out of it.
                if let Some(call) = self.call(&call_id) {
                    let end = End::new(&output, outcome, &session);
                    let bytes = end.lines.iter().map(String::len).sum::<usize>();
                    call.end = Some(end);
                    self.bytes += bytes;
                    self.bound();
                }
            }
            Update::Notice(notice) => {
                // A notice during work, such as a retry, is one of its steps.
                if let Some(work) = self.live_work() {
                    let bytes = notice.len();
                    work.steps.push(Step::Notice(notice));
                    self.bytes += bytes;
                } else {
                    self.notice(&notice);
                }
            }
            Update::Error(error) => self.error(&error),
            Update::Usage(_) => {}
            Update::TurnEnd(stop) => {
                self.settle();
                for entry in &mut self.entries {
                    let Kind::Work(work) = &mut entry.kind else { continue };
                    for step in &mut work.steps {
                        if let Step::Call(call @ Call { end: None, .. }) = step {
                            call.end = Some(End::stopped());
                            entry.width = 0;
                        }
                    }
                }
                match stop {
                    Stop::Cancelled => self.notice("interrupted"),
                    Stop::MaxTokens => self.notice("the reply hit the output token limit"),
                    Stop::Refusal => self.notice("the model refused"),
                    Stop::EndTurn | Stop::Error => {}
                }
            }
        }
    }

    /// Rows below the view when it was last drawn.
    pub(crate) fn below(&self) -> usize {
        self.total.saturating_sub(self.top + self.height)
    }

    /// Rows a page scroll moves.
    pub(crate) fn page(&self) -> isize {
        isize::try_from(self.height.saturating_sub(2).max(1)).unwrap_or(1)
    }

    /// Shows the newest rows, following what arrives.
    pub(crate) fn follow(&mut self) {
        self.view = View::Latest;
    }

    /// Moves the view by `rows`, up when negative. Reaching the end follows
    /// new rows again.
    pub(crate) fn scroll(&mut self, rows: isize) {
        self.lay_out(Instant::now(), ' ');
        let top = self.first_row().saturating_add_signed(rows);
        self.show_from(top);
    }

    pub(crate) fn is_browsing(&self) -> bool {
        self.selected.is_some()
    }

    /// Moves the selection, showing the entry it lands on.
    pub(crate) fn select(&mut self, step: Move) {
        let count = self.entries.len();
        if count == 0 {
            return;
        }
        let current = self.selected;
        let is_user = |index: &usize| matches!(self.entries[*index].kind, Kind::User(_));
        let target = match step {
            Move::First => Some(0),
            Move::Last => Some(count - 1),
            Move::Previous => Some(current.map_or(count - 1, |index| index.saturating_sub(1))),
            Move::Next => Some(current.map_or(count - 1, |index| (index + 1).min(count - 1))),
            Move::PreviousMessage => (0..current.unwrap_or(count)).rev().find(is_user),
            Move::NextMessage => (current.map_or(count, |index| index + 1)..count).find(is_user),
        };
        let Some(target) = target else { return };
        self.selected = Some(target);
        self.lay_out(Instant::now(), ' ');
        let start: usize = self.heights[..target].iter().sum();
        let end = start + self.heights[target];
        let top = self.first_row();
        if start < top || self.heights[target] > self.height {
            self.show_from(start);
        } else if end > top + self.height {
            self.show_from(end - self.height);
        }
    }

    pub(crate) fn deselect(&mut self) {
        self.selected = None;
    }

    /// Opens or closes the selected entry.
    pub(crate) fn open_selected(&mut self) -> Option<Target> {
        self.open(self.selected?, None).map(Target::Image)
    }

    /// Opens or closes what column `x` of screen row `y` shows: a step of
    /// opened work, or an entry. Returns what to show outside the terminal:
    /// the saved image of a call or a message, or the address of a link.
    pub(crate) fn click(&mut self, x: usize, y: usize) -> Option<Target> {
        let row = y.checked_sub(self.origin)? + self.top;
        self.lay_out(Instant::now(), ' ');
        let (index, within) = self.entry_at(row)?;
        let entry = &self.entries[index];
        if let Kind::Reply(reply) = &entry.kind {
            let link = entry.rows[within?].style_at(x)?.link()?;
            return reply.links.get(link).cloned().map(Target::Link);
        }
        let hit = entry.hits.get(within?).copied().flatten();
        self.open(index, hit).map(Target::Image)
    }

    /// Starts selecting text at screen column `x` of row `y`, forgetting what
    /// was selected.
    pub(crate) fn press(&mut self, x: usize, y: usize) {
        self.selection = self.spot(x, y).map(|spot| Selection { anchor: spot, head: spot });
        self.dragging = self.selection.is_some();
    }

    pub(crate) fn is_dragging(&self) -> bool {
        self.dragging
    }

    /// Selects text up to screen column `x` of row `y`, while dragging.
    pub(crate) fn drag(&mut self, x: usize, y: usize) {
        let spot = self.spot(x, y);
        if let (Some(selection), Some(spot)) = (&mut self.selection, spot)
            && self.dragging
        {
            selection.head = spot;
        }
    }

    /// Ends a drag, returning the text it selected, which stays selected, or
    /// `None` when it selected nothing, as a click does.
    pub(crate) fn release(&mut self) -> Option<String> {
        self.dragging = false;
        let text = self.selected_text();
        if text.is_none() {
            self.selection = None;
        }
        text
    }

    pub(crate) fn clear_selection(&mut self) {
        self.selection = None;
        self.dragging = false;
    }

    /// Draws the rows in view on `height` rows of `frame` from row `origin`.
    pub(crate) fn draw(
        &mut self,
        frame: &mut Frame,
        origin: usize,
        width: usize,
        height: usize,
        now: Instant,
        spinner: char,
    ) {
        // Rows laid out for another width no longer hold what was selected.
        if width != self.width {
            self.clear_selection();
        }
        self.width = width;
        self.height = height;
        self.origin = origin;
        self.lay_out(now, spinner);
        let top = self.first_row();
        self.show_from(top);
        self.top = top;
        let bottom = top + height;
        let mut end = 0;
        for (index, entry) in self.entries.iter().enumerate() {
            end += self.heights[index];
            if end <= top {
                continue;
            }
            let first = end - entry.rows.len();
            if first >= bottom {
                break;
            }
            for row in first.max(top)..end.min(bottom) {
                let y = origin + row - top;
                let shown = &entry.rows[row - first];
                frame.put(0, y, &shown.line, width);
                if let Some(columns) = self.selection.and_then(|selection| {
                    selection.columns(index, row - first).filter(|_| shown.joins != Joins::Layout)
                }) {
                    let columns =
                        columns.start.max(shown.indent)..columns.end.min(text::width(&shown.line));
                    if !columns.is_empty() {
                        frame.highlight(y, columns);
                    }
                }
                if self.selected == Some(index) {
                    frame.put(0, y, &[(Style::ACCENT, '▌')], width);
                }
            }
        }
    }

    fn push(&mut self, kind: Kind) {
        self.bytes += kind.bytes();
        self.entries.push(Entry { kind, rows: Vec::new(), hits: Vec::new(), width: 0 });
        self.bound();
    }

    /// The entry content row `row` is in, and which of its rows it is, or
    /// `None` for the blank row above it.
    fn entry_at(&self, row: usize) -> Option<(usize, Option<usize>)> {
        let mut end = 0;
        let index = self.heights.iter().position(|height| {
            end += height;
            row < end
        })?;
        // The entry's rows end where its height does, after its blank row.
        Some((index, (row + self.entries[index].rows.len()).checked_sub(end)))
    }

    /// Where screen column `x` of row `y` falls in the text as last laid out.
    /// A row above or below the view counts as its first or last row, and the
    /// blank row above an entry as the entry's start.
    fn spot(&mut self, x: usize, y: usize) -> Option<Spot> {
        self.lay_out(Instant::now(), ' ');
        let top = self.first_row();
        let shown = self.total.saturating_sub(top).min(self.height);
        let row = top + y.saturating_sub(self.origin).min(shown.checked_sub(1)?);
        let (entry, within) = self.entry_at(row)?;
        Some(match within {
            Some(row) => Spot { entry, row, column: x },
            None => Spot { entry, row: 0, column: 0 },
        })
    }

    /// The selected text without layout, with the lines a wrap broke joined
    /// again and entries apart as they are shown.
    fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let (start, end) = selection.bounds()?;
        let mut text = String::new();
        for index in start.entry..=end.entry.min(self.entries.len().checked_sub(1)?) {
            let entry = &self.entries[index];
            if index > start.entry {
                let apart = !joins(&self.entries[index - 1].kind, &entry.kind);
                text.push_str(if apart { "\n\n" } else { "\n" });
            }
            let mut continues = false;
            for (number, row) in entry.rows.iter().enumerate() {
                let Some(columns) = selection.columns(index, number) else { continue };
                match row.joins {
                    Joins::Layout => continue,
                    Joins::Wrap(spaces) if continues => {
                        text.extend(std::iter::repeat_n(' ', spaces))
                    }
                    Joins::Line | Joins::Wrap(_) | Joins::Table if continues => text.push('\n'),
                    Joins::Line | Joins::Wrap(_) | Joins::Table => {}
                }
                continues = true;
                text.push_str(&row.text(columns));
            }
        }
        let text = text.trim_matches('\n');
        (!text.is_empty()).then(|| text.to_owned())
    }

    /// The index of the work in progress, which is always the last entry.
    fn live_work_index(&self) -> Option<usize> {
        let index = self.entries.len().checked_sub(1)?;
        let live = matches!(&self.entries[index].kind, Kind::Work(work) if work.took.is_none());
        live.then_some(index)
    }

    fn live_work(&mut self) -> Option<&mut Work> {
        let index = self.live_work_index()?;
        match &mut self.entries[index].kind {
            Kind::Work(work) => Some(work),
            _ => None,
        }
    }

    /// The work in progress, started when there is none.
    fn work(&mut self) -> &mut Work {
        if self.live_work_index().is_none() {
            let work = Work { steps: Vec::new(), started: Instant::now(), took: None, open: false };
            self.push(Kind::Work(work));
        }
        let Some(Entry { kind: Kind::Work(work), .. }) = self.entries.last_mut() else {
            unreachable!("the work in progress is the last entry");
        };
        work
    }

    fn call(&mut self, id: &str) -> Option<&mut Call> {
        self.entries.iter_mut().rev().find_map(|entry| {
            let Kind::Work(work) = &mut entry.kind else { return None };
            let call = work.steps.iter_mut().find_map(|step| match step {
                Step::Call(call) if call.id == id => Some(call),
                _ => None,
            })?;
            entry.width = 0;
            Some(call)
        })
    }

    fn mark(&mut self) {
        if self.response.is_none() {
            let work = self.live_work_index().map(|index| match &self.entries[index].kind {
                Kind::Work(work) => (index, work.steps.len()),
                _ => (index, 0),
            });
            self.response = Some(Mark { entries: self.entries.len(), work });
        }
    }

    fn text(&mut self, delta: &str) {
        self.mark();
        self.settle_work();
        if let Some(Entry { kind: Kind::Reply(reply), .. }) = self.entries.last_mut()
            && !reply.done
        {
            reply.source.push_str(delta);
            self.bytes += delta.len();
            return;
        }
        self.push(Kind::Reply(Reply::new(delta)));
    }

    fn thinking(&mut self, delta: &str) {
        self.mark();
        self.end_reply();
        self.bytes += delta.len();
        let work = self.work();
        if let Some(Step::Thinking(thinking)) = work.steps.last_mut()
            && thinking.took.is_none()
        {
            return thinking.text.push_str(delta);
        }
        let thinking =
            Thinking { text: delta.to_owned(), started: Instant::now(), took: None, open: false };
        work.steps.push(Step::Thinking(thinking));
    }

    /// Ends the reasoning being streamed, removing it when the model kept it
    /// hidden and it was brief.
    fn end_thinking(&mut self) {
        let replaying = self.replaying;
        let Some(work) = self.live_work() else { return };
        let Some(Step::Thinking(thinking)) = work.steps.last_mut() else { return };
        if thinking.took.is_some() {
            return;
        }
        let took = if replaying { Duration::ZERO } else { thinking.started.elapsed() };
        thinking.took = Some(took);
        if thinking.text.trim().is_empty() && took < NOTABLE_TIME {
            work.steps.pop();
        }
    }

    fn end_reply(&mut self) {
        if let Some(Entry { kind: Kind::Reply(reply), .. }) = self.entries.last_mut()
            && !reply.done
        {
            reply.done = true;
            let len = reply.source.trim_end().len();
            reply.source.truncate(len);
        }
    }

    /// Folds the work in progress, or removes it when it did nothing to show.
    fn settle_work(&mut self) {
        self.end_thinking();
        let replaying = self.replaying;
        let Some(index) = self.live_work_index() else { return };
        let entry = &mut self.entries[index];
        let Kind::Work(work) = &mut entry.kind else { return };
        work.took = Some(if replaying { Duration::ZERO } else { work.started.elapsed() });
        entry.width = 0;
        if work.steps.is_empty() {
            self.entries.remove(index);
            self.forget(index, 1);
        }
    }

    fn settle(&mut self) {
        self.end_reply();
        self.settle_work();
        self.response = None;
    }

    /// Removes what a response whose attempt failed streamed: its replies,
    /// and the reasoning it added to the work in progress, which goes on.
    fn discard_response(&mut self) {
        let Some(mark) = self.response.take() else { return };
        let mut index = self.entries.len();
        while index > mark.entries {
            index -= 1;
            if matches!(self.entries[index].kind, Kind::Reply(_) | Kind::Work(_)) {
                let entry = self.entries.remove(index);
                self.bytes = self.bytes.saturating_sub(entry.kind.bytes());
                self.forget(index, 1);
            }
        }
        let Some((index, steps)) = mark.work else { return };
        let Some(entry) = self.entries.get_mut(index) else { return };
        let before = entry.kind.bytes();
        if let Kind::Work(work) = &mut entry.kind {
            let mut position = 0;
            work.steps.retain(|step| {
                position += 1;
                position <= steps || !matches!(step, Step::Thinking(_))
            });
            work.took = None;
            entry.width = 0;
        }
        self.bytes = self.bytes.saturating_sub(before) + entry.kind.bytes();
    }

    /// Drops the oldest entries while the text kept is over its budget.
    fn bound(&mut self) {
        let Some(oldest) = self.entries.first().filter(|_| self.bytes > RETAINED_BYTES) else {
            return;
        };
        // The notice of dropped entries stays first, and the newest entry stays.
        let first = usize::from(matches!(&oldest.kind, Kind::Notice(text) if text == DROPPED));
        let mut end = first;
        while end + 1 < self.entries.len() && self.bytes > RETAINED_BYTES * 3 / 4 {
            self.bytes = self.bytes.saturating_sub(self.entries[end].kind.bytes());
            end += 1;
        }
        if end == first {
            return;
        }
        self.entries.drain(first..end);
        self.forget(first, end - first);
        if first == 0 {
            let kind = Kind::Notice(DROPPED.into());
            self.entries.insert(0, Entry { kind, rows: Vec::new(), hits: Vec::new(), width: 0 });
            self.shift(0);
        }
    }

    /// Updates indices after `count` entries were removed at `at`.
    fn forget(&mut self, at: usize, count: usize) {
        let fix = |index: usize| {
            if index >= at + count { index - count } else { index.min(at) }
        };
        if let Some(mark) = &mut self.response {
            mark.entries = fix(mark.entries);
            mark.work = mark
                .work
                .filter(|(index, _)| !(at..at + count).contains(index))
                .map(|(index, steps)| (fix(index), steps));
        }
        self.selected = self.selected.map(fix).filter(|index| *index < self.entries.len());
        if let View::At { entry, row } = self.view {
            let moved = fix(entry);
            self.view = View::At { entry: moved, row: if moved == entry { row } else { 0 } };
        }
        // A selection in the removed entries goes, and one after them moves.
        let kept = |spot: Spot| match spot.entry {
            entry if entry < at => Some(spot),
            entry if entry >= at + count => Some(Spot { entry: entry - count, ..spot }),
            _ => None,
        };
        self.selection = self.selection.and_then(|Selection { anchor, head }| {
            Some(Selection { anchor: kept(anchor)?, head: kept(head)? })
        });
        self.dragging &= self.selection.is_some();
    }

    /// Updates indices after an entry was inserted at `at`.
    fn shift(&mut self, at: usize) {
        let fix = |index: usize| if index >= at { index + 1 } else { index };
        if let Some(mark) = &mut self.response {
            mark.entries = fix(mark.entries);
            mark.work = mark.work.map(|(index, steps)| (fix(index), steps));
        }
        self.selected = self.selected.map(fix);
        if let View::At { entry, row } = self.view {
            self.view = View::At { entry: fix(entry), row };
        }
        if let Some(Selection { anchor, head }) = &mut self.selection {
            anchor.entry = fix(anchor.entry);
            head.entry = fix(head.entry);
        }
    }

    /// Opens or closes what `hit` names in entry `index`, or the entry
    /// itself, keeping the rows above it where they are. An image is returned
    /// to show rather than opened.
    fn open(&mut self, index: usize, hit: Option<Hit>) -> Option<PathBuf> {
        self.lay_out(Instant::now(), ' ');
        let top = self.first_row();
        let entry = &mut self.entries[index];
        entry.width = 0;
        let image = match (&mut entry.kind, hit) {
            (Kind::Work(work), Some(Hit::Step(step))) => {
                if let Some(step) = work.steps.get_mut(step) {
                    step.toggle();
                }
                None
            }
            (Kind::Work(work), Some(Hit::Image { step: Some(step), image })) => {
                work.steps.get(step).and_then(|step| step.image(image))
            }
            (Kind::Work(work), None) => {
                if work.took.is_some() && work.opens() {
                    work.open = !work.open;
                }
                None
            }
            (Kind::User(user), Some(Hit::Image { image, .. })) => {
                user.images.get(image).map(|reference| self.session.join(reference))
            }
            (Kind::User(user), None) => {
                user.open = !user.open;
                None
            }
            _ => None,
        };
        self.lay_out(Instant::now(), ' ');
        self.show_from(top);
        image
    }

    /// Lays out every entry at the drawn width, noting the rows each takes
    /// and the rows there are.
    fn lay_out(&mut self, now: Instant, spinner: char) {
        self.heights.clear();
        for index in 0..self.entries.len() {
            let gap = index > 0 && !joins(&self.entries[index - 1].kind, &self.entries[index].kind);
            let entry = &mut self.entries[index];
            entry.lay_out(self.width, now, spinner);
            self.heights.push(entry.rows.len() + usize::from(gap));
        }
        self.total = self.heights.iter().sum();
    }

    /// The first row the view shows, as last laid out.
    fn first_row(&self) -> usize {
        let last = self.total.saturating_sub(self.height);
        match self.view {
            View::Latest => last,
            View::At { entry, row } => {
                let start: usize = self.heights[..entry.min(self.heights.len())].iter().sum();
                (start + row).min(last)
            }
        }
    }

    /// Shows rows from row `top`, or the newest rows once `top` reaches them.
    fn show_from(&mut self, top: usize) {
        let last = self.total.saturating_sub(self.height);
        self.view = View::Latest;
        if top >= last {
            return;
        }
        let mut start = 0;
        for (entry, height) in self.heights.iter().enumerate() {
            if top < start + height {
                self.view = View::At { entry, row: top - start };
                return;
            }
            start += height;
        }
    }
}

/// Whether `next` follows `previous` without a blank row, as consecutive
/// notices do.
fn joins(previous: &Kind, next: &Kind) -> bool {
    matches!((previous, next), (Kind::Notice(_) | Kind::Error(_), Kind::Notice(_) | Kind::Error(_)))
}

impl Compacted {
    /// `✓ Compacted context  182k → 24k tokens`, as a call's row reads.
    fn row(self, width: usize) -> Row {
        let left = format!("{} → {} tokens", token_count(self.before), token_count(self.after));
        Row::from(call_row(("✓", Style::GREEN), "Compacted context", &[(left, Style::DIM)], width))
    }
}

/// The column of an entry's marker and the column its text starts at: a
/// margin and room for the marker, less on a narrow terminal, and neither on
/// the narrowest.
fn columns(width: usize) -> (usize, usize) {
    match width {
        24.. => (1, 3),
        4.. => (0, 2),
        _ => (0, 0),
    }
}

impl Entry {
    /// Lays the entry out for `width`, unless its rows are current.
    fn lay_out(&mut self, width: usize, now: Instant, spinner: char) {
        let stale = self.width != width;
        match &mut self.kind {
            Kind::Reply(reply) => reply.lay_out(&mut self.rows, width),
            kind if stale || kind.is_live() => {
                (self.rows, self.hits) = kind.rows(width, now, spinner);
            }
            _ => {}
        }
        self.width = width;
    }
}

impl Kind {
    fn is_live(&self) -> bool {
        matches!(self, Self::Work(work) if work.took.is_none())
    }

    fn bytes(&self) -> usize {
        match self {
            Self::Notice(text) | Self::Error(text) => text.len(),
            Self::Compacted(_) => 0,
            Self::User(user) => user.text.len(),
            Self::Reply(reply) => reply.source.len(),
            Self::Work(work) => work.steps.iter().map(Step::bytes).sum(),
        }
    }

    /// The entry's rows at `width` with what a click on each opens, other
    /// than a reply's.
    fn rows(&self, width: usize, now: Instant, spinner: char) -> (Vec<Row>, Vec<Option<Hit>>) {
        let (marker, body) = columns(width);
        let inner = width.saturating_sub(body + marker);
        let rows = match self {
            Self::Notice(text) => wrap_text(&format!("· {text}"), Style::DIM, inner, 2),
            Self::Error(text) => wrap_text(&format!("✗ {text}"), Style::RED, inner, 2),
            Self::Compacted(compacted) => vec![compacted.row(inner)],
            Self::User(user) => return user.rows(width),
            Self::Work(work) => {
                let (rows, steps) = work.rows(inner, now, spinner);
                return (rows.into_iter().map(|row| row.indent(body)).collect(), steps);
            }
            Self::Reply(_) => Vec::new(),
        };
        (rows.into_iter().map(|row| row.indent(body)).collect(), Vec::new())
    }
}

impl User {
    /// The message's rows, with the image each chip row shows, which a click
    /// on it opens.
    fn rows(&self, width: usize) -> (Vec<Row>, Vec<Option<Hit>>) {
        let (marker, body) = columns(width);
        // A message `agt send` delivered says so after its marker.
        let tag = if self.origin == Origin::Send && body >= 2 { "[agt send] " } else { "" };
        let inner = width.saturating_sub(body + marker + tag.len());
        // The text is shown without the spaces an attached image left around it.
        let text = self.text.trim();
        // A message of only images starts with them.
        let mut rows =
            if text.is_empty() { Vec::new() } else { wrap_text(text, Style::BOLD, inner, 0) };
        if rows.len() > USER_ROWS && !self.open {
            let hidden = rows.len() - USER_PREVIEW;
            rows.truncate(USER_PREVIEW);
            rows.push(Row::layout(fit(&[(&format!("… {hidden} more lines"), Style::DIM)], inner)));
        }
        let mut images = vec![None; rows.len()];
        for (index, image) in self.images.iter().enumerate() {
            rows.push(Row::layout(fit(&[(&image_label(image), Style::CODE)], inner)));
            images.push(Some(Hit::Image { step: None, image: index }));
        }
        let rows = rows.into_iter().enumerate().map(|(number, row)| {
            if number == 0 && body >= 2 {
                let mark = [(Style::ACCENT, '❯'), (Style::PLAIN, ' ')].into_iter();
                row.after(mark.chain(tag.chars().map(|c| (Style::DIM, c)))).indent(marker)
            } else {
                row.indent(body + tag.len())
            }
        });
        (rows.collect(), images)
    }
}

/// A saved image as its chip shows it, such as `[image 1499×1162]`.
fn image_label(reference: &str) -> String {
    match image::size(reference) {
        Some((width, height)) => format!("[image {width}×{height}]"),
        None => "[image]".to_owned(),
    }
}

/// An image a command showed as its chip shows it, by what its header says:
/// the name of its file, the original's size and the region shown, such as
/// `[image shot.png · 2880×1800]`.
fn shown_label(header: &image::Header<'_>) -> String {
    let name = Path::new(header.path)
        .file_name()
        .map_or_else(|| header.path.into(), |name| name.to_string_lossy());
    let (width, height) = header.size;
    match header.region {
        Some(region) => format!("[image {name} · {width}×{height} · region {region}]"),
        None => format!("[image {name} · {width}×{height}]"),
    }
}

impl Work {
    /// Whether opening the work shows anything its summary does not.
    fn opens(&self) -> bool {
        self.steps.iter().any(|step| match step {
            Step::Thinking(thinking) => !thinking.text.trim().is_empty(),
            Step::Call(_) | Step::Notice(_) | Step::Compacted(_) => true,
        })
    }

    fn rows(&self, width: usize, now: Instant, spinner: char) -> (Vec<Row>, Vec<Option<Hit>>) {
        let mut rows = vec![Row::from(self.header(width, now, spinner))];
        let mut hits = vec![None];
        let live = self.took.is_none();
        if !live && !self.open {
            return (rows, hits);
        }
        let gutter = width >= 6;
        let inner = if gutter { width - 2 } else { width };
        let mut push = |row: Row, hit: Option<Hit>| {
            rows.push(if gutter {
                row.after([(Style::DIM, '│'), (Style::PLAIN, ' ')])
            } else {
                row
            });
            hits.push(hit);
        };
        // Work that only reasoned opens straight to what it thought, rather
        // than to a list of thoughts that each open again.
        if !live && self.steps.iter().all(|step| matches!(step, Step::Thinking(_))) {
            let texts = self.steps.iter().filter_map(|step| match step {
                Step::Thinking(thinking) if !thinking.text.trim().is_empty() => Some(thinking),
                _ => None,
            });
            for (index, thinking) in texts.enumerate() {
                if index > 0 {
                    push(Row::from(Vec::new()), None);
                }
                for row in thinking.text(inner) {
                    push(row, None);
                }
            }
            return (rows, hits);
        }
        let first = if live { self.steps.len().saturating_sub(LIVE_STEPS) } else { 0 };
        if first > 0 {
            let earlier = match first {
                1 => "… 1 earlier step".to_owned(),
                count => format!("… {count} earlier steps"),
            };
            push(Row::layout(fit(&[(&earlier, Style::DIM)], inner)), None);
        }
        for (index, step) in self.steps.iter().enumerate().skip(first) {
            for (row, image) in step.rows(inner, now, spinner) {
                let hit = match image {
                    Some(image) => Hit::Image { step: Some(index), image },
                    None => Hit::Step(index),
                };
                push(row, Some(hit));
            }
        }
        (rows, hits)
    }

    /// `⠹ Working · 12s` while the work goes on, then what it did, such as
    /// `▸ Worked for 38s · 6 commands · 1 failed`.
    fn header(&self, width: usize, now: Instant, spinner: char) -> Line {
        let Some(took) = self.took else {
            let elapsed = now.duration_since(self.started);
            let elapsed = if elapsed >= NOTABLE_TIME {
                format!(" · {}", bash::elapsed(elapsed))
            } else {
                String::new()
            };
            let spinner = spinner.to_string();
            let spans =
                [(&*spinner, Style::YELLOW), (" Working", Style::PLAIN), (&*elapsed, Style::DIM)];
            return fit(&spans, width);
        };
        let calls: Vec<&Call> = self
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::Call(call) => Some(call),
                _ => None,
            })
            .collect();
        let ended = |outcome: Outcome| {
            calls
                .iter()
                .filter(|call| call.end.as_ref().is_some_and(|end| end.outcome == outcome))
                .count()
        };
        let images: usize =
            calls.iter().filter_map(|call| call.end.as_ref()).map(|end| end.images.len()).sum();
        let commands = calls.len();
        let verb = if calls.is_empty() { "Thought" } else { "Worked" };
        let mut parts = vec![(
            if took >= NOTABLE_TIME {
                format!("{verb} for {}", bash::elapsed(took))
            } else {
                verb.to_owned()
            },
            Style::DIM,
        )];
        for (count, noun, style) in
            [(commands, "command", Style::DIM), (images, "image", Style::DIM)]
        {
            match count {
                0 => {}
                1 => parts.push((format!("1 {noun}"), style)),
                count => parts.push((format!("{count} {noun}s"), style)),
            }
        }
        if self.steps.iter().any(|step| matches!(step, Step::Compacted(_))) {
            parts.push(("compacted".to_owned(), Style::DIM));
        }
        let failed = ended(Outcome::Failed);
        if failed > 0 {
            parts.push((format!("{failed} failed"), Style::RED));
        }
        if ended(Outcome::Cancelled) > 0 {
            parts.push(("interrupted".to_owned(), Style::YELLOW));
        }
        let marker = match (self.opens(), self.open) {
            (false, _) => " ",
            (true, false) => "▸",
            (true, true) => "▾",
        };
        let mut spans = vec![(marker, Style::DIM), (" ", Style::PLAIN)];
        for (index, (text, style)) in parts.iter().enumerate() {
            if index > 0 {
                spans.push((" · ", Style::DIM));
            }
            spans.push((text.as_str(), *style));
        }
        fit(&spans, width)
    }
}

impl Step {
    fn bytes(&self) -> usize {
        match self {
            Self::Thinking(thinking) => thinking.text.len(),
            Self::Call(call) => {
                let output =
                    call.end.as_ref().map_or(0, |end| end.lines.iter().map(String::len).sum());
                call.title.len() + output
            }
            Self::Notice(text) => text.len(),
            Self::Compacted(_) => 0,
        }
    }

    fn toggle(&mut self) {
        match self {
            Self::Thinking(thinking)
                if thinking.took.is_some() && !thinking.text.trim().is_empty() =>
            {
                thinking.open = !thinking.open;
            }
            Self::Call(call) if call.opens() => call.open = !call.open,
            _ => {}
        }
    }

    /// The saved file of the step's image `image`.
    fn image(&self, image: usize) -> Option<PathBuf> {
        match self {
            Self::Call(Call { end: Some(end), .. }) => {
                end.images.get(image).map(|(_, chip)| chip.path.clone())
            }
            _ => None,
        }
    }

    /// The step's rows, each with the image it shows, if any.
    fn rows(&self, width: usize, now: Instant, spinner: char) -> Vec<(Row, Option<usize>)> {
        let rows = match self {
            Self::Thinking(thinking) => thinking.rows(width, now, spinner),
            Self::Call(call) => return call.rows(width, now, spinner),
            Self::Notice(text) => wrap_text(&format!("· {text}"), Style::DIM, width, 2),
            Self::Compacted(compacted) => vec![compacted.row(width)],
        };
        rows.into_iter().map(|row| (row, None)).collect()
    }
}

impl Reply {
    fn new(source: &str) -> Self {
        Self {
            source: source.to_owned(),
            done: false,
            links: Vec::new(),
            stream: Stream::default(),
        }
    }

    /// Brings the reply's rows up to date with its source at `width`.
    fn lay_out(&mut self, rows: &mut Vec<Row>, width: usize) {
        let (marker, body) = columns(width);
        let layout = Layout {
            indent: body,
            width: width.saturating_sub(body + marker),
            style: Style::PLAIN,
        };
        self.stream.lay_out(&self.source, self.done, layout, rows, &mut self.links);
    }
}

/// The indent of rows under a step's first row, and the width left for them.
fn nested(width: usize) -> (usize, usize) {
    if width > 2 { (2, width - 2) } else { (0, width) }
}

impl Thinking {
    fn rows(&self, width: usize, now: Instant, spinner: char) -> Vec<Row> {
        let (hang, inner) = nested(width);
        let Some(took) = self.took else {
            // While the model thinks: the section it is writing, and its end.
            let (heading, text) = thinking_section(&self.text);
            let elapsed = now.duration_since(self.started);
            let elapsed = if elapsed >= NOTABLE_TIME {
                format!(" · {}", bash::elapsed(elapsed))
            } else {
                String::new()
            };
            let spinner = spinner.to_string();
            let heading = heading.unwrap_or("Thinking");
            let spans = [
                (&*spinner, Style::YELLOW),
                (" ", Style::PLAIN),
                (heading, Style::THINKING),
                (&*elapsed, Style::DIM),
            ];
            let mut rows = vec![Row::from(fit(&spans, width))];
            let tail = &text[text.ceil_char_boundary(text.len().saturating_sub(THINKING_BYTES))..];
            let tail = tail.replace("**", "");
            if !tail.trim().is_empty() {
                let wrapped = wrap_text(tail.trim(), Style::THINKING, inner, 0);
                let shown = &wrapped[wrapped.len().saturating_sub(THINKING_ROWS)..];
                rows.extend(shown.iter().map(|row| row.clone().indent(hang)));
            }
            return rows;
        };
        let label = if took >= NOTABLE_TIME {
            format!("Thought for {}", bash::elapsed(took))
        } else {
            "Thought".to_owned()
        };
        let marker = match (self.text.trim().is_empty(), self.open) {
            (true, _) => " ",
            (false, false) => "▸",
            (false, true) => "▾",
        };
        let spans = [(marker, Style::DIM), (" ", Style::PLAIN), (&*label, Style::THINKING)];
        let mut rows = vec![Row::from(fit(&spans, width))];
        if self.open {
            rows.extend(self.text(inner).into_iter().map(|row| row.indent(hang)));
        }
        rows
    }

    /// The reasoning's text as Markdown, `width` columns wide, in the
    /// reasoning style. A click on reasoning opens or closes it, so its links
    /// open nothing.
    fn text(&self, width: usize) -> Vec<Row> {
        let mut rows = Vec::new();
        let layout = Layout { indent: 0, width, style: Style::THINKING };
        markdown::render(self.text.trim(), layout, &mut rows, &mut Vec::new());
        rows
    }
}

impl Call {
    fn new(call: &ToolCall, cwd: &Path) -> Self {
        let command = match &call.tool {
            Tool::Bash(bash::Action::Start { command, .. }) => {
                let command = command.trim();
                command.contains('\n').then(|| command.to_owned())
            }
            _ => None,
        };
        Self {
            id: call.id.clone(),
            title: call.headline(cwd),
            command,
            started: Instant::now(),
            progress: Vec::new(),
            end: None,
            open: false,
        }
    }

    /// Whether opening the call shows more than it shows closed.
    fn opens(&self) -> bool {
        self.end.as_ref().is_some_and(|end| end.lines.len() > end.shown || self.command.is_some())
    }

    /// The call's rows, each with the image of its output it shows, if any.
    /// Images show as chips: closed, all of them under the call's row, and
    /// opened, where the output showed them.
    fn rows(&self, width: usize, now: Instant, spinner: char) -> Vec<(Row, Option<usize>)> {
        let (hang, inner) = nested(width);
        let Some(end) = &self.end else {
            let spinner = spinner.to_string();
            let elapsed = now.duration_since(self.started);
            let details: Vec<(String, Style)> = (elapsed >= NOTABLE_TIME)
                .then(|| (bash::elapsed(elapsed), Style::DIM))
                .into_iter()
                .collect();
            let head = call_row((&spinner, Style::YELLOW), &self.title, &details, width);
            let progress = &self.progress[self.progress.len().saturating_sub(PROGRESS_LINES)..];
            let progress = progress
                .iter()
                .map(|text| (Row::from(fit(&[(text, Style::DIM)], inner)).indent(hang), None));
            return std::iter::once((Row::from(head), None)).chain(progress).collect();
        };
        let mut rows =
            vec![(Row::from(call_row(end.marker, &self.title, &end.details, width)), None)];
        let chip = |(index, (_, chip)): (usize, &(usize, Chip))| {
            (Row::layout(fit(&[(&chip.label, Style::CODE)], inner)).indent(hang), Some(index))
        };
        let text = |text: &String, style| {
            wrap_text(text, style, inner, 0).into_iter().map(|row| (row.indent(hang), None))
        };
        if self.open {
            if let Some(command) = &self.command {
                rows.extend(text(command, Style::CODE));
            }
            let mut images = end.images.iter().enumerate().peekable();
            for (number, line) in end.lines.iter().enumerate() {
                while let Some(image) = images.next_if(|(_, (before, _))| *before <= number) {
                    rows.push(chip(image));
                }
                rows.extend(text(line, end.style));
            }
            rows.extend(images.map(chip));
        } else {
            rows.extend(end.images.iter().enumerate().map(chip));
            let shown = &end.lines[end.lines.len().saturating_sub(end.shown)..];
            rows.extend(
                shown
                    .iter()
                    .map(|line| (Row::from(fit(&[(line, end.style)], inner)).indent(hang), None)),
            );
        }
        rows
    }
}

impl End {
    /// How a call that returned `output` ended.
    fn new(output: &Value, outcome: Outcome, session: &Path) -> Self {
        let marker = match outcome {
            Outcome::Ok => ("✓", Style::GREEN),
            Outcome::Failed => ("✗", Style::RED),
            Outcome::Cancelled => ("■", Style::YELLOW),
        };
        // The output's text, and the images it showed at the offsets of the
        // text they follow.
        let mut text = String::new();
        let mut shown = Vec::new();
        match output {
            Value::Array(parts) => {
                for part in parts {
                    match image::reference(part) {
                        Some(reference) => shown.push((text.len(), reference)),
                        None => text.push_str(part["text"].as_str().unwrap_or_default()),
                    }
                }
            }
            output => text = item::content_text(output),
        }
        let (header, rest) = match Header::parse(&text) {
            Some((header, rest)) => (Some(header), rest),
            None => (None, text.as_str()),
        };
        let body = rest.trim_end();
        let body = body.strip_suffix(CANCELLED).unwrap_or(body).trim_matches('\n');
        let start = text.len() - rest.len() + (rest.len() - rest.trim_start_matches('\n').len());
        let all: Vec<&str> = body.lines().collect();
        // The header line an image follows shows as the image's chip, named
        // by what the header says.
        let mut headers = vec![false; all.len()];
        let mut images = Vec::with_capacity(shown.len());
        for (at, reference) in shown {
            let before = text.get(start..at).map_or(0, |span| span.lines().count()).min(all.len());
            let named = before
                .checked_sub(1)
                .and_then(|line| Some((line, image::Header::parse(all[line])?)));
            let label = match named {
                Some((line, named)) => {
                    headers[line] = true;
                    shown_label(&named)
                }
                None => image_label(reference),
            };
            images.push((before, Chip { label, path: session.join(reference) }));
        }
        let images = images
            .into_iter()
            .map(|(before, chip)| {
                (before - headers[..before].iter().filter(|&&header| header).count(), chip)
            })
            .collect();
        let lines: Vec<String> = all
            .iter()
            .zip(&headers)
            .filter(|(_, header)| !**header)
            .map(|(line, _)| (*line).to_owned())
            .collect();
        let mut details = Vec::new();
        let (style, shown) = match (outcome, &header) {
            (Outcome::Ok, Some(Header::Process { id, exit: None, .. })) => {
                details.push((format!("running · id {id}"), Style::YELLOW));
                (Style::DIM, 0)
            }
            (Outcome::Ok, _) => (Style::DIM, 0),
            (Outcome::Failed, Some(Header::Process { exit: Some(exit), .. })) => {
                details.push((exit.to_string(), Style::RED));
                (Style::DIM, FAILURE_LINES)
            }
            // Without a process's header, the output is agt's own error message.
            (Outcome::Failed, _) => (Style::RED, FAILURE_LINES),
            (Outcome::Cancelled, _) => {
                details.push(("interrupted".to_owned(), Style::YELLOW));
                (Style::DIM, 0)
            }
        };
        if lines.len() > shown {
            let count = match lines.len() {
                1 => "1 line".to_owned(),
                count => format!("{count} lines"),
            };
            details.push((count, Style::DIM));
        }
        let ran = match header {
            Some(Header::Process { ran, .. } | Header::Waited(ran)) => ran,
            None => Duration::ZERO,
        };
        if ran >= NOTABLE_TIME {
            details.push((bash::elapsed(ran), Style::DIM));
        }
        Self { outcome, marker, details, lines, style, shown, images }
    }

    /// The end of a call still running when its turn ended.
    fn stopped() -> Self {
        Self {
            outcome: Outcome::Cancelled,
            marker: ("■", Style::YELLOW),
            details: vec![("interrupted".to_owned(), Style::YELLOW)],
            lines: Vec::new(),
            style: Style::DIM,
            shown: 0,
            images: Vec::new(),
        }
    }
}

/// A call's first row: its marker, its title cut to fit, then details.
fn call_row(marker: (&str, Style), title: &str, details: &[(String, Style)], width: usize) -> Line {
    let mut spans = Vec::new();
    for (index, (text, style)) in details.iter().enumerate() {
        spans.push((if index == 0 { "  " } else { " · " }, Style::DIM));
        spans.push((text.as_str(), *style));
    }
    let details = fit(&spans, width / 2);
    let head = [marker, (" ", Style::PLAIN), (title, Style::PLAIN)];
    let mut row = fit(&head, width - text::width(&details));
    row.extend(details);
    row
}

/// The heading of the reasoning section being written, and the text after it.
/// Reasoning summaries open each section with a line in bold.
fn thinking_section(text: &str) -> (Option<&str>, &str) {
    let mut section = (None, text);
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        offset += line.len();
        let heading = line
            .trim()
            .strip_prefix("**")
            .and_then(|rest| rest.strip_suffix("**"))
            .filter(|heading| !heading.is_empty() && !heading.contains("**"));
        if heading.is_some() {
            section = (heading, &text[offset..]);
        }
    }
    section
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The rows the transcript shows at `width` in a tall view, without
    /// styles or trailing spaces.
    fn shown(transcript: &mut Transcript, width: usize) -> Vec<String> {
        let height = 300;
        let mut frame = Frame::new(width, height);
        transcript.draw(&mut frame, 0, width, height, Instant::now(), '*');
        (0..transcript.total).map(|y| frame.row(y).trim_end().to_owned()).collect()
    }

    fn transcript() -> Transcript {
        let mut transcript = Transcript::new();
        transcript.start(Path::new("/work"), Path::new("/agt/sessions/1"));
        transcript
    }

    fn start(transcript: &mut Transcript, id: &str, arguments: &Value) {
        let call = ToolCall::new(id.into(), "bash", arguments.to_string());
        transcript.update(Update::ToolStart(call));
    }

    fn end(transcript: &mut Transcript, id: &str, output: impl Into<Value>, outcome: Outcome) {
        let output = output.into();
        transcript.update(Update::ToolEnd { call_id: id.into(), output, outcome });
    }

    #[test]
    fn rows_fit_narrow_terminals() {
        for width in 1..40 {
            let mut transcript = transcript();
            transcript.user("wide 界 input");
            transcript.update(Update::Thinking("considering 界 options".into()));
            start(&mut transcript, "call", &json!({ "command": "echo 界" }));
            let lines = vec!["tool output 界".into()];
            transcript.update(Update::ToolProgress { call_id: "call".into(), lines });
            transcript.width = width;
            transcript.lay_out(Instant::now(), '*');
            end(&mut transcript, "call", "[id 1 · exit 1 · 1.0s]\noutput 界", Outcome::Failed);
            transcript.update(Update::Text("a **bold** line\na streamed tail 界".into()));
            transcript.update(Update::TurnEnd(Stop::EndTurn));
            if let Some(Entry { kind: Kind::Work(work), width, .. }) = transcript.entries.get_mut(1)
            {
                work.open = true;
                *width = 0;
            }
            transcript.lay_out(Instant::now(), '*');
            for entry in &transcript.entries {
                let rows: Vec<String> =
                    entry.rows.iter().map(|row| text::plain(&row.line)).collect();
                assert!(
                    entry.rows.iter().all(|row| text::width(&row.line) <= width),
                    "{width}: {rows:?}"
                );
            }
        }
    }

    #[test]
    fn work_shows_its_steps_live_and_folds_when_the_agent_speaks() {
        let mut transcript = transcript();
        transcript.user("fix the test");
        transcript.update(Update::Thinking("**Reading**\n\nthe test".into()));
        assert_eq!(
            shown(&mut transcript, 60),
            [" ❯ fix the test", "", "   * Working", "   │ * Reading", "   │   the test"]
        );
        start(&mut transcript, "c1", &json!({ "command": "cargo test" }));
        let rows = shown(&mut transcript, 60);
        assert_eq!(rows[3..], ["   │ ▸ Thought", "   │ * cargo test"]);
        end(&mut transcript, "c1", "[id 1 · exit 101 · 4.2s]\none\ntwo", Outcome::Failed);
        start(&mut transcript, "c2", &json!({ "command": "sed -n 1,9p a.rs" }));
        end(&mut transcript, "c2", "[id 2 · exit 0 · 0.1s]\n1\n2\n3", Outcome::Ok);
        assert_eq!(
            shown(&mut transcript, 60)[4..],
            [
                "   │ ✗ cargo test  exit 101 · 4.2s",
                "   │   one",
                "   │   two",
                "   │ ✓ sed -n 1,9p a.rs  3 lines",
            ]
        );
        transcript.update(Update::Text("Fixed it.".into()));
        transcript.update(Update::ResponseEnd);
        transcript.update(Update::TurnEnd(Stop::EndTurn));
        assert_eq!(
            shown(&mut transcript, 60),
            [" ❯ fix the test", "", "   ▸ Worked · 2 commands · 1 failed", "", "   Fixed it."]
        );
        assert_eq!(transcript.click(0, 2), None);
        assert_eq!(
            shown(&mut transcript, 60)[2..5],
            [
                "   ▾ Worked · 2 commands · 1 failed",
                "   │ ▸ Thought",
                "   │ ✗ cargo test  exit 101 · 4.2s"
            ]
        );
    }

    #[test]
    fn compactions_say_what_they_left_on_their_own_or_as_a_step_of_work() {
        let mut transcript = transcript();
        transcript.user("/compact");
        transcript.update(Update::Compacting);
        transcript.update(Update::Compacted { before: 15_240, after: 2_133 });
        assert_eq!(
            shown(&mut transcript, 60),
            [" ❯ /compact", "", "   ✓ Compacted context  15k → 2.1k tokens"]
        );
        start(&mut transcript, "c1", &json!({ "command": "npm test" }));
        end(&mut transcript, "c1", "[id 1 · exit 0 · 0.1s]", Outcome::Ok);
        transcript.update(Update::Compacting);
        transcript.update(Update::Compacted { before: 176_000, after: 3_100 });
        assert_eq!(
            shown(&mut transcript, 60)[5..],
            ["   │ ✓ npm test", "   │ ✓ Compacted context  176k → 3.1k tokens"]
        );
        transcript.update(Update::TurnEnd(Stop::EndTurn));
        assert_eq!(shown(&mut transcript, 60)[4], "   ▸ Worked · 1 command · compacted");
    }

    #[test]
    fn calls_show_what_their_results_add() {
        let call = |arguments: Value, output: Value, outcome| {
            let mut transcript = transcript();
            start(&mut transcript, "c", &arguments);
            end(&mut transcript, "c", output, outcome);
            let rows = shown(&mut transcript, 80);
            rows[1..]
                .iter()
                .map(|row| row.trim_start_matches("   │ ").to_owned())
                .collect::<Vec<_>>()
        };
        let long = format!("[id 1 · exit 0 · 0.2s]\n{}", "x\n".repeat(40));
        assert_eq!(
            call(json!({ "command": "cd /work && sed -n 1,40p a.rs" }), long.into(), Outcome::Ok),
            ["✓ sed -n 1,40p a.rs  40 lines"]
        );
        assert_eq!(
            call(
                json!({ "command": "git status --short" }),
                "[id 2 · exit 0 · 1.5s]\n M a.rs".into(),
                Outcome::Ok
            ),
            ["✓ git status --short  1 line · 1.5s"]
        );
        let failure: String = (1..=10).map(|n| format!("\nline {n}")).collect();
        assert_eq!(
            call(
                json!({ "command": "cargo test" }),
                format!("[id 3 · exit 101 · 4.2s]{failure}").into(),
                Outcome::Failed
            ),
            [
                "✗ cargo test  exit 101 · 10 lines · 4.2s",
                "  line 6",
                "  line 7",
                "  line 8",
                "  line 9",
                "  line 10"
            ]
        );
        assert_eq!(
            call(
                json!({ "command": "npm run dev", "wait": 1 }),
                "[id 4 · running · 1.0s · you will be told when it exits]\nready".into(),
                Outcome::Ok
            ),
            ["✓ npm run dev  running · id 4 · 1 line · 1.0s"]
        );
        assert_eq!(
            call(
                json!({ "command": "sleep 30" }),
                "[id 5 · running · 3.1s]\n[cancelled by the user]".into(),
                Outcome::Cancelled
            ),
            ["■ sleep 30  interrupted · 3.1s"]
        );
        let mut invalid = transcript();
        let call = ToolCall::new("c".into(), "bash", r#"{"cmd":"ls"}"#.into());
        invalid.update(Update::ToolStart(call));
        end(&mut invalid, "c", "error: unknown argument \"cmd\"", Outcome::Failed);
        assert_eq!(
            shown(&mut invalid, 80)[1..],
            [r#"   │ ✗ {"cmd":"ls"}"#, r#"   │   error: unknown argument "cmd""#]
        );

        let mut viewed = transcript();
        start(&mut viewed, "v", &json!({ "command": "agt view shot.png" }));
        let header = "[image /work/shot.png · 2880x1800 · shown at 1980x1238, scale 1.45]";
        let mut output = item::Content::from(format!("[id 6 · exit 0 · 0.4s]\n{header}\n"));
        output.push_image("images/ab-1980x1238.png".into());
        end(&mut viewed, "v", output.into_output(), Outcome::Ok);
        assert_eq!(
            shown(&mut viewed, 80)[1..],
            ["   │ ✓ agt view shot.png", "   │   [image shot.png · 2880×1800]"],
            "an image shows as a chip instead of its header"
        );
        let saved = PathBuf::from("/agt/sessions/1/images/ab-1980x1238.png");
        assert_eq!(viewed.click(0, 2), Some(Target::Image(saved)));
    }

    #[test]
    fn opening_a_call_shows_its_command_and_all_its_output() {
        let mut transcript = transcript();
        start(&mut transcript, "c", &json!({ "command": "cat > a <<'EOF'\nbody\nEOF\nwc -l a" }));
        let output: String = (1..=3).map(|n| format!("{n}\n")).collect();
        end(&mut transcript, "c", format!("[id 1 · exit 0 · 0.1s]\n{output}"), Outcome::Ok);
        assert_eq!(shown(&mut transcript, 40)[1], "   │ ✓ cat > a <<'EOF' …  3 lines");
        assert_eq!(transcript.click(0, 1), None);
        assert_eq!(
            shown(&mut transcript, 40)[1..],
            [
                "   │ ✓ cat > a <<'EOF' …  3 lines",
                "   │   cat > a <<'EOF'",
                "   │   body",
                "   │   EOF",
                "   │   wc -l a",
                "   │   1",
                "   │   2",
                "   │   3"
            ]
        );

        let mut rendered = self::transcript();
        start(&mut rendered, "r", &json!({ "command": "./render.sh" }));
        let mut output = item::Content::from("[id 2 · exit 0 · 0.1s]\nrendering\n");
        for name in ["a", "b"] {
            output.push_str(&format!("[image /work/{name}.png · 4x2]\n"));
            output.push_image(format!("images/{name}-4x2.png"));
        }
        output.push_str("done");
        end(&mut rendered, "r", output.into_output(), Outcome::Ok);
        assert_eq!(
            shown(&mut rendered, 40)[1..],
            [
                "   │ ✓ ./render.sh  2 lines",
                "   │   [image a.png · 4×2]",
                "   │   [image b.png · 4×2]"
            ],
            "closed, a call shows its images"
        );
        assert_eq!(rendered.click(0, 1), None);
        assert_eq!(
            shown(&mut rendered, 40)[2..],
            [
                "   │   rendering",
                "   │   [image a.png · 4×2]",
                "   │   [image b.png · 4×2]",
                "   │   done"
            ],
            "opened, its images are where its output showed them"
        );
        let saved = PathBuf::from("/agt/sessions/1/images/b-4x2.png");
        assert_eq!(rendered.click(0, 4), Some(Target::Image(saved)));
    }

    #[test]
    fn reasoning_shows_its_heading_and_opens_to_its_text() {
        assert_eq!(
            thinking_section("**Reading**\n\nfiles\n\n**Planning**\n\nfix it"),
            (Some("Planning"), "\nfix it")
        );
        assert_eq!(thinking_section("no heading"), (None, "no heading"));

        let mut transcript = transcript();
        transcript.update(Update::Thinking(String::new()));
        assert_eq!(shown(&mut transcript, 80)[1], "   │ * Thinking");
        transcript.update(Update::Thinking("**Planning**\n\nfix **it**".into()));
        assert_eq!(shown(&mut transcript, 80)[1..], ["   │ * Planning", "   │   fix it"]);
        let Some(Entry { kind: Kind::Work(work), .. }) = transcript.entries.last_mut() else {
            panic!("work in progress")
        };
        work.started = Instant::now() - Duration::from_secs(3);
        let Some(Step::Thinking(thinking)) = work.steps.last_mut() else { panic!("reasoning") };
        thinking.started = Instant::now() - Duration::from_secs(2);
        transcript.update(Update::Text("done".into()));
        let rows = shown(&mut transcript, 80);
        assert!(rows[0].starts_with("   ▸ Thought for 3."), "{rows:?}");
        assert_eq!(rows[1..], ["", "   done"]);
        transcript.click(0, 0);
        let rows = shown(&mut transcript, 80);
        assert!(rows[0].starts_with("   ▾ Thought for 3."), "{rows:?}");
        assert_eq!(
            rows[1..4],
            ["   │ Planning", "   │", "   │ fix it"],
            "work that only reasoned opens straight to its text"
        );

        // Among calls, reasoning keeps a row of its own that opens to its text.
        let mut mixed = self::transcript();
        mixed.update(Update::Thinking("**Planning**\n\nfix it".into()));
        start(&mut mixed, "c", &json!({ "command": "ls" }));
        end(&mut mixed, "c", "[id 1 · exit 0 · 0.1s]", Outcome::Ok);
        mixed.update(Update::TurnEnd(Stop::EndTurn));
        mixed.click(0, 0);
        assert_eq!(shown(&mut mixed, 80)[1..3], ["   │ ▸ Thought", "   │ ✓ ls"]);
        mixed.click(0, 1);
        assert_eq!(shown(&mut mixed, 80)[2..5], ["   │   Planning", "   │", "   │   fix it"]);
    }

    #[test]
    fn a_reset_discards_the_failed_attempt() {
        let mut transcript = transcript();
        transcript.user("go");
        transcript.update(Update::Thinking("hmm".into()));
        transcript.update(Update::Text("partial".into()));
        transcript.update(Update::Notice("stream dropped; retrying in 1.0s (attempt 1)".into()));
        transcript.update(Update::Reset);
        transcript.update(Update::Text("whole".into()));
        transcript.update(Update::ResponseEnd);
        transcript.update(Update::TurnEnd(Stop::EndTurn));
        assert_eq!(
            shown(&mut transcript, 80),
            [" ❯ go", "", "   · stream dropped; retrying in 1.0s (attempt 1)", "", "   whole"]
        );
    }

    #[test]
    fn the_view_starts_at_the_top_and_follows_new_rows_until_scrolled_away() {
        let mut transcript = transcript();
        let mut frame = Frame::new(40, 10);
        let mut draw = |transcript: &mut Transcript| {
            frame = Frame::new(40, 10);
            transcript.draw(&mut frame, 0, 40, 10, Instant::now(), '*');
            (frame.row(0).trim_end().to_owned(), frame.row(9).trim_end().to_owned())
        };
        transcript.user("first");
        let rows = draw(&mut transcript);
        assert_eq!(
            rows,
            (" ❯ first".to_owned(), String::new()),
            "a short transcript sits at the top"
        );
        for n in 0..30 {
            transcript.notice(&format!("notice {n}"));
        }
        assert_eq!(draw(&mut transcript).1, "   · notice 29");
        transcript.scroll(-5);
        assert_eq!(draw(&mut transcript).1, "   · notice 24");
        transcript.notice("notice 30");
        assert_eq!(
            draw(&mut transcript).1,
            "   · notice 24",
            "new rows leave the view where it is"
        );
        assert_eq!(transcript.below(), 6);
        transcript.scroll(100);
        assert_eq!(draw(&mut transcript).1, "   · notice 30");
        transcript.notice("notice 31");
        assert_eq!(draw(&mut transcript).1, "   · notice 31", "the end follows new rows again");
    }

    #[test]
    fn browsing_selects_entries_and_jumps_between_messages() {
        let mut transcript = transcript();
        for n in 0..3 {
            transcript.user(&format!("message {n}"));
            transcript.update(Update::Text(format!("reply {n}")));
            transcript.update(Update::ResponseEnd);
        }
        transcript.select(Move::Last);
        transcript.select(Move::PreviousMessage);
        assert_eq!(transcript.selected, Some(4));
        transcript.select(Move::PreviousMessage);
        transcript.select(Move::PreviousMessage);
        assert_eq!(transcript.selected, Some(0), "stays on the first message");
        transcript.select(Move::Next);
        assert_eq!(transcript.selected, Some(1));
        assert_eq!(shown(&mut transcript, 40)[2], "▌  reply 0");
    }

    #[test]
    fn a_click_on_a_link_in_a_reply_opens_its_address() {
        let mut transcript = transcript();
        transcript.update(Update::Text("Read [the docs](https://docs.rs/agt) first.".into()));
        transcript.update(Update::ResponseEnd);
        assert_eq!(shown(&mut transcript, 40), ["   Read the docs first."]);
        assert_eq!(transcript.click(9, 0), Some(Target::Link("https://docs.rs/agt".into())));
        assert_eq!(transcript.click(4, 0), None, "text that links nowhere opens nothing");
    }

    #[test]
    fn a_drag_selects_text_that_copies_without_layout_and_with_wrapped_lines_joined() {
        let mut transcript = transcript();
        transcript.user("explain");
        let reply = "one two three four five six seven\n\n```sh\ncargo build --release\n```";
        transcript.update(Update::Text(reply.into()));
        transcript.update(Update::ResponseEnd);
        let draw = |transcript: &mut Transcript| {
            let mut frame = Frame::new(24, 12);
            transcript.draw(&mut frame, 0, 24, 12, Instant::now(), '*');
            frame
        };
        let frame = draw(&mut transcript);
        let rows: Vec<String> = (0..9).map(|y| frame.row(y).trim_end().to_owned()).collect();
        let (open, close) =
            (format!("   ─ sh {}", "─".repeat(15)), format!("   {}", "─".repeat(20)));
        assert_eq!(
            rows,
            [
                " ❯ explain",
                "",
                "   one two three four",
                "   five six seven",
                "",
                open.as_str(),
                "   cargo build",
                "   --release",
                close.as_str(),
            ]
        );
        transcript.press(3, 2);
        transcript.drag(11, 7);
        assert_eq!(
            transcript.release().as_deref(),
            Some("one two three four five six seven\n\ncargo build --release")
        );
        let frame = draw(&mut transcript);
        let bar = [(3, 2), (8, 6), (11, 7), (2, 2), (3, 5)]
            .map(|(x, y)| frame.style(x, y) == Style::PLAIN.reversed());
        assert_eq!(
            bar,
            [true, true, true, false, false],
            "one bar over text and the spaces in it, and none over margins or rules"
        );

        transcript.press(5, 2);
        transcript.drag(9, 0);
        assert_eq!(transcript.release().as_deref(), Some("n\n\none"), "backwards, across entries");
        transcript.press(3, 2);
        assert_eq!(transcript.release(), None, "a click selects nothing");
        assert_eq!(draw(&mut transcript).style(3, 2), Style::PLAIN, "and clears the selection");
    }

    #[test]
    fn long_messages_fold_and_their_images_open_on_click() {
        let mut transcript = transcript();
        let text: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        let images = vec!["images/ab-1499x1162.png".into(), "images/cd-4x2.png".into()];
        // An attached image leaves a space before the text it was attached to.
        transcript.update(Update::User { text: format!(" {text}"), images, origin: Origin::User });
        let rows = shown(&mut transcript, 40);
        assert_eq!(rows[0], " ❯ line 1");
        assert_eq!(
            rows[5..],
            ["   line 6", "   … 14 more lines", "   [image 1499×1162]", "   [image 4×2]"]
        );
        let saved = PathBuf::from("/agt/sessions/1/images/cd-4x2.png");
        assert_eq!(transcript.click(0, 8), Some(Target::Image(saved)));
        assert_eq!(
            shown(&mut transcript, 40).len(),
            9,
            "opening an image leaves the message folded"
        );
        transcript.click(0, 0);
        assert_eq!(shown(&mut transcript, 40).len(), 22);
    }

    #[test]
    fn old_entries_are_dropped_past_the_budget_and_the_newest_stays() {
        let mut transcript = transcript();
        transcript.notice(&"x".repeat(RETAINED_BYTES + 1));
        assert_eq!(transcript.entries.len(), 1, "nothing older was there to drop");
        let block = "x".repeat(1024 * 1024);
        for n in 0..30 {
            transcript.notice(&format!("{n} {block}"));
            assert!(transcript.bytes <= RETAINED_BYTES, "{} bytes kept", transcript.bytes);
        }
        let is_dropped =
            |entry: &Entry| matches!(&entry.kind, Kind::Notice(text) if text == DROPPED);
        assert!(is_dropped(&transcript.entries[0]));
        let newest = &transcript.entries[transcript.entries.len() - 1].kind;
        assert!(matches!(newest, Kind::Notice(text) if text.starts_with("29 ")));
        assert_eq!(transcript.entries.iter().filter(|entry| is_dropped(entry)).count(), 1);
    }

    /// Rows with their styles as escape sequences written `\e`, for goldens.
    fn styled(rows: &[Line]) -> String {
        let mut out = Vec::new();
        for row in rows {
            let mut current = Style::PLAIN;
            for &(style, c) in row {
                if style != current {
                    style.select(&mut out);
                    current = style;
                }
                out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            }
            if current != Style::PLAIN {
                Style::PLAIN.select(&mut out);
            }
            out.push(b'\n');
        }
        String::from_utf8(out).expect("UTF-8").replace('\x1b', "\\e")
    }

    /// Checks `actual` against `tests/support/golden/<name>`, as the end-to-end
    /// goldens do; `AGT_BLESS=1` writes it instead.
    fn golden(name: &str, actual: &str) {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/golden");
        let path = dir.join(name);
        if std::env::var_os("AGT_BLESS").is_some() {
            std::fs::create_dir_all(&dir).expect("create goldens");
            std::fs::write(&path, actual).expect("write golden");
            return;
        }
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("no golden {name}; run with AGT_BLESS=1 to write it"));
        if expected != actual {
            let line = expected
                .lines()
                .zip(actual.lines())
                .position(|(expected, actual)| expected != actual)
                .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
            panic!(
                "{name} differs from its golden at line {}:\nexpected: {}\n  actual: {}\nrun with AGT_BLESS=1 if the change is intended",
                line + 1,
                expected.lines().nth(line).unwrap_or("<end>"),
                actual.lines().nth(line).unwrap_or("<end>"),
            );
        }
    }

    #[test]
    fn replayed_sessions_render_as_they_did() {
        use crate::item::Item;

        let parse = |value: Value| Item::from_value(value).expect("an object");
        let call = |id: &str, arguments: Value| {
            parse(
                json!({ "type": "function_call", "call_id": id, "name": "bash", "arguments": arguments.to_string() }),
            )
        };
        let reply = |text: &str| {
            parse(
                json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": text }] }),
            )
        };
        // What only a replay shows, around a call of each kind and a failed one;
        // how calls and Markdown render is covered where they are shown live.
        let history = [
            Item::user(vec![item::input_text(
                "<checkpoint>\nEarlier conversation was compacted.\n</checkpoint>",
            )]),
            parse(json!({ "type": "compaction", "encrypted_content": "sealed" })),
            Item::user(vec![
                item::input_text("[image /work/shot.png · 4x2]\n"),
                json!({ "type": "input_image", "image_url": "images/ab-4x2.png", "detail": "high" }),
                item::input_text(
                    "fix the header\n\n<skill_content name=\"deploy\">steps</skill_content>",
                ),
            ]),
            parse(
                json!({ "type": "reasoning", "content": [{ "type": "reasoning_text", "text": "**Planning**\n\nread the css" }] }),
            ),
            call("c1", json!({ "command": "cd /work && npm run dev", "wait": 1 })),
            Item::output(
                "c1",
                "[id 1 · running · 1.0s · you will be told when it exits]\nready".into(),
            ),
            call("c2", json!({ "command": "agt view --region 0,0,2,1 shot.png" })),
            Item::output(
                "c2",
                json!([
                    item::input_text("[id 3 · exit 0 · 0.2s]\n[image /work/shot.png · 4x2 · region 0,0,2,1]\n"),
                    { "type": "input_image", "image_url": "images/cd-2x1.png", "detail": "high" },
                ]),
            ),
            call("c3", json!({ "command": "cargo test" })),
            Item::output("c3", "[id 2 · exit 101 · 4.2s]\nerror: 1 test failed".into()),
            Item::user(vec![item::input_text(
                "<background>\n[2026-09-15 14:02 UTC] process 1 (npm run dev) finished with exit 0\n</background>",
            )]),
            reply("Done."),
        ];
        let mut transcript = transcript();
        // A resumed session's context begins with the compaction it came from,
        // and a message `agt send` delivered follows the reply.
        let replayed =
            history.iter().flat_map(|item| crate::agent::replay_item(item, Origin::User));
        let sent =
            Update::User { text: "also run lint".into(), images: Vec::new(), origin: Origin::Send };
        transcript.replay(
            std::iter::once(Update::Notice("earlier conversation was compacted".into()))
                .chain(replayed)
                .chain([sent]),
        );
        for entry in &mut transcript.entries {
            if let Kind::Work(work) = &mut entry.kind {
                work.open = true;
                entry.width = 0;
            }
        }
        transcript.lay_out(Instant::now(), '*');
        let mut rows: Vec<Line> = Vec::new();
        for (entry, height) in transcript.entries.iter().zip(&transcript.heights) {
            rows.extend(std::iter::repeat_n(Vec::new(), height - entry.rows.len()));
            rows.extend(entry.rows.iter().map(|row| row.line.clone()));
        }
        golden("tui-replay.txt", &styled(&rows));
    }
}
