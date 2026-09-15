//! Pages better read from another source, and those sources: a code host's raw
//! file for its view of the file; GitHub's API for a repository's README, a
//! directory, an issue or a pull request; an arXiv paper's HTML for its abstract
//! or PDF; the publisher's page for a DOI; a registry's metadata for a
//! package's page; and the Stack Exchange API for a question and its answers.
//!
//! A source that fails gives way to the page itself, so a route that does not
//! fit a page costs a request and nothing more.

use std::borrow::Cow;

use dom_query::Document;
use percent_encoding::percent_decode_str;
use serde_json::Value;

use super::http::{Client, MARKDOWN_FIRST, Response};
use super::markdown::{self, words};
use super::{Error, Form, Page, Source, Url, clean, plural, read};
use crate::bash::bytes;

/// The most answers to a question read, best first.
const ANSWERS: usize = 10;
/// GitHub's first path segments that name pages rather than owners.
const GITHUB_PAGES: [&str; 22] = [
    "about",
    "apps",
    "collections",
    "customer-stories",
    "enterprise",
    "events",
    "explore",
    "features",
    "issues",
    "login",
    "marketplace",
    "new",
    "notifications",
    "orgs",
    "pricing",
    "pulls",
    "search",
    "security",
    "settings",
    "sponsors",
    "topics",
    "trending",
];
/// Hugging Face's first path segments that name pages rather than owners.
const HUGGING_FACE_PAGES: [&str; 11] = [
    "blog",
    "collections",
    "datasets",
    "docs",
    "learn",
    "models",
    "organizations",
    "papers",
    "posts",
    "spaces",
    "tasks",
];

/// The source a kind of page agt knows is read from.
#[derive(Debug)]
pub(super) struct Route {
    /// The first URL read.
    url: Url,
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    /// The page, asked for as HTML, for a site that answers a request for
    /// Markdown with something else: doi.org answers with a citation.
    Html,
    /// A code host's raw file.
    Raw,
    /// An arXiv paper's HTML, and the page to read when it has none.
    Paper { fallback: Option<Url> },
    /// A GitHub repository's README.
    Readme,
    /// A GitHub repository's directory, named as `owner/repo/path at reference`.
    Directory { name: String },
    /// A GitHub issue or pull request, and where its comments are.
    Issue { comments: Url },
    /// An npm package's metadata at a version.
    Npm,
    /// A PyPI project's metadata.
    Pypi,
    /// A crate's metadata, at a version or else its latest stable one.
    Crate { name: String, version: Option<String> },
    /// A Stack Exchange question, where its answers are, and the site's address.
    Question { answers: Url, site: Url },
}

