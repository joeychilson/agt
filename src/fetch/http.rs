//! Requests for pages: what agt asks sites for and what it takes back.

use std::io::Read;
use std::time::Duration;

use flate2::read::GzDecoder;
use ureq::ResponseExt;

use super::{Error, Url};

/// The media types a page is asked for in: Markdown first, which many
/// documentation sites answer with, then HTML and text, then anything.
pub(super) const MARKDOWN_FIRST: &str =
    "text/markdown, text/html;q=0.9, text/plain;q=0.8, */*;q=0.1";
/// The most bytes of a response read, once decompressed.
pub(super) const MAX_BODY: u64 = 32 * 1024 * 1024;
/// How long a request may take, with its redirects and body.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A successful response.
pub(super) struct Response {
    /// The URL that answered, after redirects.
    pub(super) url: Url,
    /// The media type in lowercase, without parameters; empty when none was sent.
    pub(super) media: String,
    pub(super) body: Vec<u8>,
}

pub(super) struct Client {
    http: ureq::Agent,
}

impl Client {
    pub(super) fn new() -> Self {
        let http = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .user_agent(concat!("agt/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        Self { http }
    }

    /// GETs `url` with `headers`, such as the media types it accepts.
    pub(super) fn get(&self, url: &Url, headers: &[(&str, &str)]) -> Result<Response, Error> {
        let unreachable =
            |url: &Url, reason: String| Error::Unreachable { url: url.clone(), reason };
        let mut request = self.http.get(url.to_string());
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        // agt's HTTP client decompresses nothing itself, so compression is asked
        // for only here, where it is undone.
        let response =
            request.header("accept-encoding", "gzip").call().map_err(|error| match error {
                ureq::Error::Timeout(_) => unreachable(
                    url,
                    format!("it did not answer within {} seconds", TIMEOUT.as_secs()),
                ),
                error => unreachable(url, error.to_string()),
            })?;
        let answered = Url::parse(&response.get_uri().to_string()).unwrap_or_else(|_| url.clone());
        let status = response.status();
        if !status.is_success() {
            let reason = status.canonical_reason().unwrap_or_default().to_owned();
            return Err(Error::Status { url: answered, status: status.as_u16(), reason });
        }
        let header = |name: &str| {
            let value = response.headers().get(name).and_then(|value| value.to_str().ok());
            value.unwrap_or_default().trim().to_ascii_lowercase()
        };
        let media = header("content-type").split(';').next().unwrap_or_default().trim().to_owned();
        let encoding = header("content-encoding");
        let reader = response.into_body().into_reader();
        let mut body = Vec::new();
        let read = match encoding.as_str() {
            "" | "identity" => reader.take(MAX_BODY + 1).read_to_end(&mut body),
            "gzip" | "x-gzip" => GzDecoder::new(reader).take(MAX_BODY + 1).read_to_end(&mut body),
            other => {
                let reason =
                    format!("it sent the page in {other} encoding, which agt does not read");
                return Err(unreachable(&answered, reason));
            }
        };
        read.map_err(|error| unreachable(&answered, error.to_string()))?;
        if body.len() as u64 > MAX_BODY {
            return Err(Error::TooLarge(answered));
        }
        Ok(Response { url: answered, media, body })
    }
}
