//! `agt fetch`: web pages as Markdown, for a session's agent and for people.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lexopt::{Arg, Parser, ValueExt};
use pulldown_cmark::{Event, Parser as Markdown, Tag, TagEnd};
use ring::digest::{SHA256, digest};

use super::{Error, failed, show_help};
use crate::bash::{SESSION_DIR, bytes};
use crate::fetch::{self, Form, Page, Source, Url, plural};

/// The most of a page shown whole in a session; with its header it stays
/// within the 12 KB a command's result shows.
const WHOLE: usize = 10 * 1024;
/// The most of a long page's outline shown.
const OUTLINE: usize = 5 * 1024;
/// The most of a long page's first lines shown after its outline.
const OPENING: usize = 3 * 1024;

pub(super) const HELP: &str = "\
Usage: agt fetch <url>

Reads a web page as Markdown. agt asks the site for Markdown, reads the llms.txt
of a site's root page and the Markdown version a page names, and turns other
HTML into Markdown of the page's main content, without its navigation. A URL
without a scheme is read over https. Pages that need JavaScript to show their
content, PDFs, images and other files are not read.

In an agt session the page is saved in $AGT_SESSION_DIR/fetch/ and printed after
a header that says where it came from and where it is saved. A page too long for
one command's result shows its headings with their line numbers and its first
lines instead. Piped, or outside a session, the page alone is printed.";

/// Runs `agt fetch` with the arguments after `fetch`.
pub(super) fn run(args: Parser) -> Result<ExitCode, Error> {
    let Some(url) = parse(args)? else {
        return show_help(HELP);
    };
    let page = fetch::fetch(&url).map_err(failed)?;
    let shown = match std::env::var_os(SESSION_DIR) {
        Some(dir) if io::stdout().is_terminal() => {
            let path = save(Path::new(&dir), &url, &page.text)
                .map_err(|error| failed(format!("cannot save {url}: {error}")))?;
            present(&url, &page, &path)
        }
        _ => page.text,
    };
    let mut stdout = io::stdout().lock();
    match stdout.write_all(shown.as_bytes()).and_then(|()| stdout.flush()) {
        Err(error) if error.kind() != io::ErrorKind::BrokenPipe => Err(failed(error)),
        _ => Ok(ExitCode::SUCCESS),
    }
}

/// Reads the URL after `fetch`; `None` asks for help.
fn parse(mut args: Parser) -> Result<Option<Url>, Error> {
    let mut url = None;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('h') | Arg::Long("help") => return Ok(None),
            Arg::Value(value) if url.is_none() => url = Some(value.string()?),
            Arg::Value(_) => return Err(Error::Usage("fetch reads one URL at a time".into())),
            arg => return Err(arg.unexpected().into()),
        }
    }
    let url = url.ok_or_else(|| Error::Usage("name the URL of the page to read".into()))?;
    // Addresses are often written without a scheme, such as docs.rs/ureq.
    let url = if url.contains("://") { url } else { format!("https://{url}") };
    Url::parse(&url).map(Some).map_err(Error::Usage)
}

/// Saves `text` in the session's `fetch` folder under a name made from `url`.
fn save(session: &Path, url: &Url, text: &str) -> io::Result<PathBuf> {
    let dir = session.join("fetch");
    fs::create_dir_all(&dir)?;
    let path = dir.join(file_name(url));
    fs::write(&path, text)?;
    Ok(path)
}

/// A file name that shows `url` and differs for every URL, such as
/// `docs.rs-ureq-latest-3f2a9c1b.md`.
fn file_name(url: &Url) -> String {
    let address = url.to_string();
    let readable = address.split_once("://").map_or(address.as_str(), |(_, rest)| rest);
    let mut name = String::new();
    for c in readable.chars() {
        if name.len() >= 80 {
            break;
        }
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_') {
            name.push(c);
        } else if !name.ends_with('-') {
            name.push('-');
        }
    }
    let hash = digest(&SHA256, address.as_bytes());
    let hash: String = hash.as_ref()[..4].iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}-{hash}.md", name.trim_matches(['-', '.']))
}

/// What a session's agent is shown of `page`, asked for as `requested` and
/// saved at `path`: a header, then the page, or its outline and first lines
/// when it is long.
fn present(requested: &Url, page: &Page, path: &Path) -> String {
    let text = &page.text;
    let path = path.display().to_string();
    let lines = text.lines().count();
    let mut out = format!(
        "[{requested} · {} · {} · {} · saved as {path}]\n",
        origin(requested, page),
        plural(lines as i64, "line"),
        bytes(text.len() as u64),
    );
    if text.len() <= WHOLE {
        out.push_str(text);
        return out;
    }
    let read =
        format!("Read it in ranges with sed -n '<from>,<to>p' {path}, or search it with rg -n.");
    let headings = headings(text);
    if headings.is_empty() {
        out.push_str(&format!(
            "It is too long to show whole, so these are its first lines. {read}\n"
        ));
    } else {
        out.push_str(&format!(
            "It is too long to show whole, so these are its headings with their line numbers, then its first lines. {read}\n\n"
        ));
        out.push_str(&outline(&headings, &path));
    }
    let opening = opening(text);
    match opening.ends_with('\n') {
        true => {
            out.push_str(&format!("\nLines 1-{} of {lines}:\n{opening}", opening.lines().count()))
        }
        false => out.push_str(&format!("\nThe start of line 1 of {lines}:\n{opening}\n")),
    }
    out
}

