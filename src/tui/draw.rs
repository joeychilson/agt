//! Drawing the app: the header, the transcript or the welcome, the dock of
//! waiting messages, input box and status line under it, and the open menu as
//! a panel over the transcript.

use std::borrow::Cow;
use std::ops::Range;
use std::time::{Duration, Instant};

use super::completion::Completing;
use super::screen::Frame;
use super::text::{self, Line, Style, fit, indent, pad};
use super::{App, FLASH, SPINNER_INTERVAL};
use crate::agent::{Activity, Agent, Delivery};
use crate::models::token_count;
use crate::{bash, mcp};

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// The most choices a menu lists at once.
const LIST_ROWS: usize = 10;
/// The most rows that list messages waiting for the agent.
const QUEUED_ROWS: usize = 3;
/// The most rows the input box grows to.
const INPUT_ROWS: usize = 10;
/// The least rows that fit a header and blank rows around the transcript.
const ROOMY: usize = 12;
const HINTS: &str = "/ commands · @ files · ctrl-v image · ctrl-o browse";
const BROWSING: &str = "↑↓ entries · shift-↑↓ your messages · enter opens · esc returns";
const WELCOME: &str = "enter send · shift-enter new line · / commands · @ files · ? keys";
/// The widest the welcome is drawn.
const WELCOME_WIDTH: usize = 80;
/// The most rows of skills the welcome lists.
const SKILL_ROWS: usize = 8;
/// The most rows of MCP servers the welcome lists.
const SERVER_ROWS: usize = 4;

/// Where the last frame drew a menu, for the mouse.
pub(super) struct Panel {
    pub(super) rows: Range<usize>,
    /// The screen row of each listed choice, and its position in the list.
    pub(super) choices: Vec<(usize, usize)>,
    /// The rows of a sign-in address, which a click copies.
    pub(super) url: Range<usize>,
    /// The row of the model menu's tabs, and the columns each tab takes.
    pub(super) tabs: Option<(usize, Vec<Range<usize>>)>,
}

impl App {
    /// Draws the screen on `frame`: the header, the transcript, the dock under
    /// it, and the open menu over the transcript.
    pub(super) fn paint(&mut self, frame: &mut Frame, now: Instant) {
        let (width, height) = (frame.width(), frame.height());
        let spinner = spinner(now.duration_since(self.started));
        let (dock, cursor) = self.dock(width, height, spinner, now);
        let dock_top = height.saturating_sub(dock.len());
        let roomy = height >= ROOMY;
        let origin = if roomy { 2 } else { 0 };
        let bottom = dock_top.saturating_sub(usize::from(roomy)).max(origin);
        self.transcript_rows = origin..bottom;
        if roomy {
            frame.put(0, 0, &self.header(width), width);
        }
        if self.transcript.is_empty() {
            for (index, line) in self.welcome(width, bottom - origin).iter().enumerate() {
                frame.put(0, origin + index, line, width);
            }
        } else {
            self.transcript.draw(frame, origin, width, bottom - origin, now, spinner);
            let below = self.transcript.below();
            if roomy && below > 0 {
                let more = format!(" ↓ {below} more · page down ");
                let pill = fit(&[(&more, Style::DIM.reversed())], width);
                frame.put(width.saturating_sub(text::width(&pill) + 1), bottom, &pill, width);
            }
        }
        for (index, line) in dock.iter().enumerate() {
            frame.put(0, dock_top + index, line, width);
        }
        // The status line is the dock's last row, unless the terminal is too
        // small for one; a click on its count of running processes lists them.
        let (right, counted) = self.status_right(width);
        let status_row = dock_top + dock.len().saturating_sub(1);
        self.running = (counted > 0 && dock.len() > 1 && status_row < height).then(|| {
            let start = width - text::width(&right);
            (status_row, start..start + counted)
        });
        // A menu that searches takes the cursor; otherwise it stays in the input.
        let cursor = match self.draw_menu(frame, width, dock_top) {
            Some(search) => Some(search),
            None => cursor.map(|(row, column)| (column, dock_top + row)),
        };
        if let Some((x, y)) = cursor {
            frame.cursor(x, y);
        }
    }

