//! What agt reads of a page's HTML: the Markdown version the page names, and
//! its main content.

use dom_query::Document;
use dom_smoothie::{Config, Readability};

use super::Url;
use super::markdown::{self, words};

/// Elements that never hold a page's content, removed before looking for it.
const CHROME: &str = "script, style, noscript, template, svg, canvas, iframe, nav, aside, \
    dialog, [hidden], [aria-hidden='true'], [role='navigation'], [role='complementary'], \
    [role='dialog'], .toc, .sidebar, .navbox, .infobox, .mw-editsection";
/// Characters of text an element needs to be taken for a page's main content
/// when readability finds no article.
const MAIN_TEXT: usize = 20;
/// What separates a page's name from its site's in a `<title>`.
const SEPARATORS: [&str; 7] = [" | ", " - ", " — ", " – ", " · ", " :: ", " » "];

/// The Markdown version `page` names in its HTML: a `<link rel="alternate"
/// type="text/markdown">`, or else a link to the page's own llms.txt.
pub(super) fn alternate(document: &Document, page: &Url) -> Option<Url> {
    let base = base(document, page);
    let declared = document.select("link[rel][type][href]").nodes().iter().find_map(|link| {
        let rel = link.attr("rel")?;
        let alternate =
            rel.split_ascii_whitespace().any(|rel| rel.eq_ignore_ascii_case("alternate"));
        let kind = link.attr("type")?.trim().to_ascii_lowercase();
        let markdown = matches!(kind.as_str(), "text/markdown" | "text/x-markdown");
        if alternate && markdown { base.join(&link.attr("href")?) } else { None }
    });
    declared.or_else(|| {
        if page.path() == "/" {
            return None;
        }
        let own = page.join(&format!("{}/llms.txt", page.path().trim_end_matches('/')))?;
        let links = document.select("a[href]");
        let mut targets = links.nodes().iter().filter_map(|link| base.join(&link.attr("href")?));
        targets.any(|target| target == own).then_some(own)
    })
}

/// The main content of `document`, the HTML of `page`, as Markdown under the
/// page's title; empty when it holds no text.
pub(super) fn markdown(document: Document, page: &Url) -> String {
    let base = base(&document, page);
    let title = title(&document);
    document.select(CHROME).remove();
    let main = main_content(&document);
    // Links are resolved as Markdown is written, so readability needs no URL.
    // Classes are kept for the writer, which finds code's languages in them.
    let config = Config { keep_classes: true, ..Config::default() };
    let Ok(mut readability) = Readability::with_document(document, None, Some(config)) else {
        return String::new();
    };
    let article = if readability.is_probably_readable() { readability.parse().ok() } else { None };
    let (content, byline) = match article {
        Some(article) if !article.text_content.trim().is_empty() => {
            (article.content.to_string(), article.byline)
        }
        _ => match main {
            Some(main) => (main, None),
            None => return String::new(),
        },
    };
    let content = markdown::write(&Document::fragment(content), &base);
    titled(&title, byline.as_deref(), &content)
}

/// The URL links in `document` resolve against: its `<base href>`, or the page.
fn base(document: &Document, page: &Url) -> Url {
    let href = document.select("base[href]").nodes().first().and_then(|base| base.attr("href"));
    href.and_then(|href| page.join(&href)).unwrap_or_else(|| page.clone())
}

/// The page's title: its first `h1` when its `<title>` holds that, since it
/// names the page without the site, or else its `<title>`, without the site's
/// name when `og:site_name` gives it.
fn title(document: &Document) -> String {
    let title = words(&document.select("head title").text());
    let heading = document.select("h1").nodes().first().map(|heading| {
        // A permalink's symbol sits in some headings, such as `¶` or `§`.
        let symbol = |c: char| matches!(c, '¶' | '§' | '#' | '🔗' | '⚓' | '↗');
        let text = words(&heading.text());
        text.trim_matches(|c: char| symbol(c) || c.is_whitespace()).to_owned()
    });
    if let Some(heading) = heading
        && !heading.is_empty()
        && title.to_lowercase().contains(&heading.to_lowercase())
    {
        return heading;
    }
    let site = document.select("meta[property='og:site_name']").attr("content");
    let site = site.map(|site| words(&site)).unwrap_or_default();
    let named = SEPARATORS.iter().find_map(|separator| {
        let page = title.strip_suffix(&format!("{separator}{site}"))?.trim();
        (!site.is_empty() && !page.is_empty()).then_some(page)
    });
    named.map_or(title.clone(), str::to_owned)
}

/// The HTML of the page's largest `main`, `article` or `[role=main]` element,
/// or else of its body, for pages readability finds no article in, such as
/// documentation indexes and short pages.
fn main_content(document: &Document) -> Option<String> {
    let candidates = document.select("main, article, [role='main']");
    let largest = candidates
        .nodes()
        .iter()
        .map(|node| (node.text().chars().filter(|c| !c.is_whitespace()).count(), node))
        .filter(|(length, _)| *length >= MAIN_TEXT)
        .max_by_key(|(length, _)| *length)
        .map(|(_, node)| node.html().to_string());
    largest.or_else(|| document.body().map(|body| body.html().to_string()))
}

