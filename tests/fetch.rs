//! `agt fetch` end to end: what it asks a site for, which of a page's sources
//! it reads, what it prints when a command pipes it, and what a session's
//! agent sees.

mod support;

use std::collections::BTreeMap;

use serde_json::json;
use support::{Answer, Server, Site, agt, call, golden, text};

/// What a site's agent saw, with what changes between runs replaced: the
/// site's address, and the name agt saves a page under, which follows it.
fn steady(shown: &str, site: &Site) -> String {
    let text = shown.replace(&site.url, "$SITE");
    let (mut out, mut rest) = (String::with_capacity(text.len()), text.as_str());
    while let Some(start) = rest.find("/fetch/") {
        let after = &rest[start + "/fetch/".len()..];
        let Some(end) = after.find(".md") else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push_str("/fetch/$PAGE.md");
        rest = &after[end + ".md".len()..];
    }
    out.push_str(rest);
    out
}

/// A page of `body`, sent as `media`.
fn page(media: &str, body: &str) -> Answer {
    let headers = vec![("content-type".to_owned(), media.to_owned())];
    (200, headers, body.as_bytes().to_vec())
}

/// A documentation site: its root has an llms.txt, one page is Markdown, one
/// names its Markdown version, one names a version that is gone, one moved,
/// and one is a file agt does not read.
fn docs(path: &str, headers: &BTreeMap<String, String>) -> Answer {
    let markdown = headers.get("accept").is_some_and(|accept| accept.starts_with("text/markdown"));
    match path {
        "/llms.txt" => page("text/plain", "# The site\n\nFor agents.\n"),
        "/docs" if markdown => page("text/markdown", "# Docs\n\nMarkdown as sent.\n"),
        "/guide" => page(
            "text/html",
            "<html><head><title>Guide - The Site</title>\
             <link rel=\"alternate\" type=\"text/markdown\" href=\"/guide.md\"></head>\
             <body><nav>Home</nav><main><h1>Guide</h1><p>The HTML.</p></main></body></html>",
        ),
        "/guide.md" => page("text/markdown", "# Guide\n\nThe Markdown version.\n"),
        "/only-html" => page(
            "text/html",
            "<html><head><title>Only HTML - The Site</title>\
             <link rel=\"alternate\" type=\"text/markdown\" href=\"/gone.md\"></head>\
             <body><main><h1>Only HTML</h1><p>Read <a href=\"/docs\">the docs</a>.</p></main></body></html>",
        ),
        "/moved" => (302, vec![("location".to_owned(), "/docs".to_owned())], Vec::new()),
        "/paper.pdf" => page("application/pdf", "%PDF-1.7 not a page"),
        "/" => page("text/html", "<html><body><main><p>The home page.</p></main></body></html>"),
        _ => (404, vec![("content-type".to_owned(), "text/plain".to_owned())], b"gone".to_vec()),
    }
}

#[test]
fn a_page_is_read_from_the_best_source_it_has() {
    let home = tempfile::tempdir().expect("temp dir");
    let model = Server::start(Vec::new());
    let site = Site::start(docs);
    let fetch = |path: &str| {
        let output = agt(home.path(), &model)
            .args(["fetch", &format!("{}{path}", site.url)])
            .output()
            .expect("agt runs");
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        (output.status.code(), text(&output.stdout), text(&output.stderr))
    };
    let read = |path: &str| fetch(path).1;

    assert_eq!(read("/docs"), "# Docs\n\nMarkdown as sent.\n", "Markdown is read as sent");
    assert_eq!(read("/guide"), "# Guide\n\nThe Markdown version.\n", "a page names its Markdown");
    assert_eq!(read("/"), "# The site\n\nFor agents.\n", "a root page is read from llms.txt");
    assert_eq!(read("/moved"), "# Docs\n\nMarkdown as sent.\n", "a page that moved is followed");
    assert_eq!(
        read("/only-html"),
        format!("# Only HTML\n\nRead [the docs]({}/docs).\n", site.url),
        "a Markdown version that is gone gives way to the page, whose links are absolute",
    );

    let asked = site.asked();
    let (path, headers) = asked.first().expect("a request");
    assert_eq!(path, "/docs");
    assert_eq!(
        headers.get("accept").map(String::as_str),
        Some("text/markdown, text/html;q=0.9, text/plain;q=0.8, */*;q=0.1"),
        "Markdown is asked for first",
    );
    assert_eq!(headers.get("accept-encoding").map(String::as_str), Some("gzip"));
    assert!(
        headers.get("user-agent").is_some_and(|agent| agent.starts_with("agt/")),
        "agt says who it is: {headers:?}",
    );
}

#[test]
fn what_is_not_a_page_says_what_to_do_instead() {
    let home = tempfile::tempdir().expect("temp dir");
    let model = Server::start(Vec::new());
    let site = Site::start(docs);
    let fetch = |arguments: &[&str]| {
        let output =
            agt(home.path(), &model).arg("fetch").args(arguments).output().expect("agt runs");
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        (output.status.code(), text(&output.stdout), text(&output.stderr))
    };
    let missing = format!("{}/missing", site.url);
    assert_eq!(
        fetch(&[&missing]),
        (Some(1), String::new(), format!("agt fetch: {missing} answered 404 Not Found\n")),
    );
    let paper = format!("{}/paper.pdf", site.url);
    assert_eq!(
        fetch(&[&paper]),
        (
            Some(1),
            String::new(),
            format!(
                "agt fetch: {paper} is application/pdf of 19 B, not a page agt reads; download it with curl -o <file> '{paper}'\n"
            ),
        ),
    );
    let (code, out, problem) = fetch(&["ftp://example.com/file"]);
    assert_eq!(
        (code, out),
        (Some(2), String::new()),
        "a command line agt cannot take exits with 2"
    );
    assert!(
        problem.starts_with(
            "agt fetch: \"ftp://example.com/file\" is not an http:// or https:// URL\n\nUsage: agt fetch <url>"
        ),
        "{problem}",
    );
}

#[test]
fn a_long_page_reaches_the_agent_as_an_outline_beside_the_file_it_is_saved_in() {
    let home = tempfile::tempdir().expect("temp dir");
    let site = Site::start(|path, _| match path {
        "/long" => {
            let parts: String = (1..=60)
                .map(|part| format!("## Part {part}\n\n{}\n\n", "word ".repeat(40).trim_end()))
                .collect();
            page("text/markdown", &format!("# The guide\n\nIt is long.\n\n{parts}"))
        }
        _ => (404, Vec::new(), Vec::new()),
    });
    let command = format!("agt fetch {}/long", site.url);
    let model = Server::start(vec![
        call("call_1", "bash", json!({ "command": command }), 100),
        text("read"),
    ]);
    let output =
        agt(home.path(), &model).args(["-p", "read the guide"]).output().expect("agt runs");
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));

    let requests = model.requests();
    let input = requests[1]["body"]["input"].as_array().expect("the second request's input");
    let result = input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .and_then(|item| item["output"].as_str())
        .expect("the command's result");
    golden("fetch-outline.txt", home.path(), &steady(result, &site));
}