    /// The header: agt and its version, the working directory and its branch,
    /// then the model, effort, provider, context used and cost.
    fn header(&self, width: usize) -> Line {
        let mut right: Vec<(String, Style)> = Vec::new();
        match &self.agent {
            Some(agent) => {
                let settings = agent.settings();
                right.push((settings.model.id.clone(), Style::PLAIN));
                if let Some(effort) = settings.reasoning {
                    right.push((format!(" {}", effort.as_str()), Style::DIM));
                }
                right.push((format!(" · {}", settings.endpoint.provider.spec().name), Style::DIM));
                let (used, budget) = (token_count(self.usage.used), token_count(self.usage.budget));
                right.push((format!(" · {used} / {budget}"), Style::DIM));
                if let Some(spent) = self.usage.spent() {
                    right.push((format!(" · {spent}"), Style::DIM));
                }
            }
            None => right.push(("no model".to_owned(), Style::DIM)),
        }
        right.push((" ".to_owned(), Style::PLAIN));
        let right: Vec<(&str, Style)> =
            right.iter().map(|(text, style)| (text.as_str(), *style)).collect();
        let right = fit(&right, width * 2 / 3);
        let branch = self.branch.as_deref().unwrap_or_default();
        let left = [
            (" agt", Style::ACCENT),
            (" ", Style::PLAIN),
            (env!("CARGO_PKG_VERSION"), Style::DIM),
            ("  ", Style::PLAIN),
            (self.place.as_str(), Style::DIM),
            ("  ", Style::PLAIN),
            (branch, Style::DIM),
        ];
        let room = width.saturating_sub(text::width(&right));
        let mut row = fit(&left, room.saturating_sub(1));
        pad(&mut row, room, Style::PLAIN);
        row.extend(right);
        row
    }

    /// What a new session shows, as one block in the middle of `height`
    /// rows: the skills and MCP servers the agent has, then the keys to start
    /// with. The header already names agt's version, the directory and the
    /// model.
    fn welcome(&self, width: usize, height: usize) -> Vec<Line> {
        let room = width.min(WELCOME_WIDTH);
        // The keys and the blank row above them take two rows.
        let mut rows = match &self.agent {
            Some(agent) => {
                let skills = Catalog {
                    label: "skills",
                    rows: SKILL_ROWS,
                    kind: "skills",
                    hint: "type / to see them",
                    entries: agent
                        .skills()
                        .iter()
                        .map(|skill| (skill.name.as_str(), Cow::from(skill.description.as_str())))
                        .collect(),
                };
                let servers = Catalog {
                    label: "mcp",
                    rows: SERVER_ROWS,
                    kind: "servers",
                    hint: "/mcp lists them",
                    entries: agent
                        .servers()
                        .iter()
                        .map(|server| (server.name.as_str(), server_description(server)))
                        .collect(),
                };
                catalog_rows(&[skills, servers], height.saturating_sub(2), room)
            }
            None => vec![fit(&[("choose a provider and model to start", Style::DIM)], room)],
        };
        if !rows.is_empty() {
            rows.push(Vec::new());
        }
        rows.push(fit(&[(WELCOME, Style::DIM)], room));
        rows.truncate(height);
        let block = rows.iter().map(|row| text::width(row)).max().unwrap_or(0);
        let (left, top) = (width.saturating_sub(block) / 2, height.saturating_sub(rows.len()) / 2);
        std::iter::repeat_n(Vec::new(), top)
            .chain(rows.into_iter().map(|row| indent(row, left)))
            .collect()
    }

    /// The rows under the transcript: waiting messages, the input box and the
    /// status line, with the cursor's position among them.
    pub(super) fn dock(
        &self,
        width: usize,
        height: usize,
        spinner: char,
        now: Instant,
    ) -> (Vec<Line>, Option<(usize, usize)>) {
        if width < 12 || height < 5 {
            return self.bare_input(width);
        }
        let focused = self.menu.is_none() && !self.transcript.is_browsing();
        let border = if focused { Style::CODE } else { Style::DIM };
        let inner = width - 6;
        let mut rows = self.queued_rows(width, height);
        let (texts, cursor) = self.input_rows(inner, height);
        rows.push(edge(width, ['╭', '╮'], border));
        let first = rows.len();
        for (index, text) in texts.into_iter().enumerate() {
            let prompt = if index == 0 { (Style::ACCENT, '❯') } else { (Style::PLAIN, ' ') };
            let mut row = vec![(border, '│'), (Style::PLAIN, ' '), prompt, (Style::PLAIN, ' ')];
            row.extend(text);
            pad(&mut row, width - 1, Style::PLAIN);
            row.push((border, '│'));
            rows.push(row);
        }
        rows.push(edge(width, ['╰', '╯'], border));
        rows.push(self.status(width, spinner, now));
        let cursor = focused.then_some((first + cursor.0, 4 + cursor.1));
        (rows, cursor)
    }

