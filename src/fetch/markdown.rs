//! HTML as Markdown for an agent to read.
//!
//! Each block is written on lines of its own, without wrapping, so a line
//! number or a search finds it. Links are absolute. What a reader cannot follow
//! is left out: permalinks, links without text, images without descriptions,
//! and controls. Characters are escaped only where they would begin a block
//! the HTML does not have.

use dom_query::{Document, NodeRef};

use super::Url;

/// How deep the writer descends into nested elements. Deeper content is read
/// as plain text, so no page can exhaust the stack.
const MAX_DEPTH: usize = 256;

/// Elements whose content is read as blocks of their own.
const CONTAINERS: [&str; 30] = [
    "address",
    "article",
    "aside",
    "body",
    "caption",
    "center",
    "dd",
    "details",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "header",
    "hgroup",
    "html",
    "legend",
    "li",
    "main",
    "nav",
    "search",
    "section",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
];

/// Class names highlighters give the line numbers beside code.
const LINE_NUMBERS: [&str; 7] =
    ["linenos", "lineno", "line-numbers", "line-number", "linenumber", "gutter", "hljs-ln-numbers"];

/// Writes the content of `document` as Markdown, resolving links against `base`.
pub(super) fn write(document: &Document, base: &Url) -> String {
    let blocks = Writer { base }.blocks(&document.root(), Context::default());
    let mut lines = Vec::new();
    lay_out(&blocks, &mut lines, false);
    lines.join("\n")
}

/// `text` with its whitespace collapsed to single spaces, as HTML shows it.
pub(super) fn words(text: &str) -> String {
    text.split(is_space).filter(|word| !word.is_empty()).collect::<Vec<_>>().join(" ")
}

/// A block of content, as collected before it is laid out in lines.
#[derive(Debug, PartialEq)]
enum Block {
    Paragraph(String),
    Heading(usize, String),
    Code {
        language: String,
        text: String,
    },
    /// A list's items, numbered from `start` when it is ordered.
    List {
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Quote(Vec<Block>),
    /// Rows of cells, the first of which is the header.
    Table(Vec<Vec<String>>),
    Rule,
}

impl Block {
    /// The block's text on one line, with `line_break` between its lines, for
    /// what holds no blocks, such as table cells and links.
    fn flat(&self, line_break: &str) -> String {
        let join = |blocks: &[Block]| {
            let texts: Vec<String> = blocks.iter().map(|block| block.flat(line_break)).collect();
            texts.into_iter().filter(|text| !text.is_empty()).collect::<Vec<_>>().join(line_break)
        };
        match self {
            Self::Paragraph(text) | Self::Heading(_, text) => text.replace('\n', line_break),
            Self::Code { text, .. } => code_span(&words(text)),
            Self::List { items, .. } => {
                items.iter().map(|item| join(item)).collect::<Vec<_>>().join(line_break)
            }
            Self::Quote(blocks) => join(blocks),
            Self::Table(rows) => {
                rows.iter().map(|row| row.join(" ")).collect::<Vec<_>>().join(line_break)
            }
            Self::Rule => String::new(),
        }
    }
}

/// Where in the document the writer is.
#[derive(Clone, Copy, Default)]
struct Context {
    depth: usize,
    /// Inside a link, where an image is read as its description.
    link: bool,
}

/// The blocks collected so far, and the text of the paragraph being read.
#[derive(Default)]
struct Out {
    blocks: Vec<Block>,
    inline: Inline,
}

impl Out {
    /// Ends the paragraph being read. Consecutive line breaks, which pages use
    /// for space between paragraphs, end one.
    fn flush(&mut self) {
        let text = std::mem::take(&mut self.inline).finish();
        for paragraph in text.split("\n\n").filter(|paragraph| !paragraph.is_empty()) {
            self.blocks.push(Block::Paragraph(paragraph.to_owned()));
        }
    }