/// Where `page`, asked for as `requested`, came from, as its header says.
fn origin(requested: &Url, page: &Page) -> String {
    match page.source {
        Source::LlmsTxt => format!("the site's llms.txt {}", page.url),
        Source::Alternate => format!("its Markdown version {}", page.url),
        Source::Site(source) => format!("{source} {}", page.url),
        Source::Page => {
            let form = match page.form {
                Form::Markdown => "Markdown",
                Form::Text => "text",
                Form::Json => "JSON",
                Form::Html => "HTML read as Markdown",
            };
            match page.url == *requested {
                true => form.to_owned(),
                false => format!("{form} from {}", page.url),
            }
        }
    }
}

/// A heading of a Markdown page.
#[derive(Debug, PartialEq)]
struct Heading {
    /// The number of the line it starts on.
    line: usize,
    level: usize,
    text: String,
}

/// The headings of Markdown `text`.
fn headings(text: &str) -> Vec<Heading> {
    let mut headings = Vec::new();
    let (mut line, mut counted) = (1, 0);
    let mut open: Option<Heading> = None;
    for (event, range) in Markdown::new(text).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                line += text[counted..range.start].matches('\n').count();
                counted = range.start;
                open = Some(Heading { line, level: level as usize, text: String::new() });
            }
            Event::Text(part) | Event::Code(part) => {
                if let Some(heading) = &mut open {
                    heading.text.push_str(&part);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(heading) = &mut open {
                    heading.text.push(' ');
                }
            }
            Event::End(TagEnd::Heading(_)) => headings.extend(open.take()),
            _ => {}
        }
    }
    headings
}

/// The outline of a page saved at `path` with `headings`, each after its line
/// number. It goes down to the deepest level that fits within `OUTLINE` with
/// more than one heading. When none does, as on a page whose title is over
/// many sections, it shows the top levels that have more than one heading until
/// it is full, and says how many are left out.
fn outline(headings: &[Heading], path: &str) -> String {
    let width = headings.last().map_or(1, |heading| heading.line.to_string().len());
    let entry = |heading: &Heading| {
        let marks = "#".repeat(heading.level);
        format!("{:>width$}  {marks} {}\n", heading.line, heading.text.trim())
    };
    let within = |deepest: usize| headings.iter().filter(move |heading| heading.level <= deepest);
    let size = |deepest: usize| within(deepest).map(|heading| entry(heading).len()).sum::<usize>();
    let levels = headings.iter().map(|heading| heading.level);
    let (top, bottom) = (levels.clone().min().unwrap_or(1), levels.max().unwrap_or(1));
    let deepest = (top..=bottom)
        .rev()
        .find(|deepest| within(*deepest).count() > 1 && size(*deepest) <= OUTLINE)
        .or_else(|| (top..=bottom).find(|deepest| within(*deepest).count() > 1))
        .unwrap_or(bottom);
    let mut outline = String::new();
    let mut shown = 0;
    for heading in within(deepest) {
        let entry = entry(heading);
        if outline.len() + entry.len() > OUTLINE {
            break;
        }
        outline.push_str(&entry);
        shown += 1;
    }
    if shown < headings.len() {
        outline.push_str(&format!(
            "{shown} of {} headings are shown; rg -n '^#' {path} lists them all.\n",
            headings.len()
        ));
    }
    outline
}