    /// The input alone on one row, for a terminal too small for the box.
    fn bare_input(&self, width: usize) -> (Vec<Line>, Option<(usize, usize)>) {
        match &self.menu {
            Some(menu) => {
                let input = menu.input();
                let cursor = text::str_width(&input).min(width.saturating_sub(1));
                (vec![fit(&[(&input, Style::PLAIN)], width)], Some((0, cursor)))
            }
            None => {
                let layout = self.editor.layout(width.max(1));
                let mut row = layout.rows[layout.cursor.0].clone();
                text::truncate(&mut row, width);
                (vec![row], Some((0, layout.cursor.1.min(width.saturating_sub(1)))))
            }
        }
    }

    /// Messages waiting for the agent, oldest first, with when each is sent.
    fn queued_rows(&self, width: usize, height: usize) -> Vec<Line> {
        let Some(agent) = &self.agent else { return Vec::new() };
        let queued = agent.queued();
        let room = (height / 8).min(QUEUED_ROWS);
        if queued.is_empty() || room == 0 {
            return Vec::new();
        }
        let listed = if queued.len() > room { room - 1 } else { queued.len() };
        let margin = usize::from(width >= 24) * 3;
        let mut rows: Vec<Line> = queued[..listed]
            .iter()
            .map(|queued| {
                let when = match queued.delivery {
                    Delivery::Next => " next step ",
                    Delivery::Later => " when done ",
                };
                let when = fit(&[(when, Style::DIM)], width / 3);
                let room = width.saturating_sub(margin + text::width(&when));
                let first = queued.message.typed.lines().next().unwrap_or_default();
                let mut row =
                    indent(fit(&[("↳ ", Style::DIM), (first, Style::PLAIN)], room), margin);
                pad(&mut row, width - text::width(&when), Style::PLAIN);
                row.extend(when);
                row
            })
            .collect();
        if listed < queued.len() {
            let more = format!("+{} more waiting", queued.len() - listed);
            rows.push(indent(fit(&[(&more, Style::DIM)], width.saturating_sub(margin)), margin));
        }
        rows
    }

    /// The rows inside the input box, `inner` columns wide, with the cursor's
    /// position among them: the draft, or what to do.
    fn input_rows(&self, inner: usize, height: usize) -> (Vec<Line>, (usize, usize)) {
        if self.editor.text().is_empty() {
            let hint = if self.agent.is_none() {
                "choose a provider to start"
            } else if self.busy() {
                "Message for the next step · tab sends it when the agent is done"
            } else {
                "Ask anything"
            };
            return (vec![fit(&[(hint, Style::DIM)], inner)], (0, 0));
        }
        let mut layout = self.editor.layout(inner);
        let limit = (height / 3).clamp(1, INPUT_ROWS);
        let start = layout.cursor.0.saturating_sub(limit - 1);
        let end = (start + limit).min(layout.rows.len());
        let cursor = (layout.cursor.0 - start, layout.cursor.1);
        layout.rows.truncate(end);
        layout.rows.drain(..start);
        (layout.rows, cursor)
    }