/// Markdown `content` under a heading of the page's `title` and its `byline`,
/// in place of a first heading that says the same. Content that starts with its
/// own top heading keeps it, and content with no text stays empty.
fn titled(title: &str, byline: Option<&str>, content: &str) -> String {
    if content.is_empty() || title.is_empty() || content.starts_with("# ") {
        return content.to_owned();
    }
    let (first, rest) = content.split_once('\n').unwrap_or((content, ""));
    let heading = first.trim_start_matches('#');
    let repeated = first.starts_with('#')
        && heading.starts_with(' ')
        && heading.trim().to_lowercase() == title.to_lowercase();
    let body = if repeated { rest.trim_start_matches('\n') } else { content };
    let byline = byline
        .map(str::trim)
        .filter(|byline| !byline.is_empty() && !body.contains(byline))
        .map_or_else(String::new, |byline| format!("{byline}\n\n"));
    format!("# {title}\n\n{byline}{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_name_their_markdown_versions_in_links() {
        let page = Url::parse("https://example.com/docs/page").expect("page");
        for (html, expected) in [
            (
                r#"<link rel="alternate stylesheet" type="text/markdown" href="page.md">"#,
                Some("https://example.com/docs/page.md"),
            ),
            (
                r#"<link rel="Alternate" type=" Text/X-Markdown " href="/api/page?format=md">"#,
                Some("https://example.com/api/page?format=md"),
            ),
            (
                r#"<base href="https://cdn.example/"><link rel="alternate" type="text/markdown" href="p.md">"#,
                Some("https://cdn.example/p.md"),
            ),
            (r#"<link rel="alternate" type="application/rss+xml" href="feed.xml">"#, None),
            (
                r#"<footer><a href="/docs/page/llms.txt">llms.txt</a></footer>"#,
                Some("https://example.com/docs/page/llms.txt"),
            ),
            (r#"<a href="/llms.txt">Site</a> <a href="/docs/other/llms.txt">Other</a>"#, None),
        ] {
            let found = alternate(&Document::from(html), &page).map(|url| url.to_string());
            assert_eq!(found.as_deref(), expected, "{html}");
        }
        let root = Url::parse("https://example.com/").expect("root");
        let site = Document::from(r#"<a href="/llms.txt">llms.txt</a>"#);
        assert_eq!(alternate(&site, &root), None, "a root page's llms.txt is read before its HTML");
    }

    #[test]
    fn main_content_is_read_without_the_page_around_it() {
        let paragraph =
            "Ownership is how Rust manages memory without a garbage collector. ".repeat(8);
        let html = format!(
            r#"<html><head><title>Ownership - The Rust Guide</title></head><body>
            <nav><a href="/">Home</a> <a href="/docs">Docs</a></nav>
            <aside class="sidebar">On this page</aside>
            <main><article><h1>Ownership</h1><p>{paragraph}</p>
            <p>Read about <a href="borrowing">borrowing</a> next.</p><p>{paragraph}</p></article></main>
            <footer><a href="/privacy">Privacy</a></footer></body></html>"#
        );
        let page = Url::parse("https://example.com/docs/ownership").expect("page");
        let markdown = markdown(Document::from(html.as_str()), &page);
        assert!(markdown.starts_with("# Ownership\n\nOwnership is how"), "{markdown}");
        assert!(markdown.contains("[borrowing](https://example.com/docs/borrowing)"), "{markdown}");
        for chrome in ["Home", "On this page", "Privacy", "The Rust Guide"] {
            assert!(!markdown.contains(chrome), "{chrome:?} in {markdown}");
        }
        let empty = Document::from("<html><head><title>App</title></head><body><div id=app></div>");
        assert_eq!(super::markdown(empty, &page), "", "a page that needs JavaScript has no text");
    }

    #[test]
    fn titles_name_the_page_without_its_site() {
        for (html, expected) in [
            (
                r##"<head><title>json — JSON encoder — Python docs</title></head>
                <h1>json — JSON encoder<a href="#json">¶</a></h1>"##,
                "json — JSON encoder",
            ),
            (
                r#"<head><title>Rust is Beautiful · Issue #1 · rust-lang/rust · GitHub</title>
                <meta property="og:site_name" content="GitHub"></head><h1>Rust is Beautiful <span>#1</span></h1>"#,
                "Rust is Beautiful · Issue #1 · rust-lang/rust",
            ),
            (
                "<head><title>ureq - Rust</title></head><h1>Crate ureq</h1><svg><title>Lock</title></svg>",
                "ureq - Rust",
            ),
        ] {
            assert_eq!(title(&Document::from(html)), expected, "{html}");
        }
    }

    #[test]
    fn content_is_titled_once() {
        for (title, byline, content, expected) in [
            ("Ownership", None, "## Ownership\n\nText", "# Ownership\n\nText"),
            ("Guide", Some("By Ada"), "Text", "# Guide\n\nBy Ada\n\nText"),
            ("Guide", Some("By Ada"), "## Part\n\nBy Ada", "# Guide\n\n## Part\n\nBy Ada"),
            ("Guide", None, "# Own heading\n\nText", "# Own heading\n\nText"),
            ("", None, "Text", "Text"),
            ("Guide", None, "", ""),
        ] {
            assert_eq!(titled(title, byline, content), expected, "{title:?} {content:?}");
        }
    }
}