/// The source agt knows for the page at `url`, if it knows one.
pub(super) fn route(url: &Url) -> Option<Route> {
    let host = url.host();
    let host = host.strip_prefix("www.").unwrap_or(host);
    let segments = url.segments();
    let at = |address: String, kind: Kind| Url::parse(&address).ok().map(|url| Route { url, kind });
    match (host, segments.as_slice()) {
        ("github.com", [owner, ..]) if GITHUB_PAGES.contains(owner) => None,
        ("github.com", [owner, repo]) => {
            at(format!("https://api.github.com/repos/{owner}/{repo}/readme"), Kind::Readme)
        }
        ("github.com", [owner, repo, "blob", file @ ..]) if file.len() >= 2 => {
            at(format!("https://github.com/{owner}/{repo}/raw/{}", file.join("/")), Kind::Raw)
        }
        ("github.com", [owner, repo, "tree", reference, path @ ..]) => {
            let (name, path) = match path.join("/") {
                path if path.is_empty() => (format!("{owner}/{repo} at {reference}"), path),
                path => (format!("{owner}/{repo}/{path} at {reference}"), format!("/{path}")),
            };
            let contents = format!("https://api.github.com/repos/{owner}/{repo}/contents{path}");
            at(format!("{contents}?ref={reference}"), Kind::Directory { name })
        }
        ("github.com", [owner, repo, "issues" | "pull", number, ..])
            if number.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            let issue = format!("https://api.github.com/repos/{owner}/{repo}/issues/{number}");
            let comments = Url::parse(&format!("{issue}/comments?per_page=100")).ok()?;
            at(issue, Kind::Issue { comments })
        }
        ("gitlab.com", _) => {
            let marker = segments.windows(2).position(|pair| pair == ["-", "blob"])?;
            let (project, file) = (&segments[..marker], &segments[marker + 2..]);
            (project.len() >= 2 && file.len() >= 2).then_some(())?;
            let raw = format!("https://gitlab.com/{}/-/raw/{}", project.join("/"), file.join("/"));
            at(raw, Kind::Raw)
        }
        ("codeberg.org", [owner, repo, "src", kind @ ("branch" | "tag" | "commit"), file @ ..])
            if file.len() >= 2 =>
        {
            let raw = format!("https://codeberg.org/{owner}/{repo}/raw/{kind}/{}", file.join("/"));
            at(raw, Kind::Raw)
        }
        ("huggingface.co", ["datasets", owner, name]) => at(
            format!("https://huggingface.co/datasets/{owner}/{name}/raw/main/README.md"),
            Kind::Raw,
        ),
        ("huggingface.co", [owner, name]) if !HUGGING_FACE_PAGES.contains(owner) => {
            at(format!("https://huggingface.co/{owner}/{name}/raw/main/README.md"), Kind::Raw)
        }
        ("huggingface.co", _) => {
            let blob =
                segments.iter().position(|segment| *segment == "blob").filter(|at| *at >= 2)?;
            let mut path = segments.clone();
            path[blob] = "raw";
            at(format!("https://huggingface.co/{}", path.join("/")), Kind::Raw)
        }
        ("arxiv.org" | "export.arxiv.org", [page @ ("abs" | "pdf"), id @ ..]) if !id.is_empty() => {
            let id = id.join("/");
            let id = id.strip_suffix(".pdf").unwrap_or(&id);
            // A PDF with no HTML version is better read from its abstract.
            let fallback = match *page {
                "pdf" => Url::parse(&format!("https://arxiv.org/abs/{id}")).ok(),
                _ => None,
            };
            at(format!("https://arxiv.org/html/{id}"), Kind::Paper { fallback })
        }
        ("doi.org" | "dx.doi.org", [_, ..]) => Some(Route { url: url.clone(), kind: Kind::Html }),
        ("npmjs.com", ["package", rest @ ..]) => {
            let (name, version) = npm_package(rest)?;
            let name = name.replace('/', "%2F");
            at(format!("https://registry.npmjs.org/{name}/{version}"), Kind::Npm)
        }
        ("pypi.org", ["project", name, version @ ..]) if version.len() <= 1 => {
            let version = version.first().map_or_else(String::new, |version| format!("/{version}"));
            at(format!("https://pypi.org/pypi/{name}{version}/json"), Kind::Pypi)
        }
        ("crates.io", ["crates", name, version @ ..]) if version.len() <= 1 => {
            let kind = Kind::Crate {
                name: (*name).to_owned(),
                version: version.first().map(|version| (*version).to_owned()),
            };
            at(format!("https://crates.io/api/v1/crates/{name}"), kind)
        }
        (_, ["questions", id, ..]) if id.bytes().all(|byte| byte.is_ascii_digit()) => {
            let site = stack_exchange(host)?;
            let api = format!("https://api.stackexchange.com/2.3/questions/{id}");
            let answers =
                format!("{api}/answers?site={site}&filter=withbody&sort=votes&pagesize={ANSWERS}");
            let kind = Kind::Question { answers: Url::parse(&answers).ok()?, site: url.join("/")? };
            at(format!("{api}?site={site}&filter=withbody"), kind)
        }
        _ => None,
    }
}