    /// The status line: a passing message, what the agent is doing, or the
    /// keys to know; then processes running in the background.
    fn status(&self, width: usize, spinner: char, now: Instant) -> Line {
        let margin = usize::from(width >= 24) * 3;
        let spinner = spinner.to_string();
        let elapsed = self
            .busy_since
            .map(|since| bash::elapsed(now.duration_since(since)))
            .unwrap_or_default();
        let activity = self.agent.as_ref().and_then(Agent::activity).map(word);
        let flash =
            self.flash.as_ref().filter(|(_, at)| now < *at + FLASH).map(|(text, _)| text.as_str());
        let left: Vec<(&str, Style)> = match (flash, activity) {
            (Some(flash), _) => vec![(flash, Style::PLAIN)],
            (None, Some(activity)) => vec![
                (&spinner, Style::YELLOW),
                (" ", Style::PLAIN),
                (activity, Style::PLAIN),
                (" · ", Style::DIM),
                (&elapsed, Style::DIM),
                (" · esc to stop", Style::DIM),
            ],
            (None, None) if self.preparing > 0 => {
                vec![(&spinner, Style::YELLOW), (" Attaching…", Style::PLAIN)]
            }
            (None, None) if self.transcript.is_browsing() => vec![(BROWSING, Style::DIM)],
            (None, None) if self.agent.is_none() => {
                vec![("choose a provider to start", Style::DIM)]
            }
            (None, None) => vec![(HINTS, Style::DIM)],
        };
        let (right, _) = self.status_right(width);
        let room = width.saturating_sub(margin + text::width(&right) + 1);
        let mut row = indent(fit(&left, room), margin);
        pad(&mut row, width - text::width(&right), Style::PLAIN);
        row.extend(right);
        row
    }

    /// The right end of the status line: how many processes run in the
    /// background, then the key list while the agent is idle and no menu is
    /// open. Also returns the columns the count takes.
    fn status_right(&self, width: usize) -> (Line, usize) {
        let running = self.agent.as_ref().map_or(0, Agent::background);
        let running = if running > 0 { format!("{running} running ") } else { String::new() };
        let idle = self.agent.as_ref().and_then(Agent::activity).is_none();
        let keys = if idle && self.menu.is_none() { "? keys " } else { "" };
        let right = fit(&[(&running, Style::YELLOW), (keys, Style::DIM)], width / 3);
        let counted = text::str_width(running.trim_end()).min(text::width(&right));
        (right, counted)
    }

