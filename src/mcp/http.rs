//! Servers agt POSTs to over Streamable HTTP. In the modern era each request
//! stands alone, with its metadata mirrored into headers. In the legacy era
//! `initialize` may begin a session that `Mcp-Session-Id` carries on.

use std::io::{BufRead, BufReader, Read};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::connection::{Era, Error, MODERN, Wait, answer, outcome};
use super::{NEVER, lock};

/// Bytes of a failed response's body read.
const ERROR_BODY_LIMIT: u64 = 64 * 1024;

/// A server's endpoint.
pub(super) struct Endpoint {
    http: ureq::Agent,
    url: String,
    headers: Vec<(String, String)>,
    next_id: AtomicU64,
    legacy: Mutex<Legacy>,
}

/// What a legacy session sends with each request after `initialize`.
#[derive(Default)]
struct Legacy {
    version: Option<String>,
    session: Option<String>,
}

impl Endpoint {
    pub(super) fn new(url: &str, headers: &[(String, String)]) -> Self {
        let http = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // A server may close a connection it has answered without saying
            // so, and a message sent on it then is lost: sending it again could
            // call a tool twice. So no connection is kept for another message.
            .max_idle_connections(0)
            .timeout_connect(Some(Duration::from_secs(30)))
            .user_agent(concat!("agt/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        Self {
            http,
            url: url.to_owned(),
            headers: headers.to_vec(),
            next_id: AtomicU64::new(1),
            legacy: Mutex::default(),
        }
    }

    /// Starts sending a legacy session's headers, negotiated as `version`.
    pub(super) fn begin(&self, version: &str) {
        lock(&self.legacy).version = Some(version.to_owned());
    }

    pub(super) fn request(
        &self,
        method: &str,
        params: Value,
        headers: &[(String, String)],
        era: Era,
        wait: Wait<'_>,
    ) -> Result<Value, Error> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.post(&message, Some(method), headers, era, wait)?.ok_or_else(|| {
            Error::Failed(format!("the server accepted {method} without answering it"))
        })
    }

    /// Sends a legacy notification; the modern era defines none over HTTP.
    pub(super) fn notify(&self, method: &str) -> Result<(), Error> {
        let wait = Wait { until: None, cancel: &NEVER };
        self.post(&json!({ "jsonrpc": "2.0", "method": method }), None, &[], Era::Legacy, wait)
            .map(drop)
    }