/// The first lines of `text` within `OPENING`, or the start of its first line
/// when that is longer.
fn opening(text: &str) -> &str {
    let mut end = 0;
    for line in text.split_inclusive('\n') {
        if end + line.len() > OPENING {
            break;
        }
        end += line.len();
    }
    match end {
        0 => &text[..text.floor_char_boundary(OPENING)],
        end => &text[..end],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).expect("URL")
    }

    fn page(text: &str, source: Source, form: Form, answered: &str) -> Page {
        Page { source, url: url(answered), form, text: text.to_owned() }
    }

    #[test]
    fn a_url_is_read_from_the_arguments() {
        let parse = |args: &[&str]| {
            parse(Parser::from_args(args)).map(|url| url.map(|url| url.to_string()))
        };
        assert_eq!(parse(&["docs.rs/ureq"]), Ok(Some("https://docs.rs/ureq".into())));
        assert_eq!(
            parse(&["--", "http://example.com/-x"]),
            Ok(Some("http://example.com/-x".into()))
        );
        assert_eq!(parse(&["--help"]), Ok(None));
        for (args, error) in [
            (&[][..], "name the URL of the page to read"),
            (&["a.com", "b.com"][..], "fetch reads one URL at a time"),
            (
                &["ftp://example.com/f"][..],
                r#""ftp://example.com/f" is not an http:// or https:// URL"#,
            ),
            (&["--raw", "a.com"][..], "invalid option '--raw'"),
        ] {
            assert_eq!(parse(args), Err(Error::Usage(error.into())), "{args:?}");
        }
    }

    #[test]
    fn saved_pages_are_named_for_their_urls() {
        let docs = file_name(&url("https://docs.rs/ureq/latest/ureq/"));
        assert!(docs.starts_with("docs.rs-ureq-latest-ureq-") && docs.ends_with(".md"), "{docs}");
        assert_eq!(docs.len(), "docs.rs-ureq-latest-ureq-".len() + 8 + 3, "{docs}");
        let first = file_name(&url("https://example.com/search?q=1"));
        let second = file_name(&url("https://example.com/search?q=2"));
        assert!(
            first.starts_with("example.com-search-q-1-") && first != second,
            "{first} {second}"
        );
        let long = file_name(&url(&format!("https://example.com/{}", "a".repeat(300))));
        assert!(long.len() <= 80 + 12, "{long}");
    }

    #[test]
    fn a_short_page_is_shown_whole_after_a_header() {
        let requested = url("https://example.com/docs");
        let path = Path::new("/s/fetch/example.com-docs-00000000.md");
        let saved = "saved as /s/fetch/example.com-docs-00000000.md";
        for (page, expected) in [
            (
                page("# Docs\nok\n", Source::Page, Form::Markdown, "https://example.com/docs"),
                format!(
                    "[https://example.com/docs · Markdown · 2 lines · 10 B · {saved}]\n# Docs\nok\n"
                ),
            ),
            (
                page("# Docs\n", Source::Page, Form::Html, "https://www.example.com/docs/"),
                format!(
                    "[https://example.com/docs · HTML read as Markdown from https://www.example.com/docs/ · 1 line · 7 B · {saved}]\n# Docs\n"
                ),
            ),
            (
                page("# Docs\n", Source::Alternate, Form::Markdown, "https://example.com/docs.md"),
                format!(
                    "[https://example.com/docs · its Markdown version https://example.com/docs.md · 1 line · 7 B · {saved}]\n# Docs\n"
                ),
            ),
            (
                page("# Site\n", Source::LlmsTxt, Form::Text, "https://example.com/llms.txt"),
                format!(
                    "[https://example.com/docs · the site's llms.txt https://example.com/llms.txt · 1 line · 7 B · {saved}]\n# Site\n"
                ),
            ),
            (
                page(
                    "# Docs\n",
                    Source::Site("the raw file"),
                    Form::Text,
                    "https://raw.example.com/docs.md",
                ),
                format!(
                    "[https://example.com/docs · the raw file https://raw.example.com/docs.md · 1 line · 7 B · {saved}]\n# Docs\n"
                ),
            ),
        ] {
            assert_eq!(present(&requested, &page, path), expected);
        }
    }

    #[test]
    fn a_long_page_is_shown_as_its_outline_and_first_lines() {
        let mut text = String::from("# Guide\n\nIntro.\n\n");
        for part in 1..=300 {
            text.push_str(&format!(
                "## Part {part}\n\n{}\n\n```sh\n# a comment, not a heading\n```\n\n",
                "word ".repeat(20).trim_end()
            ));
        }
        let requested = url("https://example.com/guide");
        let shown = present(
            &requested,
            &page(&text, Source::Page, Form::Markdown, "https://example.com/guide"),
            Path::new("/s/g.md"),
        );
        assert!(shown.len() < 12 * 1024, "a result shows it whole: {} bytes", shown.len());
        let (outline, first_lines) =
            shown.split_once("\nLines 1-").expect("the first lines follow the outline");
        assert!(outline.starts_with(
            "[https://example.com/guide · Markdown · 2404 lines · 44.4 KB · saved as /s/g.md]\n\
             It is too long to show whole, so these are its headings with their line numbers, then its first lines. \
             Read it in ranges with sed -n '<from>,<to>p' /s/g.md, or search it with rg -n.\n\n   \
             1  # Guide\n   5  ## Part 1\n  13  ## Part 2\n"
        ), "{outline}");
        assert!(!outline.contains("a comment"), "code is not a heading: {outline}");
        assert!(
            outline.ends_with(" of 301 headings are shown; rg -n '^#' /s/g.md lists them all.\n"),
            "{outline}"
        );
        assert!(
            first_lines.contains(" of 2404:\n# Guide\n\nIntro.\n\n## Part 1\n"),
            "{first_lines}"
        );
        assert_eq!(opening(&"x".repeat(5000)).len(), OPENING, "a long first line is cut");
    }
}