    /// What was collected as text on one line, for an element that holds no
    /// blocks.
    fn into_inline(mut self) -> Inline {
        if self.blocks.is_empty() {
            return self.inline;
        }
        self.flush();
        let texts = self.blocks.iter().map(|block| block.flat(" "));
        let text = texts.filter(|text| !text.is_empty()).collect::<Vec<_>>().join(" ");
        Inline { text, leading: true, space: true }
    }
}

/// The text of a paragraph as it is read, with whitespace collapsed as HTML
/// collapses it.
#[derive(Default)]
struct Inline {
    text: String,
    /// Whether whitespace came before the first word.
    leading: bool,
    /// Whether whitespace follows the last word.
    space: bool,
}

impl Inline {
    /// Adds text from the page.
    fn text(&mut self, text: &str) {
        for (index, word) in text.split(is_space).enumerate() {
            if index > 0 {
                self.space = true;
            }
            if !word.is_empty() {
                self.word(&word.replace('\u{a0}', " "));
            }
        }
    }

    /// Adds Markdown that is not split at its spaces, such as a link.
    fn word(&mut self, word: &str) {
        if self.text.is_empty() {
            self.leading |= self.space;
        } else if self.space && !self.text.ends_with('\n') {
            self.text.push(' ');
        }
        self.space = false;
        self.text.push_str(word);
    }

    fn line_break(&mut self) {
        self.text.push('\n');
        self.space = false;
    }

    /// Adds `inner`, the text of an element in this one, as `render` writes it,
    /// with the whitespace that came around it.
    fn wrapped(&mut self, inner: Inline, render: impl FnOnce(&str) -> String) {
        let trailing = inner.space;
        if inner.leading {
            self.space = true;
        }
        let text = inner.finish();
        if !text.is_empty() {
            self.word(&render(&text));
        }
        if trailing {
            self.space = true;
        }
    }

    /// The text's lines without the spaces around them, with each run of blank
    /// lines made one and none at the ends.
    fn finish(self) -> String {
        let mut lines: Vec<&str> = Vec::new();
        for line in self.text.lines().map(str::trim) {
            if !line.is_empty() || lines.last().is_some_and(|last| !last.is_empty()) {
                lines.push(line);
            }
        }
        while lines.last().is_some_and(|last| last.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }
}

struct Writer<'a> {
    base: &'a Url,
}

impl Writer<'_> {
    /// The blocks the children of `node` hold.
    fn blocks(&self, node: &NodeRef, context: Context) -> Vec<Block> {
        let mut out = Out::default();
        self.children(node, context, &mut out);
        out.flush();
        out.blocks
    }

    /// The text the children of `node` hold, on one line.
    fn inline(&self, node: &NodeRef, context: Context) -> Inline {
        let mut out = Out::default();
        self.children(node, context, &mut out);
        out.into_inline()
    }

    fn children(&self, node: &NodeRef, context: Context, out: &mut Out) {
        let context = Context { depth: context.depth + 1, ..context };
        for child in node.children_it(false) {
            self.node(&child, context, out);
        }
    }