impl Route {
    /// Reads the page from this source.
    pub(super) fn read(&self, client: &Client) -> Result<Page, Error> {
        let get = |url: &Url, accept: &str| client.get(url, &[("accept", accept)]);
        let url = &self.url;
        match &self.kind {
            Kind::Html => read(client, get(url, "text/html")?),
            Kind::Raw => {
                let page = read(client, get(url, "*/*")?)?;
                Ok(Page { source: Source::Site("the raw file"), ..page })
            }
            Kind::Paper { fallback } => {
                let paper = get(url, MARKDOWN_FIRST).and_then(|response| read(client, response));
                match (paper, fallback) {
                    (Ok(page), _) => Ok(Page { source: Source::Site("the paper's HTML"), ..page }),
                    (Err(_), Some(fallback)) => {
                        let page = read(client, get(fallback, MARKDOWN_FIRST)?)?;
                        Ok(Page { source: Source::Site("the paper's abstract"), ..page })
                    }
                    (Err(error), None) => Err(error),
                }
            }
            Kind::Readme => {
                let response = github(client, url, "application/vnd.github.raw")?;
                let text = String::from_utf8_lossy(&response.body);
                site("the README from GitHub's API", response.url, &text)
            }
            Kind::Directory { name } => {
                let (answered, listing) =
                    json(github(client, url, "application/vnd.github+json")?)?;
                let text = directory_markdown(name, &listing)
                    .ok_or(Error::Unexpected(answered.clone()))?;
                site("the directory from GitHub's API", answered, &text)
            }
            Kind::Issue { comments } => {
                let (answered, issue) = json(github(client, url, "application/vnd.github+json")?)?;
                let comments = github(client, comments, "application/vnd.github+json")
                    .and_then(json)
                    .map_or(Value::Null, |(_, comments)| comments);
                let text =
                    issue_markdown(&issue, &comments).ok_or(Error::Unexpected(answered.clone()))?;
                site("the issue and its comments from GitHub's API", answered, &text)
            }
            Kind::Npm => {
                let (answered, package) = json(get(url, "application/json")?)?;
                let text = npm_markdown(&package).ok_or(Error::Unexpected(answered.clone()))?;
                site("the package from npm's registry", answered, &text)
            }
            Kind::Pypi => {
                let (answered, project) = json(get(url, "application/json")?)?;
                let text =
                    pypi_markdown(&project["info"]).ok_or(Error::Unexpected(answered.clone()))?;
                site("the project from PyPI's API", answered, &text)
            }
            Kind::Crate { name, version } => {
                let (answered, info) = json(get(url, "application/json")?)?;
                let package = &info["crate"];
                let latest =
                    text(&package["max_stable_version"]).or_else(|| text(&package["max_version"]));
                let version =
                    version.as_deref().or(latest).ok_or(Error::Unexpected(answered.clone()))?;
                let readme = url
                    .join(&format!("/api/v1/crates/{name}/{version}/readme"))
                    .and_then(|readme| get(&readme, "text/html").ok())
                    .map(|response| {
                        let html = String::from_utf8_lossy(&response.body).into_owned();
                        markdown::write(&Document::fragment(html), &response.url)
                    });
                let text = crate_markdown(package, version, readme.as_deref())
                    .ok_or(Error::Unexpected(answered.clone()))?;
                site("the crate from crates.io's API", answered, &text)
            }
            Kind::Question { answers, site: address } => {
                let (answered, question) = json(get(url, "application/json")?)?;
                let answers = get(answers, "application/json")
                    .and_then(json)
                    .map_or(Value::Null, |(_, answers)| answers);
                let text = question_markdown(&question["items"][0], &answers["items"], address)
                    .ok_or(Error::Unexpected(answered.clone()))?;
                site("the question and its answers from the Stack Exchange API", answered, &text)
            }
        }
    }
}

/// A page read as Markdown from a site's source, which its header names.
fn site(source: &'static str, url: Url, text: &str) -> Result<Page, Error> {
    let text = clean(text);
    if text.is_empty() {
        return Err(Error::Empty(url));
    }
    Ok(Page { source: Source::Site(source), url, form: Form::Markdown, text })
}

/// GETs `url` from GitHub's API, asking for `accept`, with the token
/// `GH_TOKEN` or `GITHUB_TOKEN` holds, which raises GitHub's limit of 60
/// requests an hour.
fn github(client: &Client, url: &Url, accept: &str) -> Result<Response, Error> {
    let token = ["GH_TOKEN", "GITHUB_TOKEN"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|token| !token.trim().is_empty()));
    let authorization = token.map(|token| format!("Bearer {}", token.trim()));
    let mut headers = vec![("accept", accept)];
    if let Some(authorization) = &authorization {
        headers.push(("authorization", authorization));
    }
    client.get(url, &headers)
}

/// The JSON `response` holds, with the URL that answered.
fn json(response: Response) -> Result<(Url, Value), Error> {
    match serde_json::from_slice(&response.body) {
        Ok(value) => Ok((response.url, value)),
        Err(_) => Err(Error::Unexpected(response.url)),
    }
}

