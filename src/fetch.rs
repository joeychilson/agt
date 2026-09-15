//! Web pages as Markdown for agents, which `agt fetch` reads.
//!
//! A page is read from the best of its sources. Kinds of pages agt knows, such
//! as a GitHub file or an npm package, are read from where their content is
//! published. The root page of a site is read from the site's llms.txt when it
//! has one. Requests ask for Markdown first, which many documentation sites
//! answer with. HTML that names a Markdown version of itself, with
//! `<link rel="alternate">` or a link to the page's own llms.txt, is read from
//! that version, and other HTML becomes Markdown of its main content. A source
//! that fails gives way to the page itself.

mod html;
mod http;
mod markdown;
mod sites;
mod url;

use std::fmt;

use dom_query::Document;

use crate::bash::bytes;
use http::{Client, Response};
pub(crate) use url::Url;

/// A page, read as text.
#[derive(Debug, PartialEq)]
pub(crate) struct Page {
    /// Which of the page's sources the text came from.
    pub(crate) source: Source,
    /// The URL that answered, after redirects.
    pub(crate) url: Url,
    /// What that URL sent.
    pub(crate) form: Form,
    /// The text, with Unix line endings, a final newline, and no control
    /// characters but tabs.
    pub(crate) text: String,
}

/// Where a page's text came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// The page itself.
    Page,
    /// The Markdown version the page's HTML names.
    Alternate,
    /// The llms.txt of the site whose root page was asked for.
    LlmsTxt,
    /// The source agt knows for this kind of page, as the page's header names
    /// it, such as `the raw file`.
    Site(&'static str),
}

/// What a URL sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Form {
    Markdown,
    /// Text that is neither Markdown nor HTML, such as source code.
    Text,
    /// JSON, which is read indented.
    Json,
    /// HTML, which is read as Markdown of its main content.
    Html,
}

impl Form {
    /// The form of a response of media type `media` holding `body`, or `None`
    /// when it is not text.
    fn of(media: &str, body: &[u8]) -> Option<Self> {
        match media {
            "text/markdown" | "text/x-markdown" => Some(Self::Markdown),
            "text/html" | "application/xhtml+xml" => Some(Self::Html),
            "application/json" | "text/json" => Some(Self::Json),
            _ if media.ends_with("+json") => Some(Self::Json),
            "application/xml"
            | "application/javascript"
            | "application/x-javascript"
            | "application/ecmascript"
            | "application/toml"
            | "application/yaml"
            | "application/x-yaml"
            | "application/x-sh" => Some(Self::Text),
            _ if media.starts_with("text/") || media.ends_with("+xml") => Some(Self::Text),
            "" | "application/octet-stream" | "binary/octet-stream" => {
                let text = std::str::from_utf8(body).ok().filter(|text| !text.contains('\0'))?;
                let start = text.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
                let start = start.get(..14).unwrap_or(start).to_ascii_lowercase();
                let html = start.starts_with("<!doctype html") || start.starts_with("<html");
                Some(if html { Self::Html } else { Self::Text })
            }
            _ => None,
        }
    }
}

/// Why a page could not be read.
#[derive(Debug, PartialEq)]
pub(crate) enum Error {
    /// No response arrived, or its body could not be read.
    Unreachable { url: Url, reason: String },
    /// The response was not a success.
    Status { url: Url, status: u16, reason: String },
    /// The response was larger than agt reads.
    TooLarge(Url),
    /// The response was a file that is not text.
    NotText { url: Url, media: String, size: usize },
    /// The page held no text.
    Empty(Url),
    /// A site's source answered with something other than what agt reads from
    /// it.
    Unexpected(Url),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable { url, reason } => write!(f, "cannot read {url}: {reason}"),
            Self::Status { url, status, reason } => {
                write!(f, "{url} answered {}", format!("{status} {reason}").trim_end())
            }
            Self::TooLarge(url) => {
                write!(f, "{url} is larger than the {} agt reads", bytes(http::MAX_BODY))
            }
            Self::NotText { url, media, size } => {
                let media = if media.is_empty() { "a file" } else { media };
                let size = bytes(*size as u64);
                write!(
                    f,
                    "{url} is {media} of {size}, not a page agt reads; download it with curl -o <file> '{url}'"
                )
            }
            Self::Empty(url) => write!(
                f,
                "{url} has no text to read; a page that shows its content with JavaScript cannot be read"
            ),
            Self::Unexpected(url) => write!(f, "{url} answered with something agt does not read"),
        }
    }
}

/// Reads the page at `url` from its best source.
pub(crate) fn fetch(url: &Url) -> Result<Page, Error> {
    let client = Client::new();
    if let Some(route) = sites::route(url) {
        match route.read(&client) {
            Ok(page) => return Ok(page),
            // A file that is not text is no page either, whatever the page
            // around it holds.
            Err(error @ Error::NotText { .. }) => return Err(error),
            Err(_) => {}
        }
    }
    if url.path() == "/"
        && let Some(llms) = url.join("/llms.txt")
        && let Some(page) = text_at(&client, &llms, Source::LlmsTxt)
    {
        return Ok(page);
    }
    read(&client, client.get(url, &[("accept", http::MARKDOWN_FIRST)])?)
}