    fn node(&self, node: &NodeRef, context: Context, out: &mut Out) {
        if node.is_text() {
            out.inline.text(&node.text());
            return;
        }
        let Some(name) = node.node_name() else {
            return;
        };
        if context.depth > MAX_DEPTH {
            out.inline.text(&node.text());
            return;
        }
        match &*name {
            "head" | "title" | "meta" | "link" | "script" | "style" | "noscript" | "template"
            | "svg" | "canvas" | "iframe" | "object" | "embed" | "audio" | "video" | "button"
            | "select" | "textarea" | "dialog" | "map" => {}
            "input" => {
                if node.attr("type").is_some_and(|kind| kind.eq_ignore_ascii_case("checkbox")) {
                    out.inline.word(if node.has_attr("checked") { "[x]" } else { "[ ]" });
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                out.flush();
                let level = usize::from(name.as_bytes()[1] - b'0');
                let text = one_line(&self.inline(node, context).finish());
                if !text.is_empty() {
                    out.blocks.push(Block::Heading(level, text));
                }
            }
            "p" => {
                out.flush();
                self.children(node, context, out);
                out.flush();
            }
            "br" => out.inline.line_break(),
            "hr" => {
                out.flush();
                out.blocks.push(Block::Rule);
            }
            "pre" => {
                out.flush();
                let mut text = String::new();
                code_text(node, &mut text, context.depth);
                let text = text.trim_end().trim_start_matches('\n');
                if !text.trim().is_empty() {
                    let language = language(node);
                    out.blocks.push(Block::Code { language, text: text.to_owned() });
                }
            }
            "ul" | "ol" | "menu" => self.list(node, &*name == "ol", context, out),
            "blockquote" => {
                out.flush();
                let quoted = self.blocks(node, context);
                if !quoted.is_empty() {
                    out.blocks.push(Block::Quote(quoted));
                }
            }
            "table" => self.table(node, context, out),
            // A term or a summary is a line of its own, in bold.
            "dt" | "summary" => {
                out.flush();
                let inner = self.inline(node, context);
                out.inline.wrapped(inner, |text| format!("**{text}**"));
                out.flush();
            }
            "a" => self.link(node, context, out),
            "img" => self.image(node, context, out),
            "strong" | "b" => self.emphasis(node, "**", context, out),
            "em" | "i" => self.emphasis(node, "*", context, out),
            "del" | "s" | "strike" => self.emphasis(node, "~~", context, out),
            "q" => self.emphasis(node, "\"", context, out),
            "code" | "kbd" | "samp" | "tt" => {
                let text = node.text();
                let inner = Inline {
                    text: words(&text),
                    leading: text.starts_with(is_space),
                    space: text.ends_with(is_space),
                };
                out.inline.wrapped(inner, code_span);
            }
            "math" => math(node, out),
            name if CONTAINERS.contains(&name) => {
                out.flush();
                self.children(node, context, out);
                out.flush();
            }
            _ => self.children(node, context, out),
        }
    }

    fn emphasis(&self, node: &NodeRef, marker: &str, context: Context, out: &mut Out) {
        let inner = self.inline(node, context);
        out.inline.wrapped(inner, |text| format!("{marker}{text}{marker}"));
    }

    /// A link to another page, with its text. A link within the page, such as
    /// a permalink, reads as its text, and is left out when that has no words.
    fn link(&self, node: &NodeRef, context: Context, out: &mut Out) {
        let inner = self.inline(node, Context { link: true, ..context });
        let href = node.attr("href");
        let target = href.and_then(|href| self.base.join(&href)).filter(|url| url != self.base);
        match target {
            Some(target) => out.inline.wrapped(inner, |text| {
                let address = target.to_string();
                let text = one_line(text);
                match text == address {
                    true => address,
                    false => format!("[{}]({})", escape_label(&text), destination(&address)),
                }
            }),
            None if inner.text.chars().any(char::is_alphanumeric) => {
                out.inline.wrapped(inner, str::to_owned);
            }
            None => out.inline.space |= inner.leading || inner.space,
        }
    }

    /// An image with a description; in a link, the description alone.
    fn image(&self, node: &NodeRef, context: Context, out: &mut Out) {
        let alt = node.attr("alt").map(|alt| words(&alt)).unwrap_or_default();
        if alt.is_empty() {
            return;
        }
        let source = ["src", "data-src"]
            .into_iter()
            .filter_map(|name| node.attr(name))
            .find(|source| !source.trim().is_empty() && !source.trim().starts_with("data:"))
            .and_then(|source| self.base.join(&source));
        match source {
            Some(source) if !context.link => {
                let destination = destination(&source.to_string());
                out.inline.word(&format!("![{}]({destination})", escape_label(&alt)));
            }
            _ => out.inline.text(&alt),
        }
    }

    /// A list, without the markers some pages write before each item's text.
    fn list(&self, node: &NodeRef, ordered: bool, context: Context, out: &mut Out) {
        out.flush();
        let start = ordered
            .then(|| node.attr("start").and_then(|start| start.trim().parse().ok()).unwrap_or(1));
        let mut items = Vec::new();
        for item in node.children_it(false).filter(NodeRef::is_element) {
            let mut blocks = match item.node_name().as_deref() {
                Some("li") => self.blocks(&item, context),
                // A list nested directly in a list is an item of its own.
                _ => {
                    let mut out = Out::default();
                    self.node(&item, context, &mut out);
                    out.flush();
                    out.blocks
                }
            };
            if matches!(blocks.first(), Some(Block::Paragraph(text)) if is_marker(text)) {
                blocks.remove(0);
            }
            if !blocks.is_empty() {
                items.push(blocks);
            }
        }
        if !items.is_empty() {
            out.blocks.push(Block::List { start, items });
        }
    }

    /// A table of data, or the content of a table used for layout: one marked
    /// as a presentation, with a single column, holding code, headings or
    /// tables of its own, or without headers and with empty cells.
    fn table(&self, node: &NodeRef, context: Context, out: &mut Out) {
        out.flush();
        let rows = rows(node);
        let (mut nested, mut headed) = (false, false);
        for descendant in node.descendants_it() {
            match descendant.node_name().as_deref() {
                Some("table" | "pre" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6") => nested = true,
                Some("th" | "thead") => headed = true,
                _ => {}
            }
        }
        let role = node.attr("role").map(|role| role.to_ascii_lowercase());
        let layout = matches!(role.as_deref(), Some("presentation" | "none"))
            || nested
            || rows.iter().all(|row| cells(row).len() <= 1)
            || (!headed
                && rows
                    .iter()
                    .any(|row| cells(row).iter().any(|cell| words(&cell.text()).is_empty())));
        if layout {
            self.children(node, context, out);
            out.flush();
            return;
        }
        for caption in node.children_it(false) {
            if caption.node_name().as_deref() == Some("caption") {
                self.node(&caption, context, out);
            }
        }
        let mut table = Vec::new();
        for row in &rows {
            let mut texts = Vec::new();
            for cell in cells(row) {
                let blocks = self.blocks(&cell, context);
                let flat = blocks.iter().map(|block| block.flat("<br>"));
                texts.push(flat.filter(|text| !text.is_empty()).collect::<Vec<_>>().join("<br>"));
                let span = cell.attr("colspan").and_then(|span| span.trim().parse().ok());
                texts.extend((1..span.unwrap_or(1).clamp(1, 64)).map(|_| String::new()));
            }
            if texts.iter().any(|text| !text.is_empty()) {
                table.push(texts);
            }
        }
        if !table.is_empty() {
            out.blocks.push(Block::Table(table));
        }
    }
}

/// The rows of `table`, not counting those of tables inside it.
fn rows<'a>(table: &NodeRef<'a>) -> Vec<NodeRef<'a>> {
    let mut rows = Vec::new();
    for child in table.children_it(false) {
        match child.node_name().as_deref() {
            Some("tr") => rows.push(child),
            Some("thead" | "tbody" | "tfoot") => rows.extend(
                child.children_it(false).filter(|row| row.node_name().as_deref() == Some("tr")),
            ),
            _ => {}
        }
    }
    rows
}