/// The Stack Exchange API's name for the site at `host`.
fn stack_exchange(host: &str) -> Option<String> {
    match host {
        "stackoverflow.com" | "superuser.com" | "serverfault.com" | "askubuntu.com"
        | "stackapps.com" => host.strip_suffix(".com").map(str::to_owned),
        "mathoverflow.net" => Some(host.to_owned()),
        _ => host
            .strip_suffix(".stackexchange.com")
            .filter(|name| !name.is_empty() && !name.contains('.'))
            .map(str::to_owned),
    }
}

/// An npm package's name and version from the path after `/package/`: a name
/// such as `react`, or `@babel/core` in one segment or two, then `v/<version>`
/// or else `latest`.
fn npm_package(rest: &[&str]) -> Option<(String, String)> {
    let decoded: Vec<String> = rest
        .iter()
        .map(|segment| percent_decode_str(segment).decode_utf8().ok().map(Cow::into_owned))
        .collect::<Option<_>>()?;
    let (name, rest) = match decoded.as_slice() {
        [scoped, rest @ ..] if scoped.starts_with('@') && scoped.contains('/') => {
            (scoped.clone(), rest)
        }
        [scope, name, rest @ ..] if scope.starts_with('@') => (format!("{scope}/{name}"), rest),
        [name, rest @ ..] => (name.clone(), rest),
        [] => return None,
    };
    match rest {
        [] => Some((name, "latest".to_owned())),
        [marker, version] if *marker == "v" => Some((name, version.clone())),
        _ => None,
    }
}

/// The trimmed text of `value`, unless it has none.
fn text(value: &Value) -> Option<&str> {
    value.as_str().map(str::trim).filter(|text| !text.is_empty())
}

/// Appends the `facts` that have values, as a list.
fn facts(out: &mut String, facts: &[(&str, Option<String>)]) {
    let lines: Vec<String> = facts
        .iter()
        .filter_map(|(name, value)| Some(format!("- {name}: {}", value.as_ref()?)))
        .collect();
    if !lines.is_empty() {
        out.push('\n');
        out.push_str(&lines.join("\n"));
        out.push('\n');
    }
}

/// A GitHub directory's `listing`: directories first, each linked to where agt
/// reads it.
fn directory_markdown(name: &str, listing: &Value) -> Option<String> {
    let entries = listing.as_array()?;
    let (directories, files): (Vec<&Value>, Vec<&Value>) =
        entries.iter().partition(|entry| entry["type"] == "dir");
    let mut lines = vec![format!("# {name}"), String::new()];
    for entry in directories {
        lines.push(format!("- [{}/]({})", text(&entry["name"])?, text(&entry["html_url"])?));
    }
    for entry in files {
        let link = text(&entry["download_url"]).or_else(|| text(&entry["html_url"]))?;
        let size =
            entry["size"].as_u64().map_or_else(String::new, |size| format!(" · {}", bytes(size)));
        lines.push(format!("- [{}]({link}){size}", text(&entry["name"])?));
    }
    Some(lines.join("\n"))
}

/// The day of a time as GitHub's API writes it, such as `2026-09-01T10:00:00Z`.
fn day(value: &Value) -> &str {
    text(value).and_then(|time| time.get(..10)).unwrap_or("an unknown date")
}

/// A GitHub issue or pull request, and the `comments` read of it.
fn issue_markdown(issue: &Value, comments: &Value) -> Option<String> {
    let pull = issue.get("pull_request").filter(|pull| pull.is_object());
    let state = match pull.and_then(|pull| text(&pull["merged_at"])) {
        Some(_) => "Merged",
        None if issue["state"] == "closed" => "Closed",
        None => "Open",
    };
    let kind = if pull.is_some() { "pull request" } else { "issue" };
    let author = |post: &Value| text(&post["user"]["login"]).unwrap_or("ghost").to_owned();
    let mut about =
        vec![format!("{state} {kind} by @{}, opened {}", author(issue), day(&issue["created_at"]))];
    let labels = issue["labels"].as_array().into_iter().flatten();
    let labels: Vec<&str> = labels.filter_map(|label| text(&label["name"])).collect();
    if !labels.is_empty() {
        about.push(format!("labels: {}", labels.join(", ")));
    }
    let count = issue["comments"].as_i64().unwrap_or(0);
    about.push(plural(count, "comment"));
    let (title, number) = (text(&issue["title"])?, issue["number"].as_u64()?);
    let mut out = format!("# {title} (#{number})\n\n{}\n\n", about.join(" · "));
    let link = text(&issue["html_url"]);
    if let (Some(_), Some(link)) = (pull, link) {
        out.push_str(&format!("The diff: {link}.diff\n\n"));
    }
    out.push_str(text(&issue["body"]).unwrap_or("No description was given."));
    out.push('\n');
    let comments = comments.as_array().map(Vec::as_slice).unwrap_or_default();
    for comment in comments {
        let body = text(&comment["body"]).unwrap_or_default();
        out.push_str(&format!(
            "\n## @{} on {}\n\n{body}\n",
            author(comment),
            day(&comment["created_at"])
        ));
    }
    let more = count - comments.len() as i64;
    if let (true, Some(link)) = (more > 0, link) {
        out.push_str(&format!("\n{} more on {link}\n", plural(more, "comment")));
    }
    Some(out)
}

