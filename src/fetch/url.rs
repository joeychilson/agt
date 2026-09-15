//! Web addresses: the http and https URLs agents write, and the links in pages
//! resolved against them, as RFC 3986 describes.

use std::fmt;

/// An absolute http or https URL to request. It keeps no fragment, which is
/// never sent, and characters a request cannot carry are percent-encoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Url {
    scheme: &'static str,
    /// The host in lowercase, with any user information and port.
    authority: String,
    /// Starts with `/`.
    path: String,
    query: Option<String>,
}

impl Url {
    /// Reads an absolute http or https URL.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let parts = Parts::split(text.trim());
        let scheme = parts
            .scheme
            .and_then(web_scheme)
            .ok_or_else(|| format!("{text:?} is not an http:// or https:// URL"))?;
        match parts.authority {
            Some(authority) if !authority.is_empty() => {
                Ok(Self::new(scheme, authority, parts.path, parts.query))
            }
            _ => Err(format!("{text:?} names no host")),
        }
    }

    /// The URL `reference`, a link as a page holds it, names from this one, or
    /// `None` when it names no http or https URL.
    pub(crate) fn join(&self, reference: &str) -> Option<Self> {
        // Browsers ignore the spaces around a link and any tabs and newlines in it.
        let reference: String =
            reference.trim().chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
        let parts = Parts::split(&reference);
        if let Some(scheme) = parts.scheme {
            let authority = parts.authority.filter(|authority| !authority.is_empty())?;
            return Some(Self::new(web_scheme(scheme)?, authority, parts.path, parts.query));
        }
        if let Some(authority) = parts.authority {
            return (!authority.is_empty())
                .then(|| Self::new(self.scheme, authority, parts.path, parts.query));
        }
        if parts.path.is_empty() {
            let query = parts.query.map(encode).or_else(|| self.query.clone());
            return Some(Self { query, ..self.clone() });
        }
        let path = if parts.path.starts_with('/') {
            parts.path.to_owned()
        } else {
            // A relative path replaces the last segment of this one.
            let directory = &self.path[..=self.path.rfind('/').unwrap_or(0)];
            format!("{directory}{}", parts.path)
        };
        Some(Self::new(self.scheme, &self.authority, &path, parts.query))
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    /// The host in lowercase, without user information or port.
    pub(crate) fn host(&self) -> &str {
        let host =
            self.authority.rsplit_once('@').map_or(self.authority.as_str(), |(_, host)| host);
        match host.find(']') {
            Some(end) if host.starts_with('[') => &host[..=end],
            _ => host.rsplit_once(':').map_or(host, |(name, _)| name),
        }
    }

    /// The path's segments that are not empty, as the URL writes them.
    pub(crate) fn segments(&self) -> Vec<&str> {
        self.path.split('/').filter(|segment| !segment.is_empty()).collect()
    }

    fn new(scheme: &'static str, authority: &str, path: &str, query: Option<&str>) -> Self {
        let authority = encode(authority);
        let host = authority.rfind('@').map_or(0, |at| at + 1);
        let authority = format!("{}{}", &authority[..host], authority[host..].to_ascii_lowercase());
        let path = remove_dot_segments(&encode(path));
        let path = if path.starts_with('/') { path } else { format!("/{path}") };
        Self { scheme, authority, path, query: query.map(encode) }
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}{}", self.scheme, self.authority, self.path)?;
        match &self.query {
            Some(query) => write!(f, "?{query}"),
            None => Ok(()),
        }
    }
}

/// A reference's components as RFC 3986's appendix B splits them, without its
/// fragment.
struct Parts<'a> {
    scheme: Option<&'a str>,
    authority: Option<&'a str>,
    path: &'a str,
    query: Option<&'a str>,
}

impl<'a> Parts<'a> {
    fn split(reference: &'a str) -> Self {
        let reference = reference.split_once('#').map_or(reference, |(before, _)| before);
        let (scheme, rest) = match reference.find([':', '/', '?']) {
            Some(end) if reference[end..].starts_with(':') && is_scheme(&reference[..end]) => {
                (Some(&reference[..end]), &reference[end + 1..])
            }
            _ => (None, reference),
        };
        let (authority, rest) = match rest.strip_prefix("//") {
            Some(rest) => {
                let end = rest.find(['/', '?']).unwrap_or(rest.len());
                (Some(&rest[..end]), &rest[end..])
            }
            None => (None, rest),
        };
        let (path, query) =
            rest.split_once('?').map_or((rest, None), |(path, query)| (path, Some(query)));
        Self { scheme, authority, path, query }
    }
}

/// Whether `text` is a scheme: a letter, then letters, digits, `+`, `-` and `.`.
fn is_scheme(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_alphabetic())
        && text.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn web_scheme(scheme: &str) -> Option<&'static str> {
    match scheme.to_ascii_lowercase().as_str() {
        "http" => Some("http"),
        "https" => Some("https"),
        _ => None,
    }
}

/// `text` with each character a URL cannot hold percent-encoded as UTF-8,
/// including a `%` that begins no escape.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (at, c) in text.char_indices() {
        let escape = c == '%'
            && text
                .as_bytes()
                .get(at + 1..at + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit));
        let allowed = c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=".contains(c);
        if escape || allowed {
            out.push(c);
        } else {
            let mut buffer = [0; 4];
            for byte in c.encode_utf8(&mut buffer).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    out
}