fn cells<'a>(row: &NodeRef<'a>) -> Vec<NodeRef<'a>> {
    let is_cell = |cell: &NodeRef| matches!(cell.node_name().as_deref(), Some("td" | "th"));
    row.children_it(false).filter(is_cell).collect()
}

/// Whether `text` is only the marker of a list item, such as `•` or `2.`.
fn is_marker(text: &str) -> bool {
    let numbered = text.strip_suffix(['.', ')']).is_some_and(|number| {
        (1..=3).contains(&number.len()) && number.chars().all(|c| c.is_ascii_digit())
            || number.len() == 1 && number.chars().all(|c| c.is_ascii_alphabetic())
    });
    numbered || text.chars().count() <= 3 && !text.chars().any(char::is_alphanumeric)
}

/// Appends the text of the code block `node` holds: its line breaks kept, and
/// line numbers left out.
fn code_text(node: &NodeRef, out: &mut String, depth: usize) {
    for child in node.children_it(false) {
        if child.is_text() {
            out.push_str(&child.text());
            continue;
        }
        let Some(name) = child.node_name() else {
            continue;
        };
        let classes = child.attr("class");
        let class = |names: &[&str]| {
            classes.as_ref().is_some_and(|class| {
                class.split_ascii_whitespace().any(|name| names.contains(&name))
            })
        };
        match &*name {
            _ if class(&LINE_NUMBERS) => {}
            _ if depth > MAX_DEPTH => out.push_str(&child.text()),
            "br" => out.push('\n'),
            "script" | "style" | "button" => {}
            // Some highlighters put each line in an element of its own, with
            // or without a line break between them.
            _ if matches!(&*name, "div" | "p" | "tr" | "li") || class(&["line"]) => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                code_text(&child, out, depth + 1);
            }
            _ => code_text(&child, out, depth + 1),
        }
    }
}