/// An npm package version's metadata.
fn npm_markdown(package: &Value) -> Option<String> {
    let (name, version) = (text(&package["name"])?, text(&package["version"])?);
    let mut out = format!("# {name} {version}\n\n");
    if let Some(description) = text(&package["description"]) {
        out.push_str(&format!("{description}\n\n"));
    }
    if let Some(deprecated) = text(&package["deprecated"]) {
        out.push_str(&format!("> Deprecated: {deprecated}\n\n"));
    }
    out.push_str(&format!("```sh\nnpm install {name}@{version}\n```\n"));
    let link = |value: &Value| text(value).or_else(|| text(&value["url"])).map(repository);
    let license = text(&package["license"]).or_else(|| text(&package["license"]["type"]));
    facts(
        &mut out,
        &[
            ("License", license.map(str::to_owned)),
            ("Homepage", link(&package["homepage"])),
            ("Repository", link(&package["repository"])),
            ("Issues", link(&package["bugs"])),
        ],
    );
    for (title, field) in [
        ("Dependencies", "dependencies"),
        ("Peer dependencies", "peerDependencies"),
        ("Optional dependencies", "optionalDependencies"),
        ("Engines", "engines"),
        ("Commands", "bin"),
    ] {
        let Some(entries) = package[field].as_object().filter(|entries| !entries.is_empty()) else {
            continue;
        };
        out.push_str(&format!("\n## {title}\n\n"));
        for (key, value) in entries {
            out.push_str(&format!("- `{key}`: `{}`\n", value.as_str().unwrap_or_default()));
        }
    }
    if let Some(readme) =
        text(&package["readme"]).filter(|readme| *readme != "ERROR: No README data found!")
    {
        out.push_str(&format!("\n## README\n\n{readme}\n"));
    }
    Some(out)
}

/// A repository's address as a browser opens it, from the forms package
/// metadata writes, such as `git+https://github.com/o/r.git`.
fn repository(address: &str) -> String {
    let address = address.strip_prefix("git+").unwrap_or(address);
    let address = match address.strip_prefix("git://") {
        Some(rest) => format!("https://{rest}"),
        None => address.to_owned(),
    };
    address.strip_suffix(".git").map_or_else(|| address.clone(), str::to_owned)
}

/// A PyPI project's metadata, its `info`.
fn pypi_markdown(info: &Value) -> Option<String> {
    let (name, version) = (text(&info["name"])?, text(&info["version"])?);
    let mut out = format!("# {name} {version}\n\n");
    if let Some(summary) = text(&info["summary"]) {
        out.push_str(&format!("{summary}\n\n"));
    }
    out.push_str(&format!("```sh\npip install {name}=={version}\n```\n"));
    // A license field sometimes holds the license's whole text.
    let license = text(&info["license_expression"]).or_else(|| {
        text(&info["license"]).filter(|license| !license.contains('\n') && license.len() <= 80)
    });
    let mut listed = vec![
        ("Requires Python", text(&info["requires_python"]).map(str::to_owned)),
        ("License", license.map(str::to_owned)),
    ];
    for (label, link) in info["project_urls"].as_object().into_iter().flatten() {
        listed.push((label.as_str(), text(link).map(str::to_owned)));
    }
    facts(&mut out, &listed);
    let requires: Vec<&str> =
        info["requires_dist"].as_array().into_iter().flatten().filter_map(text).collect();
    if !requires.is_empty() {
        out.push_str("\n## Dependencies\n\n");
        for requirement in requires {
            out.push_str(&format!("- `{requirement}`\n"));
        }
    }
    if let Some(description) = text(&info["description"]) {
        out.push_str(&format!("\n## Description\n\n{description}\n"));
    }
    Some(out)
}