    /// Draws the open menu or completion as a panel ending just above row
    /// `bottom`, and returns where the cursor goes when the menu has a search
    /// row.
    fn draw_menu(
        &mut self,
        frame: &mut Frame,
        width: usize,
        bottom: usize,
    ) -> Option<(usize, usize)> {
        self.panel = None;
        if width < 12 || bottom < 4 {
            return None;
        }
        let inner = width - 2;
        let (title, search, keys, preview, lists, url, tabs) = match (&self.menu, &self.completion)
        {
            (Some(menu), _) => (
                menu.title(),
                Some((menu.input(), menu.hint())),
                menu.keys(),
                menu.preview(self.agent.as_ref(), inner.saturating_sub(2)),
                menu.lists(),
                menu.url().is_some(),
                menu.tabs().map(|(providers, tab)| {
                    let names: Vec<&str> =
                        providers.iter().map(|provider| provider.spec().name).collect();
                    (names, tab)
                }),
            ),
            (None, Some(completion)) => {
                let (title, keys) = match completion.completing {
                    Completing::Command if completion.picker.is_empty() => return None,
                    Completing::Command => {
                        ("Commands", "↑↓ choose · enter run · tab complete · esc close")
                    }
                    Completing::File => ("Files", "↑↓ choose · enter insert · esc close"),
                };
                let preview = if completion.listed {
                    Vec::new()
                } else {
                    vec![fit(&[("listing files…", Style::DIM)], inner.saturating_sub(2))]
                };
                (title.to_owned(), None, keys, preview, completion.listed, false, None)
            }
            (None, None) => return None,
        };
        let chrome =
            2 + usize::from(search.is_some()) + usize::from(tabs.is_some()) + preview.len();
        let room = bottom.saturating_sub(chrome);
        let menu = self.menu.is_some();
        let list = match (lists, self.list()) {
            (true, Some(list)) if room > 0 => {
                let limit = room.min(LIST_ROWS);
                let mut shown = list.show(inner, limit);
                // A menu keeps its height while typing filters its list, and
                // across its tabs, so its borders stay where they are.
                if menu {
                    let steady = if tabs.is_some() { limit } else { list.height().min(limit) };
                    shown.rows.resize(steady.max(shown.rows.len()), Vec::new());
                    shown.positions.resize(shown.rows.len(), None);
                }
                Some(shown)
            }
            _ => None,
        };
        let count = self.list().map(|list| list.count()).filter(|(_, total)| *total > LIST_ROWS);
        let count =
            count.map(|(selected, total)| format!("{selected} of {total}")).unwrap_or_default();
        let height = (chrome + list.as_ref().map_or(0, |list| list.rows.len())).min(bottom);
        let top = bottom - height;
        let mut rows =
            vec![titled(width, ['╭', '╮'], &[(&title, Style::BOLD)], &[(&count, Style::DIM)])];
        let side = |row: Line| {
            let mut line = vec![(Style::DIM, '│')];
            line.extend(row);
            pad(&mut line, width - 1, Style::PLAIN);
            line.push((Style::DIM, '│'));
            line
        };
        let mut tab_columns = None;
        if let Some((names, shown)) = &tabs {
            // Columns count from the panel's left edge, which the border takes.
            let mut row: Line = vec![(Style::PLAIN, ' ')];
            let mut at = 2;
            let mut columns = Vec::with_capacity(names.len());
            for (index, name) in names.iter().enumerate() {
                let style = if index == *shown { Style::ACCENT.reversed() } else { Style::DIM };
                let tab = fit(&[(" ", style), (name, style), (" ", style)], usize::MAX);
                let end = at + text::width(&tab);
                columns.push(at..end);
                row.extend(tab);
                row.push((Style::PLAIN, ' '));
                at = end + 1;
            }
            text::truncate(&mut row, inner);
            tab_columns = Some((top + rows.len(), columns));
            rows.push(side(row));
        }
        let mut cursor = None;
        if let Some((input, hint)) = &search {
            let shown = if input.is_empty() {
                fit(&[(hint, Style::DIM)], inner - 3)
            } else {
                fit(&[(input, Style::PLAIN)], inner - 3)
            };
            let row = [(Style::PLAIN, ' '), (Style::DIM, '⌕'), (Style::PLAIN, ' ')];
            rows.push(side(row.into_iter().chain(shown).collect()));
            let column = if input.is_empty() { 0 } else { text::str_width(input) };
            cursor = Some(((4 + column).min(width - 2), top + rows.len() - 1));
        }
        let mut choices = Vec::new();
        if let Some(list) = list {
            for (row, position) in list.rows.into_iter().zip(list.positions) {
                if let Some(position) = position {
                    choices.push((top + rows.len(), position));
                }
                rows.push(side(row));
            }
        }
        let url_start = top + rows.len();
        rows.extend(preview.into_iter().map(|row| side(indent(row, 1))));
        let url = if url { url_start..top + rows.len() } else { 0..0 };
        rows.push(titled(width, ['╰', '╯'], &[(keys, Style::DIM)], &[]));
        rows.truncate(height.max(2));
        for (index, row) in rows.iter().enumerate() {
            frame.put(0, top + index, row, width);
        }
        self.panel = Some(Panel { rows: top..top + rows.len(), choices, url, tabs: tab_columns });
        cursor
    }
}

/// The word the status line says for what the agent is doing.
fn word(activity: Activity) -> &'static str {
    match activity {
        Activity::Thinking => "Thinking…",
        Activity::Writing => "Writing…",
        Activity::Working => "Working…",
        Activity::Retrying => "Retrying…",
        Activity::Compacting => "Compacting…",
    }
}

/// A list the welcome shows under its label, a name and description per
/// entry.
struct Catalog<'a> {
    label: &'static str,
    /// The most rows it takes.
    rows: usize,
    /// What its entries are, for the count of those that do not fit.
    kind: &'static str,
    /// How to see the entries that do not fit.
    hint: &'static str,
    entries: Vec<(&'a str, Cow<'a, str>)>,
}