/// The language a code block names on its `code`, itself or the two elements
/// around it: as `language-rust`, `lang-rust`, `highlight-rust` or
/// `highlight-source-rust` classes, or a `data-lang` attribute.
fn language(pre: &NodeRef) -> String {
    let code = pre.children_it(false).find(|child| child.node_name().as_deref() == Some("code"));
    let parent = pre.parent();
    let grandparent = parent.as_ref().and_then(NodeRef::parent);
    for node in [code, Some(*pre), parent, grandparent].into_iter().flatten() {
        let named = ["data-lang", "data-language"].into_iter().filter_map(|name| node.attr(name));
        let classes = node.attr("class").map(|class| {
            let prefixes = ["language-", "lang-", "highlight-source-", "highlight-"];
            class
                .split_ascii_whitespace()
                .filter_map(|name| prefixes.iter().find_map(|prefix| name.strip_prefix(prefix)))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
        let names = named.map(|name| name.to_string()).chain(classes.into_iter().flatten());
        for name in names {
            let language: String = name
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '#' | '-' | '_' | '.'))
                .collect::<String>()
                .to_ascii_lowercase();
            if !matches!(
                language.as_str(),
                "" | "default" | "none" | "nohighlight" | "plain" | "plaintext"
            ) {
                return language;
            }
        }
    }
    String::new()
}

/// MathML as TeX, from its `alttext` or its TeX annotation.
fn math(node: &NodeRef, out: &mut Out) {
    let annotation = || {
        node.descendants_it()
            .find(|descendant| {
                descendant.node_name().as_deref() == Some("annotation")
                    && descendant.attr("encoding").is_some_and(|encoding| encoding.contains("tex"))
            })
            .map(|annotation| annotation.text().trim().to_owned())
    };
    let alt = node.attr("alttext").map(|alt| alt.trim().to_owned());
    let tex = alt
        .filter(|tex| !tex.is_empty())
        .or_else(annotation)
        .unwrap_or_else(|| words(&node.text()));
    if tex.is_empty() {
        return;
    }
    if node.attr("display").is_some_and(|display| display.eq_ignore_ascii_case("block")) {
        out.flush();
        out.blocks.push(Block::Paragraph(format!("$$\n{tex}\n$$")));
    } else {
        out.inline.word(&format!("${tex}$"));
    }
}

/// Appends the lines of `blocks`, with a blank line between blocks, except
/// before a list that continues the text of the list item it is in.
fn lay_out(blocks: &[Block], lines: &mut Vec<String>, in_item: bool) {
    for (index, block) in blocks.iter().enumerate() {
        let continues = in_item
            && matches!(block, Block::List { .. })
            && matches!(blocks.get(index.wrapping_sub(1)), Some(Block::Paragraph(_)));
        if index > 0 && !continues {
            lines.push(String::new());
        }
        match block {
            Block::Paragraph(text) => lines.extend(text.lines().map(escape_start)),
            Block::Heading(level, text) => lines.push(format!("{} {text}", "#".repeat(*level))),
            Block::Code { language, text } => {
                let fence = "`".repeat(longest_run(text).max(2) + 1);
                lines.push(format!("{fence}{language}"));
                lines.extend(text.lines().map(|line| line.trim_end().to_owned()));
                lines.push(fence);
            }
            Block::List { start, items } => {
                for (number, item) in (start.unwrap_or(1)..).zip(items) {
                    let marker = match start {
                        Some(_) => format!("{number}. "),
                        None => "- ".to_owned(),
                    };
                    let indent = " ".repeat(marker.len());
                    let mut item_lines = Vec::new();
                    lay_out(item, &mut item_lines, true);
                    for (at, line) in item_lines.into_iter().enumerate() {
                        lines.push(match at {
                            0 => format!("{marker}{line}"),
                            _ if line.is_empty() => line,
                            _ => format!("{indent}{line}"),
                        });
                    }
                }
            }
            Block::Quote(quoted) => {
                let mut quoted_lines = Vec::new();
                lay_out(quoted, &mut quoted_lines, false);
                lines.extend(quoted_lines.into_iter().map(|line| match line.is_empty() {
                    true => ">".to_owned(),
                    false => format!("> {line}"),
                }));
            }
            Block::Table(rows) => {
                let width = rows.iter().map(Vec::len).max().unwrap_or(0);
                for (at, row) in rows.iter().enumerate() {
                    let cells = (0..width).map(|column| {
                        row.get(column).map_or_else(String::new, |cell| cell.replace('|', "\\|"))
                    });
                    lines.push(format!("| {} |", cells.collect::<Vec<_>>().join(" | ")));
                    if at == 0 {
                        lines.push(format!("|{}|", vec![" --- "; width].join("|")));
                    }
                }
            }
            Block::Rule => lines.push("---".to_owned()),
        }
    }
}