/// The text at `url`, as `source`, when it is Markdown or text: a site without
/// the file asked for often answers with an HTML page instead.
fn text_at(client: &Client, url: &Url, source: Source) -> Option<Page> {
    let response = client.get(url, &[("accept", http::MARKDOWN_FIRST)]).ok()?;
    let form = Form::of(&response.media, &response.body)
        .filter(|form| matches!(form, Form::Markdown | Form::Text))?;
    let text = clean(&String::from_utf8_lossy(&response.body));
    (!text.is_empty()).then_some(Page { source, url: response.url, form, text })
}

/// The page `response` holds, or its Markdown version when its HTML names one.
fn read(client: &Client, response: Response) -> Result<Page, Error> {
    let Response { url, media, body } = response;
    let Some(mut form) = Form::of(&media, &body) else {
        return Err(Error::NotText { url, media, size: body.len() });
    };
    let sent = String::from_utf8_lossy(&body);
    let text = match form {
        Form::Markdown | Form::Text => clean(&sent),
        Form::Json => match indent_json(&sent) {
            Some(json) => clean(&json),
            None => {
                form = Form::Text;
                clean(&sent)
            }
        },
        Form::Html => {
            let document = Document::from(&*sent);
            if let Some(alternate) = html::alternate(&document, &url)
                && alternate != url
                && let Some(page) = text_at(client, &alternate, Source::Alternate)
            {
                return Ok(page);
            }
            clean(&html::markdown(document, &url))
        }
    };
    if text.is_empty() {
        return Err(Error::Empty(url));
    }
    Ok(Page { source: Source::Page, url, form, text })
}

/// `count` of `noun`, such as `1 line` or `3 lines`.
pub(crate) fn plural(count: i64, noun: &str) -> String {
    match count {
        1 => format!("1 {noun}"),
        count => format!("{count} {noun}s"),
    }
}

/// `text` as pages are read: Unix line endings, no byte order mark, control
/// characters but tabs, or blank lines at either end, and a final newline.
fn clean(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut out = String::with_capacity(text.len() + 1);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => out.push('\n'),
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    let start = out.len() - out.trim_start_matches('\n').len();
    let end = out.trim_end().len();
    let mut out = out[start.min(end)..end].to_owned();
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// JSON `text` indented two spaces a level, its members in their order, or
/// `None` when it is not JSON.
fn indent_json(text: &str) -> Option<String> {
    serde_json::from_str::<serde::de::IgnoredAny>(text).ok()?;
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
    };
    let mut out = String::with_capacity(text.len() * 2);
    let (mut depth, mut in_string, mut escaped) = (0, false, false);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                while chars.next_if(char::is_ascii_whitespace).is_some() {}
                match chars.next_if(|next| matches!(next, '}' | ']')) {
                    Some(close) => out.push(close),
                    None => {
                        depth += 1;
                        newline(&mut out, depth);
                    }
                }
            }
            '}' | ']' => {
                depth -= 1;
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_ascii_whitespace() => {}
            c => out.push(c),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_are_read_by_their_media_type_or_else_their_bytes() {
        for (media, body, form) in [
            ("text/markdown", &b"# Guide"[..], Some(Form::Markdown)),
            ("text/html", b"<main>Guide</main>", Some(Form::Html)),
            ("application/xhtml+xml", b"<html/>", Some(Form::Html)),
            ("application/problem+json", b"{}", Some(Form::Json)),
            ("text/plain", b"llms", Some(Form::Text)),
            ("application/rss+xml", b"<rss/>", Some(Form::Text)),
            ("application/toml", b"a = 1", Some(Form::Text)),
            ("", b"\n <!DOCTYPE html><title>t</title>", Some(Form::Html)),
            ("application/octet-stream", b"fn main() {}", Some(Form::Text)),
            ("application/octet-stream", b"\x89PNG\r\n\x1a\n\0", None),
            ("", b"a\0b", None),
            ("application/pdf", b"%PDF-1.7", None),
            ("image/png", b"\x89PNG", None),
        ] {
            assert_eq!(Form::of(media, body), form, "{media} {body:?}");
        }
    }

    #[test]
    fn text_is_cleaned_of_what_a_terminal_would_act_on() {
        assert_eq!(
            clean("\u{feff}\n\n# Title\r\n\rbody\x1b[31m\0\ttab\u{85}  \n\n"),
            "# Title\n\nbody[31m\ttab\n"
        );
        assert_eq!(clean(" \n\t\n"), "");
        assert_eq!(clean("  indented\n"), "  indented\n", "leading spaces are content");
    }

    #[test]
    fn json_is_indented_with_its_members_in_order() {
        let text = r#" {"z":1,"a":[true,null,{}],"s":"a \"quoted\", {bracket}: [x]","e":[ ]} "#;
        let expected = "{\n  \"z\": 1,\n  \"a\": [\n    true,\n    null,\n    {}\n  ],\n  \"s\": \"a \\\"quoted\\\", {bracket}: [x]\",\n  \"e\": []\n}";
        assert_eq!(indent_json(text).as_deref(), Some(expected));
        assert_eq!(indent_json("temporarily unavailable"), None);
        assert_eq!(indent_json("{\"a\": 1} trailing"), None);
    }
}