/// The welcome's rows of the `catalogs` that have entries, one after another
/// with a blank row between them, in at most `rows` rows. Names line up across
/// catalogs, and entries that do not fit are counted.
fn catalog_rows(catalogs: &[Catalog], rows: usize, room: usize) -> Vec<Line> {
    let mut left = rows;
    let mut shown: Vec<(&Catalog, usize)> = Vec::new();
    for catalog in catalogs.iter().filter(|catalog| !catalog.entries.is_empty()) {
        let gap = usize::from(!shown.is_empty());
        let limit = catalog.rows.min(left.saturating_sub(gap));
        if limit == 0 {
            break;
        }
        let total = catalog.entries.len();
        shown.push((catalog, if total > limit { limit - 1 } else { total }));
        left -= gap + total.min(limit);
    }
    let names = shown.iter().flat_map(|(catalog, listed)| &catalog.entries[..*listed]);
    let names = names.map(|(name, _)| text::str_width(name)).max().unwrap_or(0).min(room / 3);
    let labels = shown.iter().map(|(catalog, _)| catalog.label.len()).max().unwrap_or(0) + 2;
    let mut lines = Vec::new();
    for (catalog, listed) in shown {
        if !lines.is_empty() {
            lines.push(Vec::new());
        }
        let mut entries: Vec<Line> = catalog.entries[..listed]
            .iter()
            .map(|(name, description)| {
                let mut line = fit(&[(name, Style::PLAIN)], names);
                pad(&mut line, names + 2, Style::PLAIN);
                line.extend(fit(&[(description, Style::DIM)], usize::MAX));
                line
            })
            .collect();
        let more = catalog.entries.len() - listed;
        if more > 0 {
            let count = match listed {
                0 => format!("{more} {} · {}", catalog.kind, catalog.hint),
                _ => format!("+{more} more · {}", catalog.hint),
            };
            entries.push(fit(&[(&count, Style::DIM)], usize::MAX));
        }
        // The label heads the first row, and the rest line up after it.
        for (index, entry) in entries.into_iter().enumerate() {
            let label = if index == 0 { catalog.label } else { "" };
            let mut row = fit(&[(label, Style::DIM)], labels);
            pad(&mut row, labels, Style::PLAIN);
            row.extend(entry);
            text::truncate(&mut row, room);
            lines.push(row);
        }
    }
    lines
}

/// What the welcome says of an MCP server: what it said it is for, else the
/// tools it had, else, when it has never been used, what serves it.
fn server_description(server: &mcp::Listing) -> Cow<'_, str> {
    match (&server.about, server.tools.as_slice()) {
        (Some(about), _) => Cow::from(about.as_str()),
        (None, []) => Cow::from(server.target.as_str()),
        (None, [tool]) => Cow::from(format!("1 tool: {tool}")),
        (None, tools) => Cow::from(format!("{} tools: {}", tools.len(), tools.join(", "))),
    }
}

/// The spinner's character `elapsed` into the session.
fn spinner(elapsed: Duration) -> char {
    let step = elapsed.as_millis() / SPINNER_INTERVAL.as_millis();
    SPINNER[usize::try_from(step % 10).unwrap_or(0)]
}

/// A horizontal edge of a box, `width` columns wide.
fn edge(width: usize, corners: [char; 2], style: Style) -> Line {
    let mut row = vec![(style, corners[0])];
    row.extend(std::iter::repeat_n((style, '─'), width.saturating_sub(2)));
    row.push((style, corners[1]));
    row
}