/// A paragraph's `line`, escaped where it would begin a heading, quote, list
/// item, code fence or rule.
fn escape_start(line: &str) -> String {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    let after = &line[digits..];
    let numbered = matches!(after, "." | ")") || after.starts_with(". ") || after.starts_with(") ");
    if (1..=9).contains(&digits) && numbered {
        return format!("{}\\{after}", &line[..digits]);
    }
    let hashes = line.len() - line.trim_start_matches('#').len();
    let heading = (1..=6).contains(&hashes) && line[hashes..].starts_with(' ');
    let item = ["- ", "+ ", "* "].iter().any(|marker| line.starts_with(marker));
    let fence = line.starts_with("```") || line.starts_with("~~~");
    let symbols: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    let rule = symbols.len() >= 3
        && ['-', '*', '_', '='].iter().any(|symbol| symbols.chars().all(|c| c == *symbol));
    match heading || item || fence || rule || line.starts_with('>') {
        true => format!("\\{line}"),
        false => line.to_owned(),
    }
}

/// `text` as inline code, fenced by more backticks than it holds in a row.
fn code_span(text: &str) -> String {
    let ticks = "`".repeat(longest_run(text) + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') { " " } else { "" };
    format!("{ticks}{pad}{text}{pad}{ticks}")
}

/// The longest run of backticks in `text`.
fn longest_run(text: &str) -> usize {
    text.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

/// A link's text, with the brackets that would end it escaped.
fn escape_label(text: &str) -> String {
    text.replace('[', "\\[").replace(']', "\\]")
}

/// A link's destination, in angle brackets when its parentheses do not pair.
fn destination(address: &str) -> String {
    let mut depth = 0_i32;
    for c in address.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return format!("<{address}>"),
            ')' => depth -= 1,
            _ => {}
        }
    }
    if depth == 0 { address.to_owned() } else { format!("<{address}>") }
}

/// `text` on one line, its lines joined by spaces.
fn one_line(text: &str) -> String {
    text.lines().filter(|line| !line.is_empty()).collect::<Vec<_>>().join(" ")
}