/// `path` without its `.` and `..` segments, as RFC 3986 5.2.4 removes them.
fn remove_dot_segments(path: &str) -> String {
    let mut input = path;
    let mut output = String::with_capacity(path.len());
    let pop = |output: &mut String| output.truncate(output.rfind('/').unwrap_or(0));
    while !input.is_empty() {
        if let Some(rest) = input.strip_prefix("../").or_else(|| input.strip_prefix("./")) {
            input = rest;
        } else if input.starts_with("/./") {
            input = &input[2..];
        } else if input == "/." {
            input = "/";
        } else if input.starts_with("/../") {
            input = &input[3..];
            pop(&mut output);
        } else if input == "/.." {
            input = "/";
            pop(&mut output);
        } else if input == "." || input == ".." {
            input = "";
        } else {
            let start = usize::from(input.starts_with('/'));
            let end = input[start..].find('/').map_or(input.len(), |end| end + start);
            output.push_str(&input[..end]);
            input = &input[end..];
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_resolve_as_rfc_3986_resolves_its_examples() {
        let base = Url::parse("http://a/b/c/d;p?q").expect("base");
        // RFC 3986 5.4.1 and 5.4.2, without fragments, which a Url never
        // keeps, and with an empty path written as `/`.
        for (reference, expected) in [
            ("g:h", None),
            ("g", Some("http://a/b/c/g")),
            ("./g", Some("http://a/b/c/g")),
            ("g/", Some("http://a/b/c/g/")),
            ("/g", Some("http://a/g")),
            ("//g", Some("http://g/")),
            ("?y", Some("http://a/b/c/d;p?y")),
            ("g?y", Some("http://a/b/c/g?y")),
            ("#s", Some("http://a/b/c/d;p?q")),
            ("g#s", Some("http://a/b/c/g")),
            ("g?y#s", Some("http://a/b/c/g?y")),
            (";x", Some("http://a/b/c/;x")),
            ("g;x", Some("http://a/b/c/g;x")),
            ("g;x?y#s", Some("http://a/b/c/g;x?y")),
            ("", Some("http://a/b/c/d;p?q")),
            (".", Some("http://a/b/c/")),
            ("./", Some("http://a/b/c/")),
            ("..", Some("http://a/b/")),
            ("../", Some("http://a/b/")),
            ("../g", Some("http://a/b/g")),
            ("../..", Some("http://a/")),
            ("../../", Some("http://a/")),
            ("../../g", Some("http://a/g")),
            ("../../../g", Some("http://a/g")),
            ("../../../../g", Some("http://a/g")),
            ("/./g", Some("http://a/g")),
            ("/../g", Some("http://a/g")),
            ("g.", Some("http://a/b/c/g.")),
            (".g", Some("http://a/b/c/.g")),
            ("g..", Some("http://a/b/c/g..")),
            ("..g", Some("http://a/b/c/..g")),
            ("./../g", Some("http://a/b/g")),
            ("./g/.", Some("http://a/b/c/g/")),
            ("g/./h", Some("http://a/b/c/g/h")),
            ("g/../h", Some("http://a/b/c/h")),
            ("g;x=1/./y", Some("http://a/b/c/g;x=1/y")),
            ("g;x=1/../y", Some("http://a/b/c/y")),
            ("g?y/./x", Some("http://a/b/c/g?y/./x")),
            ("g?y/../x", Some("http://a/b/c/g?y/../x")),
            ("g#s/./x", Some("http://a/b/c/g")),
            ("g#s/../x", Some("http://a/b/c/g")),
            // A strict parser reads a scheme, and http needs a host.
            ("http:g", None),
        ] {
            let joined = base.join(reference).map(|url| url.to_string());
            assert_eq!(joined.as_deref(), expected, "{reference:?}");
        }
    }

    #[test]
    fn links_are_read_as_browsers_read_them() {
        let page = Url::parse("https://example.com/docs/guide").expect("page");
        for (reference, expected) in [
            (" /api\n/v2 ", Some("https://example.com/api/v2")),
            ("HTTPS://Other.Example/Path", Some("https://other.example/Path")),
            ("//cdn.example/a b.png", Some("https://cdn.example/a%20b.png")),
            ("mailto:someone@example.com", None),
            ("javascript:void(0)", None),
        ] {
            let joined = page.join(reference).map(|url| url.to_string());
            assert_eq!(joined.as_deref(), expected, "{reference:?}");
        }
    }

    #[test]
    fn urls_are_read_as_agents_write_them() {
        for (text, expected) in [
            ("HTTPS://Docs.RS/ureq", Ok("https://docs.rs/ureq")),
            ("https://example.com", Ok("https://example.com/")),
            ("  https://example.com/a b?q=ü#top ", Ok("https://example.com/a%20b?q=%C3%BC")),
            ("https://example.com/100%/%41", Ok("https://example.com/100%25/%41")),
            (
                "https://en.wikipedia.org/wiki/Rust_(programming_language)",
                Ok("https://en.wikipedia.org/wiki/Rust_(programming_language)"),
            ),
            ("http://User@Example.com:8080/x/../y", Ok("http://User@example.com:8080/y")),
            (
                "ftp://example.com/file",
                Err(r#""ftp://example.com/file" is not an http:// or https:// URL"#),
            ),
            ("example.com/page", Err(r#""example.com/page" is not an http:// or https:// URL"#)),
            ("https:///path", Err(r#""https:///path" names no host"#)),
        ] {
            let url = Url::parse(text).map(|url| url.to_string());
            assert_eq!(url, expected.map(str::to_owned).map_err(str::to_owned), "{text:?}");
        }
    }
}