/// A crate's metadata at `version`, with its README.
fn crate_markdown(package: &Value, version: &str, readme: Option<&str>) -> Option<String> {
    let name = text(&package["name"])?;
    let mut out = format!("# {name} {version}\n\n");
    if let Some(description) = text(&package["description"]) {
        out.push_str(&format!("{description}\n\n"));
    }
    out.push_str(&format!("```toml\n{name} = \"{version}\"\n```\n"));
    let documentation = text(&package["documentation"])
        .map_or_else(|| format!("https://docs.rs/{name}/{version}"), str::to_owned);
    facts(
        &mut out,
        &[
            ("Documentation", Some(documentation)),
            ("Repository", text(&package["repository"]).map(str::to_owned)),
            ("Homepage", text(&package["homepage"]).map(str::to_owned)),
            ("Downloads", package["downloads"].as_u64().map(|downloads| downloads.to_string())),
        ],
    );
    if let Some(readme) = readme.filter(|readme| !readme.trim().is_empty()) {
        out.push_str(&format!("\n## README\n\n{readme}\n"));
    }
    Some(out)
}

/// A Stack Exchange question and its `answers`, whose HTML links resolve
/// against the `site`.
fn question_markdown(question: &Value, answers: &Value, site: &Url) -> Option<String> {
    let decode = |html: &str| words(&Document::fragment(html).text());
    let owner = |post: &Value| {
        text(&post["owner"]["display_name"]).map_or_else(|| "a deleted user".to_owned(), decode)
    };
    let score = |post: &Value| plural(post["score"].as_i64().unwrap_or(0), "vote");
    let body = |post: &Value| {
        markdown::write(&Document::fragment(text(&post["body"]).unwrap_or_default()), site)
    };
    let tags: Vec<&str> =
        question["tags"].as_array().into_iter().flatten().filter_map(text).collect();
    let tags = match tags.is_empty() {
        true => String::new(),
        false => format!(" · tags: {}", tags.join(", ")),
    };
    let answered = plural(question["answer_count"].as_i64().unwrap_or(0), "answer");
    let mut out = format!(
        "# {}\n\n{} · {answered} · asked by {}{tags}\n\n{}\n",
        decode(text(&question["title"])?),
        score(question),
        owner(question),
        body(question),
    );
    for answer in answers.as_array().into_iter().flatten().take(ANSWERS) {
        let accepted = if answer["is_accepted"] == true { " · accepted" } else { "" };
        out.push_str(&format!(
            "\n## Answer by {} · {}{accepted}\n\n{}\n",
            owner(answer),
            score(answer),
            body(answer)
        ));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn known_pages_are_read_from_their_sources() {
        for (page, source) in [
            (
                "https://github.com/rust-lang/rust",
                Some("https://api.github.com/repos/rust-lang/rust/readme"),
            ),
            ("https://www.github.com/o/r/", Some("https://api.github.com/repos/o/r/readme")),
            (
                "https://github.com/o/r/blob/main/docs/a%20b.md?plain=1",
                Some("https://github.com/o/r/raw/main/docs/a%20b.md"),
            ),
            (
                "https://github.com/o/blob/blob/main/README.md",
                Some("https://github.com/o/blob/raw/main/README.md"),
            ),
            (
                "https://github.com/o/r/tree/main/src/cli",
                Some("https://api.github.com/repos/o/r/contents/src/cli?ref=main"),
            ),
            (
                "https://github.com/o/r/tree/v1.0",
                Some("https://api.github.com/repos/o/r/contents?ref=v1.0"),
            ),
            (
                "https://github.com/o/r/issues/42#issuecomment-1",
                Some("https://api.github.com/repos/o/r/issues/42"),
            ),
            (
                "https://github.com/o/r/pull/7/files",
                Some("https://api.github.com/repos/o/r/issues/7"),
            ),
            ("https://github.com/o/r/pulls", None),
            ("https://github.com/o/r/blob/main", None),
            ("https://github.com/settings/profile", None),
            (
                "https://gitlab.com/group/sub/project/-/blob/main/docs/guide.md",
                Some("https://gitlab.com/group/sub/project/-/raw/main/docs/guide.md"),
            ),
            ("https://gitlab.com/group/project/-/tree/main/docs", None),
            (
                "https://codeberg.org/forgejo/forgejo/src/branch/forgejo/README.md",
                Some("https://codeberg.org/forgejo/forgejo/raw/branch/forgejo/README.md"),
            ),
            (
                "https://huggingface.co/openai/gpt-oss-20b",
                Some("https://huggingface.co/openai/gpt-oss-20b/raw/main/README.md"),
            ),
            (
                "https://huggingface.co/datasets/o/d",
                Some("https://huggingface.co/datasets/o/d/raw/main/README.md"),
            ),
            (
                "https://huggingface.co/o/m/blob/main/config.json",
                Some("https://huggingface.co/o/m/raw/main/config.json"),
            ),
            ("https://huggingface.co/docs/transformers", None),
            ("https://arxiv.org/abs/2402.08954v2", Some("https://arxiv.org/html/2402.08954v2")),
            (
                "https://arxiv.org/pdf/hep-th/9901001.pdf",
                Some("https://arxiv.org/html/hep-th/9901001"),
            ),
            ("https://arxiv.org/html/2402.08954", None),
            (
                "https://doi.org/10.1038/s41586-021-03819-2",
                Some("https://doi.org/10.1038/s41586-021-03819-2"),
            ),
            (
                "https://www.npmjs.com/package/react",
                Some("https://registry.npmjs.org/react/latest"),
            ),
            (
                "https://www.npmjs.com/package/@babel/core/v/7.28.0",
                Some("https://registry.npmjs.org/@babel%2Fcore/7.28.0"),
            ),
            (
                "https://npmjs.com/package/%40babel%2Fcore",
                Some("https://registry.npmjs.org/@babel%2Fcore/latest"),
            ),
            ("https://www.npmjs.com/package/react/access", None),
            ("https://pypi.org/project/requests/", Some("https://pypi.org/pypi/requests/json")),
            (
                "https://pypi.org/project/requests/2.32.3/",
                Some("https://pypi.org/pypi/requests/2.32.3/json"),
            ),
            ("https://crates.io/crates/ureq", Some("https://crates.io/api/v1/crates/ureq")),
            (
                "https://stackoverflow.com/questions/11227809/why-is-processing",
                Some(
                    "https://api.stackexchange.com/2.3/questions/11227809?site=stackoverflow&filter=withbody",
                ),
            ),
            (
                "https://unix.stackexchange.com/questions/1/x",
                Some("https://api.stackexchange.com/2.3/questions/1?site=unix&filter=withbody"),
            ),
            ("https://example.com/questions/1", None),
            ("https://example.com/o/r/blob/main/README.md", None),
        ] {
            let url = Url::parse(page).expect("page");
            let routed = route(&url).map(|route| route.url.to_string());
            assert_eq!(routed.as_deref(), source, "{page}");
        }
    }

    #[test]
    fn github_issues_and_directories_read_as_markdown() {
        let issue = json!({
            "number": 7, "title": "Add fetch", "state": "closed", "user": { "login": "ada" },
            "created_at": "2026-09-01T10:00:00Z", "labels": [{ "name": "feature" }], "comments": 3,
            "body": "Reads pages.\r\n", "html_url": "https://github.com/o/r/pull/7",
            "pull_request": { "merged_at": "2026-09-02T00:00:00Z" },
        });
        let comments = json!([
            { "user": { "login": "lin" }, "created_at": "2026-09-01T11:00:00Z", "body": "Looks good." },
            { "user": null, "created_at": "2026-09-01T12:00:00Z", "body": "Merged." },
        ]);
        let expected = "\
# Add fetch (#7)

Merged pull request by @ada, opened 2026-09-01 · labels: feature · 3 comments

The diff: https://github.com/o/r/pull/7.diff

Reads pages.

## @lin on 2026-09-01

Looks good.

## @ghost on 2026-09-01

Merged.

1 comment more on https://github.com/o/r/pull/7
";
        assert_eq!(issue_markdown(&issue, &comments).as_deref(), Some(expected));
        let open = json!({ "number": 1, "title": "Bug", "state": "open", "user": { "login": "ada" }, "comments": 0 });
        assert_eq!(
            issue_markdown(&open, &Value::Null).as_deref(),
            Some(
                "# Bug (#1)\n\nOpen issue by @ada, opened an unknown date · 0 comments\n\nNo description was given.\n"
            )
        );

        let listing = json!([
            {
                "name": "main.rs", "type": "file", "size": 1400,
                "download_url": "https://raw.githubusercontent.com/o/r/main/src/main.rs",
                "html_url": "https://github.com/o/r/blob/main/src/main.rs",
            },
            { "name": "cli", "type": "dir", "size": 0, "download_url": null, "html_url": "https://github.com/o/r/tree/main/src/cli" },
        ]);
        let expected = "# o/r/src at main\n\n- [cli/](https://github.com/o/r/tree/main/src/cli)\n- [main.rs](https://raw.githubusercontent.com/o/r/main/src/main.rs) · 1.4 KB";
        assert_eq!(directory_markdown("o/r/src at main", &listing).as_deref(), Some(expected));
        assert_eq!(
            directory_markdown("o/r at main", &json!({ "type": "file" })),
            None,
            "a file is no directory"
        );
    }

    #[test]
    fn packages_read_as_their_registries_describe_them() {
        let npm = json!({
            "name": "@acme/widget", "version": "2.3.1", "description": "A widget.", "license": "MIT",
            "repository": { "type": "git", "url": "git+https://github.com/acme/widget.git" },
            "bugs": { "url": "https://github.com/acme/widget/issues" },
            "dependencies": { "alpha": "^1.0.0" }, "engines": { "node": ">=20" },
        });
        let expected = "\
# @acme/widget 2.3.1

A widget.

```sh
npm install @acme/widget@2.3.1
```

- License: MIT
- Repository: https://github.com/acme/widget
- Issues: https://github.com/acme/widget/issues

## Dependencies

- `alpha`: `^1.0.0`

## Engines

- `node`: `>=20`
";
        assert_eq!(npm_markdown(&npm).as_deref(), Some(expected));

        let pypi = json!({
            "name": "requests", "version": "2.32.3", "summary": "HTTP for Humans.", "requires_python": ">=3.8",
            "license": "Apache-2.0", "project_urls": { "Source": "https://github.com/psf/requests" },
            "requires_dist": ["charset_normalizer<4,>=2"], "description": "# Requests\n\nUse it.",
        });
        let expected = "\
# requests 2.32.3

HTTP for Humans.

```sh
pip install requests==2.32.3
```

- Requires Python: >=3.8
- License: Apache-2.0
- Source: https://github.com/psf/requests

## Dependencies

- `charset_normalizer<4,>=2`

## Description

# Requests

Use it.
";
        assert_eq!(pypi_markdown(&pypi).as_deref(), Some(expected));

        let package = json!({
            "name": "ureq", "description": "Simple, safe HTTP client", "documentation": null,
            "repository": "https://github.com/algesten/ureq", "homepage": null, "downloads": 201802988,
        });
        let expected = "\
# ureq 3.4.2

Simple, safe HTTP client

```toml
ureq = \"3.4.2\"
```

- Documentation: https://docs.rs/ureq/3.4.2
- Repository: https://github.com/algesten/ureq
- Downloads: 201802988

## README

# ureq

A client.
";
        assert_eq!(
            crate_markdown(&package, "3.4.2", Some("# ureq\n\nA client.")).as_deref(),
            Some(expected)
        );
        assert_eq!(npm_markdown(&json!({ "name": "no-version" })), None);
    }

    #[test]
    fn questions_read_with_their_best_answers() {
        let question = json!({
            "title": "Why is &quot;sorted&quot; faster?", "score": 27000, "answer_count": 2,
            "tags": ["java", "performance"], "owner": { "display_name": "GMan" },
            "body": "<p>Here is <code>code</code> and <a href=\"/q/1\">a link</a>.</p>",
        });
        let answers = json!([
            { "score": 1, "is_accepted": true, "owner": { "display_name": "Mysticial" }, "body": "<p>Branch prediction.</p><pre><code>if (x)</code></pre>" },
            { "score": -2, "is_accepted": false, "body": "<p>Luck.</p>" },
        ]);
        let site = Url::parse("https://stackoverflow.com/").expect("site");
        let expected = "\
# Why is \"sorted\" faster?

27000 votes · 2 answers · asked by GMan · tags: java, performance

Here is `code` and [a link](https://stackoverflow.com/q/1).

## Answer by Mysticial · 1 vote · accepted

Branch prediction.

```
if (x)
```

## Answer by a deleted user · -2 votes

Luck.
";
        assert_eq!(question_markdown(&question, &answers, &site).as_deref(), Some(expected));
    }
}