/// Whether `c` is whitespace as HTML collapses it.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn markdown(html: &str) -> String {
        let base = Url::parse("https://example.com/docs/guide").expect("base");
        write(&Document::fragment(html), &base)
    }

    #[test]
    fn text_blocks_are_written_one_to_a_line() {
        let html = r##"
            <h1>Guide <a class="anchor" href="#guide">§</a></h1>
            <p>Read   the <a href="/docs/start">getting
            started</a> page, or <a href="#install">install it</a>.</p>
            <p><strong>Bold </strong>and <em>emphasis</em>, <code>let x = `y`;</code> and
            <kbd>Ctrl</kbd>+<kbd>C</kbd>.<br>Second&nbsp;&nbsp;line.</p>
            <ul><li>One</li><li>Two<ul><li>Nested</li></ul></li></ul>
            <ol start="3"><li><p>Three</p><p>More</p></li><li>Four</li></ol>
            <blockquote><p>Quoted</p><p>twice</p></blockquote>
            <hr>
            <dl><dt>Term</dt><dd>Definition</dd></dl>
            <details><summary>More</summary><div>Hidden text</div></details>"##;
        let expected = "\
# Guide

Read the [getting started](https://example.com/docs/start) page, or install it.

**Bold** and *emphasis*, ``let x = `y`;`` and `Ctrl`+`C`.
Second  line.

- One
- Two
  - Nested

3. Three

   More
4. Four

> Quoted
>
> twice

---

**Term**

Definition

**More**

Hidden text";
        assert_eq!(markdown(html), expected);
    }

    #[test]
    fn code_and_tables_keep_their_structure() {
        let html = r#"
            <div class="highlight-python notranslate"><div class="highlight"><pre><span></span><span class="n">print</span>(<span class="s2">"hi"</span>)
</pre></div></div>
            <pre><code class="language-rust">fn main() {
    println!("```");
}</code></pre>
            <pre><code><span class="line-number">1</span><div>let a = 1;</div><div>let b = 2;</div></code></pre>
            <pre><code><span class="line">npm create vite</span><span class="line">cd app</span></code></pre>
            <pre><code><span class="line">a</span>
<span class="line">b</span></code></pre>
            <table><caption>Settings</caption><thead><tr><th>Name</th><th>Value</th></tr></thead>
            <tbody><tr><td><code>a|b</code></td><td><p>one</p><p>two</p></td></tr>
            <tr><td colspan="2">wide</td></tr></tbody></table>
            <table role="presentation"><tr><td><p>Layout</p></td><td><p>text</p></td></tr></table>
            <table><tr><td>1.</td><td></td><td><a href="/item">Story</a></td></tr>
            <tr><td></td><td></td><td>9 points</td></tr></table>"#;
        let expected = "\
```python
print(\"hi\")
```

````rust
fn main() {
    println!(\"```\");
}
````

```
let a = 1;
let b = 2;
```

```
npm create vite
cd app
```

```
a
b
```

Settings

| Name | Value |
| --- | --- |
| `a\\|b` | one<br>two |
| wide |  |

Layout

text

1\\.

[Story](https://example.com/item)

9 points";
        assert_eq!(markdown(html), expected);
    }

    #[test]
    fn only_what_a_reader_can_follow_is_linked() {
        let html = r#"
            <p><a href="https://crates.io/crates/ureq"><img src="https://img.shields.io/badge.svg" alt="Crates.io"></a>
            <img src="diagram.png" alt="The  flow"> <img src="spacer.gif"> <img src="data:image/png;base64,AAAA" alt="inline"></p>
            <p><a href="https://example.com/wiki/Rust_(language)">https://example.com/wiki/Rust_(language)</a>
            <a href="/a(b">odd</a> <a href="javascript:void(0)">Run</a> <a href="/icon"><svg></svg></a>
            <a href="/x">[1]</a></p>
            <p># not a heading</p><p>1. not a list</p><p>---</p><p>&gt; not a quote</p>
            <ul><li><input type="checkbox" checked> done</li><li><input type="checkbox"> todo</li></ul>
            <p>Inline <math alttext="h_{t-1}"><mi>h</mi></math> math.</p>
            <script>alert(1)</script><button>Copy</button>"#;
        let expected = "\
[Crates.io](https://crates.io/crates/ureq) ![The flow](https://example.com/docs/diagram.png) inline

https://example.com/wiki/Rust_(language) [odd](<https://example.com/a(b>) Run [\\[1\\]](https://example.com/x)

\\# not a heading

1\\. not a list

\\---

\\> not a quote

- [x] done
- [ ] todo

Inline $h_{t-1}$ math.";
        assert_eq!(markdown(html), expected);
    }

    #[test]
    fn pages_laid_out_with_breaks_and_markers_read_as_paragraphs_and_items() {
        let html = r#"
            <p><font>July 2023<br><br>First paragraph,<br>on two lines.<br><br><br>Last.</font></p>
            <ul><li><span class="ltx_tag">•</span><div class="ltx_para"><p>Item text</p></div></li>
            <li><span class="ltx_tag">2.</span><div>Numbered</div></li><li>OK.</li></ul>"#;
        let expected = "\
July 2023

First paragraph,
on two lines.

Last.

- Item text
- Numbered
- OK.";
        assert_eq!(markdown(html), expected);
    }

    #[test]
    fn deeply_nested_pages_are_read_without_exhausting_the_stack() {
        let html = format!("{}deep text{}", "<span>".repeat(5000), "</span>".repeat(5000));
        assert_eq!(markdown(&html), "deep text");
    }
}