    /// POSTs one message and returns the response to it, if it is a request.
    fn post(
        &self,
        message: &Value,
        method: Option<&str>,
        extra: &[(String, String)],
        era: Era,
        wait: Wait<'_>,
    ) -> Result<Option<Value>, Error> {
        let mut request = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        let session = match era {
            Era::Modern => {
                request = request.header("mcp-protocol-version", MODERN);
                if let Some(method) = method {
                    request = request.header("mcp-method", method);
                }
                None
            }
            Era::Legacy => {
                let legacy = lock(&self.legacy);
                if let Some(version) = &legacy.version {
                    request = request.header("mcp-protocol-version", version);
                }
                if let Some(session) = &legacy.session {
                    request = request.header("mcp-session-id", session);
                }
                legacy.session.clone()
            }
        };
        for (name, value) in extra {
            request = request.header(name, value);
        }
        if let Some(until) = wait.until {
            let left = until.saturating_duration_since(Instant::now());
            request =
                request.config().timeout_global(Some(left.max(Duration::from_millis(1)))).build();
        }
        let body = serde_json::to_vec(message).expect("JSON values always serialize");
        let response = request.send(&body[..]).map_err(|error| match error {
            ureq::Error::Timeout(_) => Error::TimedOut,
            error => Error::Failed(format!("cannot reach {}: {error}", self.url)),
        })?;
        if wait.cancelled() {
            return Err(Error::Cancelled);
        }
        let status = response.status().as_u16();
        let header = |name: &str| {
            response.headers().get(name).and_then(|value| value.to_str().ok()).map(str::to_owned)
        };
        if method == Some("initialize")
            && let Some(id) = header("mcp-session-id")
        {
            lock(&self.legacy).session = Some(id);
        }
        let events =
            header("content-type").is_some_and(|kind| kind.starts_with("text/event-stream"));
        let mut reader = response.into_body().into_reader();
        if status == 202 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            if status == 404 && session.is_some() {
                lock(&self.legacy).session = None;
                return Err(Error::Expired);
            }
            let mut bytes = Vec::new();
            let _ = (&mut reader).take(ERROR_BODY_LIMIT).read_to_end(&mut bytes);
            let body = String::from_utf8_lossy(&bytes).trim().to_owned();
            return Err(match serde_json::from_str::<Value>(&body) {
                Ok(message) if message.get("error").is_some() => {
                    outcome(&message).err().unwrap_or(Error::Status(status, body))
                }
                _ => Error::Status(status, body.chars().take(300).collect()),
            });
        }
        let id = message.get("id");
        if id.is_none() {
            return Ok(None);
        }
        if events {
            return self.events(BufReader::new(reader), message, wait).map(Some);
        }
        let mut text = String::new();
        reader
            .read_to_string(&mut text)
            .map_err(|error| Error::Failed(format!("cannot read the response: {error}")))?;
        let reply: Value = serde_json::from_str(&text)
            .map_err(|error| Error::Failed(format!("unreadable response: {error}")))?;
        // Servers of 2025-03-26 could answer with a batch.
        let reply = match reply {
            Value::Array(replies) => replies.into_iter().find(|reply| reply.get("id") == id),
            reply => Some(reply),
        };
        let reply =
            reply.ok_or_else(|| Error::Failed("the response did not answer the request".into()))?;
        outcome(&reply).map(Some)
    }

    /// Reads a response stream until it carries the response to `request`.
    /// A legacy server's own requests on it are answered; notifications are
    /// dropped. Returning drops the stream, which cancels a request whose
    /// caller gave up.
    fn events(
        &self,
        mut reader: impl BufRead,
        request: &Value,
        wait: Wait<'_>,
    ) -> Result<Value, Error> {
        let mut line = String::new();
        let mut data = String::new();
        loop {
            line.clear();
            let read = reader
                .read_line(&mut line)
                .map_err(|error| Error::Failed(format!("the response stream broke: {error}")))?;
            if wait.cancelled() {
                return Err(Error::Cancelled);
            }
            let field = line.trim_end_matches(['\n', '\r']);
            if read == 0 || field.is_empty() {
                if !data.is_empty() {
                    let event = std::mem::take(&mut data);
                    if let Some(reply) = self.event(&event, request) {
                        return reply;
                    }
                }
                if read == 0 {
                    return Err(Error::Failed(
                        "the response stream ended without a response".into(),
                    ));
                }
            } else if let Some(value) = field.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
            }
        }
    }

    fn event(&self, data: &str, request: &Value) -> Option<Result<Value, Error>> {
        let message: Value = serde_json::from_str(data).ok()?;
        match (message.get("id"), message["method"].as_str()) {
            (Some(id), None) if Some(id) == request.get("id") => Some(outcome(&message)),
            (Some(id), Some(method)) => {
                let wait = Wait { until: None, cancel: &NEVER };
                let _ = self.post(&answer(id, method), None, &[], Era::Legacy, wait);
                None
            }
            _ => None,
        }
    }
}

impl Drop for Endpoint {
    /// Ends a legacy session, as the protocol asks, without waiting on it.
    fn drop(&mut self) {
        let Some(session) = lock(&self.legacy).session.take() else { return };
        let (http, url, headers) = (self.http.clone(), self.url.clone(), self.headers.clone());
        let _ = thread::Builder::new().name("agt-mcp-end".into()).spawn(move || {
            let mut request = http.delete(&url).header("mcp-session-id", &session);
            for (name, value) in &headers {
                request = request.header(name, value);
            }
            let _ = request.config().timeout_global(Some(Duration::from_secs(5))).build().call();
        });
    }
}