/// A panel's edge, with labels set into its left and right ends.
fn titled(
    width: usize,
    corners: [char; 2],
    left: &[(&str, Style)],
    right: &[(&str, Style)],
) -> Line {
    let room = width.saturating_sub(8);
    let right = fit(right, room / 3);
    let left = fit(left, room.saturating_sub(text::width(&right)));
    let label = |label: Line| -> Line {
        if label.is_empty() {
            return label;
        }
        std::iter::once((Style::PLAIN, ' ')).chain(label).chain([(Style::PLAIN, ' ')]).collect()
    };
    let (left, right) = (label(left), label(right));
    let fill = width.saturating_sub(4 + text::width(&left) + text::width(&right));
    let mut row = vec![(Style::DIM, corners[0]), (Style::DIM, '─')];
    row.extend(left);
    row.extend(std::iter::repeat_n((Style::DIM, '─'), fill));
    row.extend(right);
    row.extend([(Style::DIM, '─'), (Style::DIM, corners[1])]);
    text::truncate(&mut row, width);
    row
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::test_app;
    use super::*;
    use crate::agent::Update;

    /// The screen at `width` × `height`, as rows without styles.
    fn paint(app: &mut App, width: usize, height: usize) -> (Frame, Vec<String>) {
        let mut frame = Frame::new(width, height);
        app.paint(&mut frame, Instant::now());
        let rows = (0..height).map(|y| frame.row(y).trim_end().to_owned()).collect();
        (frame, rows)
    }

    #[test]
    fn the_dock_fits_small_terminals_and_keeps_its_cursor_visible() {
        let (mut app, _dir) = test_app((80, 24));
        app.editor.insert(&"界 long input\n".repeat(40));
        for columns in 1..80 {
            for rows in 1..24 {
                let (dock, cursor) = app.dock(columns, rows, '*', Instant::now());
                assert!(dock.len() <= rows, "{columns}×{rows}: {} rows", dock.len());
                assert!(
                    dock.iter().all(|row| text::width(row) <= columns),
                    "{columns}×{rows}: {:?}",
                    dock.iter().map(|row| text::plain(row)).collect::<Vec<_>>()
                );
                let cursor = cursor.expect("the cursor shows");
                assert!(
                    cursor.0 < dock.len() && cursor.1 < columns,
                    "{columns}×{rows}: {cursor:?}"
                );
            }
        }
    }

    #[test]
    fn every_terminal_size_draws_without_panicking_with_menus_open() {
        let (mut app, _dir) = test_app((80, 24));
        // A frame clips what is put on it, so reaching the end of a paint is the check.
        let draw_all = |app: &mut App| {
            for width in 0..100 {
                for height in 0..30 {
                    paint(app, width, height);
                }
            }
        };
        draw_all(&mut app);
        app.transcript.start(Path::new("/work"), Path::new("/agt/sessions/1"));
        app.transcript.user("draw 界 everywhere");
        app.transcript.update(Update::Thinking("**Plan** 界".into()));
        app.transcript.update(Update::Text("reply\n```\ncode\n```".into()));
        app.open_keys();
        draw_all(&mut app);
        app.menu = None;
        app.editor.insert("/");
        app.refresh_completion();
        draw_all(&mut app);
    }

    #[test]
    fn a_menu_sits_above_the_input_and_the_draft_stays() {
        let (mut app, _dir) = test_app((60, 20));
        app.editor.insert("half a thought");
        app.open_efforts();
        let (frame, rows) = paint(&mut app, 60, 20);
        let border =
            rows.iter().position(|row| row.starts_with("╭─ Reasoning effort")).expect("panel");
        let input =
            rows.iter().position(|row| row.starts_with("│ ❯ half a thought")).expect("input");
        assert!(border < input, "{rows:#?}");
        assert!(rows[border + 1].starts_with("│ ⌕ type to filter"), "{rows:#?}");
        assert!(
            rows.iter().any(|row| row.starts_with("╰─ ↑↓ choose · enter select · esc close")),
            "{rows:#?}"
        );
        assert_eq!(frame.cursor_at(), Some((4, border + 1)), "the search row has the cursor");
    }

    #[test]
    fn a_menu_keeps_its_place_while_its_list_is_filtered() {
        let (mut app, _dir) = test_app((60, 30));
        app.open_efforts();
        let border = |app: &mut App| {
            let (_, rows) = paint(app, 60, 30);
            rows.iter().position(|row| row.starts_with("╭─ Reasoning effort")).expect("panel")
        };
        let unfiltered = border(&mut app);
        app.menu.as_mut().expect("menu").picker.set_query("max");
        assert_eq!(border(&mut app), unfiltered);
    }

    #[test]
    fn a_completion_leaves_the_cursor_in_the_input() {
        let (mut app, _dir) = test_app((60, 20));
        app.editor.insert("/mo");
        app.refresh_completion();
        let (frame, rows) = paint(&mut app, 60, 20);
        let input = rows.iter().position(|row| row.starts_with("│ ❯ /mo")).expect("input");
        assert!(rows.iter().any(|row| row.starts_with("╭─ Commands")), "{rows:#?}");
        assert_eq!(frame.cursor_at(), Some((7, input)));
    }
}
