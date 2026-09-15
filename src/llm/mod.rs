//! Streaming client for the OpenAI Responses API.
//!
//! Requests are stateless (`store: false`, full input on every call), which
//! every supported provider accepts and which keeps the session log the only
//! state. Each provider gets exactly the fields and headers it documents for
//! caching and session affinity; `body` builds a request's bytes and `sse`
//! reads its stream.

mod body;
mod sse;

use std::cell::Cell;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::auth::{Auth, Unavailable};
use crate::item::Item;
use crate::models::Effort;
use crate::provider::{Dialect, Provider};

/// Attempts allowed while no request has ever succeeded. Failing fast then
/// surfaces configuration mistakes; once the endpoint is proven, transient
/// failures are retried indefinitely so long runs survive outages.
const UNPROVEN_ATTEMPTS: u32 = 4;
const MAX_DELAY: Duration = Duration::from_secs(60);
const MAX_RETRY_AFTER: f64 = 600.0;
const ERROR_BODY_LIMIT: u64 = 64 * 1024;
/// Phrases of throttling errors, which pass when retried later.
const THROTTLED: [&str; 6] =
    ["rate limit", "rate_limit", "too many requests", "throttl", "please wait", "per minute"];

/// One model request.
pub(crate) struct Request<'a> {
    pub(crate) model: &'a str,
    /// The reasoning effort, or `None` for the provider's default.
    pub(crate) reasoning: Option<Effort>,
    pub(crate) instructions: &'a str,
    pub(crate) input: &'a [Item],
    /// Reasoning items before this index of `input` are left out, and
    /// compaction items there are replaced by a note: they came from another
    /// model, or the provider rejected them.
    pub(crate) reasoning_from: usize,
    pub(crate) tools: &'a [Value],
    /// `none` asks for text only while keeping the tools, and so the cached
    /// prompt prefix, unchanged.
    pub(crate) tool_choice: Option<&'a str>,
    /// The session id, which keys the provider's prompt cache and session
    /// affinity.
    pub(crate) cache_key: &'a str,
    pub(crate) max_output_tokens: Option<u32>,
    /// The context size in tokens at which the provider compacts the
    /// conversation itself while it responds, for providers that do.
    pub(crate) compact_at: Option<u64>,
    /// The session directory, where the images the input shows are saved, or
    /// `None` to send the model a note in place of each.
    pub(crate) images: Option<&'a Path>,
}

/// Progress of a request, delivered from its worker thread.
#[derive(Debug)]
pub(crate) enum Event {
    /// The connection is alive but produced nothing to show.
    Alive,
    Text(String),
    /// Reasoning text, or an empty string when a reasoning item starts.
    Thinking(String),
    /// A transient failure; the request is sent again after `delay`.
    Retry {
        attempt: u32,
        delay: Duration,
        reason: String,
    },
    /// Text streamed so far is void because the attempt failed.
    Reset,
    Done(Response),
    Failed(Error),
}

/// A completed response.
#[derive(Debug)]
pub(crate) struct Response {
    pub(crate) output: Vec<Item>,
    pub(crate) usage: Option<Usage>,
    /// Why generation stopped early, such as `max_output_tokens`.
    pub(crate) incomplete: Option<String>,
}

/// Token counts a provider reported for one response.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Usage {
    /// All input tokens, including those read from and written to the cache.
    pub(crate) input: u64,
    pub(crate) output: u64,
    /// Input tokens read from the prompt cache.
    pub(crate) cached: u64,
    /// Input tokens written to the prompt cache, where reported.
    pub(crate) cache_write: u64,
    /// The dollar cost, where the provider reports it.
    pub(crate) cost: Option<f64>,
}

/// A request that cannot succeed as sent.
#[derive(Debug, PartialEq)]
pub(crate) enum Error {
    /// The input exceeds the model's context window.
    Overflow,
    /// The provider rejected replayed reasoning items.
    Reasoning,
    /// The provider rejected an image in the input.
    Image(String),
    /// A failure that retrying cannot fix, or retries were exhausted.
    Fatal(String),
}

/// A request in flight. Dropping it cancels the request, interrupting retry
/// delays and closing the connection at the next streamed line.
pub(crate) struct Stream {
    cancel: Arc<AtomicBool>,
    /// The worker, unless it could not start.
    thread: Option<Thread>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(thread) = &self.thread {
            thread.unpark();
        }
    }
}

/// Where requests go: a provider's endpoint and the credential they carry.
#[derive(Clone, PartialEq)]
pub(crate) struct Endpoint {
    pub(crate) provider: Provider,
    /// The provider's URL, or `AGT_BASE_URL`.
    pub(crate) url: String,
    pub(crate) auth: Auth,
}

/// Sends requests to one endpoint.
pub(crate) struct Client {
    http: ureq::Agent,
    provider: Provider,
    url: String,
    auth: Auth,
    /// Whether any request has succeeded.
    proven: Arc<AtomicBool>,
}

impl Client {
    pub(crate) fn new(endpoint: &Endpoint) -> Self {
        let http = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(30)))
            // No response legitimately takes an hour; the bound limits how long
            // a worker abandoned on a dead connection lingers.
            .timeout_recv_response(Some(Duration::from_secs(3600)))
            .timeout_recv_body(Some(Duration::from_secs(3600)))
            .user_agent(concat!("agt/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        Self {
            http,
            provider: endpoint.provider,
            url: format!("{}/responses", endpoint.url.trim_end_matches('/')),
            auth: endpoint.auth.clone(),
            proven: Arc::default(),
        }
    }

    /// Sends `request` on a worker thread that reports progress through `on`.
    pub(crate) fn stream(
        &self,
        request: &Request<'_>,
        on: impl Fn(Event) + Send + Sync + 'static,
    ) -> Stream {
        self.send(self.url.clone(), true, request, on)
    }

    /// Compacts `request`'s conversation with the provider's own compaction.
    /// The response's output is the context to continue with, used exactly
    /// as returned.
    pub(crate) fn compact(
        &self,
        request: &Request<'_>,
        on: impl Fn(Event) + Send + Sync + 'static,
    ) -> Stream {
        self.send(format!("{}/compact", self.url), false, request, on)
    }

    fn send(
        &self,
        url: String,
        streaming: bool,
        request: &Request<'_>,
        on: impl Fn(Event) + Send + Sync + 'static,
    ) -> Stream {
        let dialect = &self.provider.spec().dialect;
        let body = body::build(request, dialect, streaming);
        let cancel = Arc::new(AtomicBool::new(false));
        let call = Call {
            http: self.http.clone(),
            dialect,
            url,
            streaming,
            auth: self.auth.clone(),
            cache_key: request.cache_key.to_owned(),
            proven: Arc::clone(&self.proven),
            cancel: Arc::clone(&cancel),
        };
        let on = Arc::new(on);
        let worker = {
            let on = Arc::clone(&on);
            thread::Builder::new().name("agt-request".into()).spawn(move || call.run(&body, &*on))
        };
        let thread = match worker {
            Ok(worker) => Some(worker.thread().clone()),
            Err(error) => {
                on(Event::Failed(Error::Fatal(format!("cannot start the request: {error}"))));
                None
            }
        };
        Stream { cancel, thread }
    }
}

/// Everything a worker thread needs to send one request.
struct Call {
    http: ureq::Agent,
    dialect: &'static Dialect,
    url: String,
    /// Whether the response streams events rather than arriving whole.
    streaming: bool,
    auth: Auth,
    /// The session id, sent in the header that keeps the session on one backend.
    cache_key: String,
    proven: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

/// Why an attempt failed.
#[derive(Debug)]
enum Failure {
    Cancelled,
    Fatal(Error),
    /// The backend rejected a subscription's access token.
    Unauthorized(String),
    Retry {
        reason: String,
        after: Option<Duration>,
    },
}

impl Call {
    fn run(&self, body: &[u8], on: &dyn Fn(Event)) {
        let mut attempt = 0;
        let mut reauthorized = false;
        loop {
            let streamed = Cell::new(false);
            let forward = |event: Event| {
                if matches!(event, Event::Text(_) | Event::Thinking(_)) {
                    streamed.set(true);
                }
                on(event);
            };
            let result = self.attempt(body, &forward);
            // Text from an attempt that failed is void, whether the request is
            // retried or the agent recovers another way.
            if result.is_err() && streamed.get() {
                on(Event::Reset);
            }
            let (reason, after) = match result {
                Ok(response) => {
                    self.proven.store(true, Ordering::Relaxed);
                    return on(Event::Done(response));
                }
                Err(Failure::Cancelled) => return,
                Err(Failure::Fatal(error)) => return on(Event::Failed(error)),
                // The token was refreshed for the next attempt; a second
                // rejection is not fixed by refreshing again.
                Err(Failure::Unauthorized(_)) if !reauthorized => {
                    reauthorized = true;
                    continue;
                }
                Err(Failure::Unauthorized(reason)) => {
                    return on(Event::Failed(Error::Fatal(reason)));
                }
                Err(Failure::Retry { reason, after }) => (reason, after),
            };
            attempt += 1;
            if attempt >= UNPROVEN_ATTEMPTS && !self.proven.load(Ordering::Relaxed) {
                return on(Event::Failed(Error::Fatal(reason)));
            }
            let delay = after.unwrap_or_else(|| backoff(attempt));
            on(Event::Retry { attempt, delay, reason });
            let until = Instant::now() + delay;
            while !self.cancel.load(Ordering::Relaxed) {
                let now = Instant::now();
                if now >= until {
                    break;
                }
                thread::park_timeout(until - now);
            }
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
        }
    }

    fn attempt(&self, body: &[u8], on: &dyn Fn(Event)) -> Result<Response, Failure> {
        let accept = if self.streaming { "text/event-stream" } else { "application/json" };
        let mut request = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", accept);
        for &(name, value) in self.dialect.headers {
            request = request.header(name, value);
        }
        if let Some(name) = self.dialect.affinity {
            request = request.header(name, &self.cache_key);
        }
        let headers = self.auth.headers(&self.http).map_err(|error| match error {
            Unavailable::SignIn(message) => Failure::Fatal(Error::Fatal(message)),
            Unavailable::Transient(message) => retry(message),
        })?;
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send(body).map_err(|error| match error {
            ureq::Error::BadUri(_) | ureq::Error::Http(_) => {
                Failure::Fatal(Error::Fatal(error.to_string()))
            }
            error => retry(error.to_string()),
        })?;
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Failure::Cancelled);
        }
        let status = response.status().as_u16();
        let after = retry_after(response.headers());
        let json = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"));
        let mut reader = response.into_body().into_reader();
        if !(200..300).contains(&status) {
            let mut body = Vec::new();
            // The status alone still classifies the failure if the body is
            // unreadable, and the limit can cut a character in half.
            let _ = (&mut reader).take(ERROR_BODY_LIMIT).read_to_end(&mut body);
            let body = String::from_utf8_lossy(&body);
            if rejects_token(status, &body)
                && let Auth::Subscription(subscription) = &self.auth
            {
                subscription.reject();
                return Err(Failure::Unauthorized(format!(
                    "HTTP {status}: {}",
                    error_message(&body)
                )));
            }
            return Err(classify_status(status, &body, after));
        }
        // A compaction arrives whole, as do responses from servers that ignore
        // `stream: true`.
        if json || !self.streaming {
            let mut text = String::new();
            reader.read_to_string(&mut text).map_err(|error| retry(error.to_string()))?;
            let response: Value = serde_json::from_str(&text)
                .map_err(|error| retry(format!("invalid response: {error}")))?;
            return sse::finish(response, Vec::new());
        }
        sse::Parser::new(on).read(BufReader::new(reader), &self.cancel)
    }
}

fn retry(reason: String) -> Failure {
    Failure::Retry { reason, after: None }
}

/// Whether a failure rejects the access token: a 401, or the 403 xAI answers
/// for a token it cannot validate.
fn rejects_token(status: u16, body: &str) -> bool {
    status == 401 || (status == 403 && body.contains("unauthenticated"))
}

fn classify_status(status: u16, body: &str, after: Option<Duration>) -> Failure {
    let lower = body.to_ascii_lowercase();
    let reason = format!("HTTP {status}: {}", error_message(body));
    match status {
        400 | 413 | 422 | 429 if status == 413 || is_overflow(&lower) => {
            Failure::Fatal(Error::Overflow)
        }
        400 | 422 if rejects_reasoning(&lower) => Failure::Fatal(Error::Reasoning),
        400 | 422 if lower.contains("image") => Failure::Fatal(Error::Image(reason)),
        429 if lower.contains("insufficient_quota") => Failure::Fatal(Error::Fatal(reason)),
        408 | 409 | 425 | 429 | 500..=599 => Failure::Retry { reason, after },
        _ => Failure::Fatal(Error::Fatal(reason)),
    }
}

/// Classifies an error delivered inside a stream, where HTTP status is always 200.
fn classify_event(error: &Value) -> Failure {
    let raw = error.to_string().to_ascii_lowercase();
    let message = error["message"]
        .as_str()
        .or(error.as_str())
        .unwrap_or("the provider reported an error")
        .to_owned();
    if is_overflow(&raw) {
        return Failure::Fatal(Error::Overflow);
    }
    if rejects_reasoning(&raw) {
        return Failure::Fatal(Error::Reasoning);
    }
    if raw.contains("image") {
        return Failure::Fatal(Error::Image(message));
    }
    let transient = ["server_error", "overloaded", "timeout", "unavailable"];
    if THROTTLED.iter().chain(&transient).any(|code| raw.contains(code)) {
        retry(message)
    } else {
        Failure::Fatal(Error::Fatal(message))
    }
}

fn is_overflow(lower: &str) -> bool {
    // A request above a per-minute token limit can never pass. Other throttling
    // also mentions tokens ("too many tokens, please wait") but passes later.
    if lower.contains("request too large") {
        return true;
    }
    if THROTTLED.iter().any(|phrase| lower.contains(phrase)) {
        return false;
    }
    [
        "context_length_exceeded",
        "context length",
        "context window",
        "maximum context",
        "maximum prompt length",
        "prompt is too long",
        "input is too long",
        "too many tokens",
        "exceeds the context",
        "greater than the context",
        "reduce the length",
        "request_too_large",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
}

/// Whether a 400 rejects replayed reasoning: encrypted content the provider
/// cannot use, a reasoning item without the item that followed it, or a Gemini
/// thought signature made on another of Google's backends, where OpenRouter can
/// send any request.
fn rejects_reasoning(lower: &str) -> bool {
    lower.contains("encrypted")
        || lower.contains("thought signature")
        || (lower.contains("reasoning") && lower.contains("item"))
}

/// The message of an error body, shortened for display. OpenRouter's own
/// message is generic when the model's provider failed, so what that provider
/// said is shown instead.
fn error_message(body: &str) -> String {
    let parsed = serde_json::from_str::<Value>(body).unwrap_or_default();
    let error = &parsed["error"];
    if let Some(raw) = error["metadata"]["raw"].as_str().filter(|raw| !raw.trim().is_empty()) {
        let provider = error["metadata"]["provider_name"].as_str().unwrap_or("the provider");
        return format!("{provider}: {}", error_message(raw));
    }
    let message = error["message"].as_str().or(parsed["message"].as_str()).or(error.as_str());
    let message = message.unwrap_or(body).trim();
    match message.char_indices().nth(300) {
        Some((end, _)) => format!("{}…", &message[..end]),
        None => message.to_owned(),
    }
}

/// A client for short exchanges such as signing in and listing models, which
/// takes at most `timeout` and returns failed responses rather than errors.
pub(crate) fn http(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .new_agent()
}

/// Why a short exchange failed.
#[derive(Debug, PartialEq)]
pub(crate) struct ExchangeError {
    /// The response's status, when one arrived.
    pub(crate) status: Option<u16>,
    pub(crate) message: String,
}

/// The JSON body of a successful response, or why there is none.
pub(crate) fn json_body(
    response: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<Value, ExchangeError> {
    let failed = |status, message| ExchangeError { status, message };
    let response = response.map_err(|error| failed(None, error.to_string()))?;
    let status = response.status().as_u16();
    let text = response
        .into_body()
        .read_to_string()
        .map_err(|error| failed(Some(status), error.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(failed(Some(status), format!("HTTP {status}: {}", error_message(&text))));
    }
    serde_json::from_str(&text)
        .map_err(|error| failed(Some(status), format!("unreadable response: {error}")))
}

fn retry_after(headers: &ureq::http::HeaderMap) -> Option<Duration> {
    let seconds = |name: &str, scale: f64| {
        headers
            .get(name)?
            .to_str()
            .ok()?
            .trim()
            .parse::<f64>()
            .ok()
            // A zero delay would retry in a tight loop; backoff applies instead.
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| value / scale)
    };
    let delay = seconds("retry-after-ms", 1000.0).or_else(|| seconds("retry-after", 1.0))?;
    Some(Duration::from_secs_f64(delay.min(MAX_RETRY_AFTER)))
}

fn backoff(attempt: u32) -> Duration {
    let base = Duration::from_millis(500).saturating_mul(1 << attempt.min(8)).min(MAX_DELAY);
    // Jitter keeps concurrent sessions from retrying in lockstep. Microseconds
    // vary on every platform; macOS clocks have no finer resolution.
    let micros =
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.subsec_micros());
    base.mul_f64(0.75 + f64::from(micros % 1000) / 2000.0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn http_statuses_are_classified() {
        let fatal = |status, body: &str| match classify_status(status, body, None) {
            Failure::Fatal(error) => error,
            failure => panic!("HTTP {status} {body:?} is not fatal: {failure:?}"),
        };
        let reasoning = [
            "reasoning.encrypted_content is not supported",
            "Item 'rs_1' of type 'reasoning' was provided without its required following item.",
        ];
        for body in reasoning {
            assert_eq!(fatal(400, body), Error::Reasoning, "{body}");
        }
        let overflows = [
            (400, "This model's maximum context length is 128000 tokens"),
            (413, ""),
            (
                429,
                "Request too large for gpt-4o on tokens per min (TPM): Limit 30000, Requested 45000.",
            ),
        ];
        for (status, body) in overflows {
            assert_eq!(fatal(status, body), Error::Overflow, "HTTP {status} {body}");
        }
        let store = r#"{"error":{"message":"Unsupported parameter: 'store'","param":"store"}}"#;
        assert_eq!(
            fatal(400, store),
            Error::Fatal("HTTP 400: Unsupported parameter: 'store'".into())
        );
        assert_eq!(
            fatal(429, "insufficient_quota"),
            Error::Fatal("HTTP 429: insufficient_quota".into())
        );
        let key = r#"{"error":{"message":"bad key"}}"#;
        assert_eq!(fatal(401, key), Error::Fatal("HTTP 401: bad key".into()));
        let image = r#"{"error":{"message":"Invalid image data"}}"#;
        assert_eq!(fatal(400, image), Error::Image("HTTP 400: Invalid image data".into()));
        let throttled =
            classify_status(429, "too many tokens per minute", Some(Duration::from_secs(3)));
        let retried = matches!(throttled, Failure::Retry { after: Some(after), .. } if after == Duration::from_secs(3));
        assert!(retried, "{throttled:?}");
        // What Google said shows through OpenRouter's generic message, and it
        // is replayed reasoning that failed.
        let google = r#"{"error":{"message":"Provider returned error","code":400,"metadata":{"raw":"{\n  \"error\": {\n    \"code\": 400,\n    \"message\": \"Corrupted thought signature.\"\n  }\n}\n","provider_name":"Google AI Studio"}}}"#;
        assert_eq!(error_message(google), "Google AI Studio: Corrupted thought signature.");
        assert_eq!(fatal(400, google), Error::Reasoning);
    }

    #[test]
    fn stream_errors_are_classified() {
        let error = |code: &str, message: &str| json!({ "code": code, "message": message });
        let limited = classify_event(&error("rate_limit_exceeded", "slow down"));
        assert!(matches!(limited, Failure::Retry { .. }), "{limited:?}");
        let overflow = classify_event(&error("context_length_exceeded", "too long"));
        assert!(matches!(overflow, Failure::Fatal(Error::Overflow)), "{overflow:?}");
        let throttled = classify_event(&error(
            "throttled",
            "Too many tokens, please wait before trying again.",
        ));
        assert!(matches!(throttled, Failure::Retry { .. }), "{throttled:?}");
        let invalid = classify_event(&json!({ "message": "invalid model" }));
        let fatal =
            matches!(&invalid, Failure::Fatal(Error::Fatal(message)) if message == "invalid model");
        assert!(fatal, "{invalid:?}");
    }

    #[test]
    fn exchanges_give_json_or_why_they_failed() {
        let response = |status: u16, body: &str| {
            Ok(ureq::http::Response::builder()
                .status(status)
                .body(ureq::Body::builder().data(body))
                .expect("response"))
        };
        assert_eq!(json_body(response(200, r#"{"key":"k1"}"#)), Ok(json!({ "key": "k1" })));
        let refused = json_body(response(401, r#"{"error":{"message":"bad key"}}"#));
        let error = ExchangeError { status: Some(401), message: "HTTP 401: bad key".into() };
        assert_eq!(refused, Err(error));
        let truncated = json_body(response(200, r#"{"data":"#)).expect_err("unreadable");
        assert_eq!(truncated.status, Some(200));
        assert!(truncated.message.starts_with("unreadable response: "), "{}", truncated.message);
    }

    #[test]
    fn retry_after_prefers_milliseconds_and_is_capped() {
        let mut headers = ureq::http::HeaderMap::new();
        headers.insert("retry-after", "7".parse().expect("valid header"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(7)));
        headers.insert("retry-after-ms", "1500".parse().expect("valid header"));
        assert_eq!(retry_after(&headers), Some(Duration::from_millis(1500)));
        headers.remove("retry-after-ms");
        headers.insert("retry-after", "99999".parse().expect("valid header"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(600)));
        headers.insert("retry-after", "0".parse().expect("valid header"));
        assert_eq!(retry_after(&headers), None);
    }

    #[test]
    fn rejected_tokens_are_told_from_other_refusals() {
        assert!(rejects_token(401, ""));
        // As xAI answers an access token it cannot validate.
        let xai = r#"{"code":"unauthenticated:bad-credentials","error":"The OAuth2 access token could not be validated."}"#;
        assert!(rejects_token(403, xai));
        assert_eq!(error_message(xai), "The OAuth2 access token could not be validated.");
        assert!(!rejects_token(400, xai));
        assert!(!rejects_token(403, r#"{"error":"forbidden"}"#));
    }

    #[test]
    fn base_urls_with_a_trailing_slash_reach_responses() {
        let endpoint = Endpoint {
            provider: Provider::OpenAi,
            url: "http://localhost/v1/".into(),
            auth: Auth::None,
        };
        assert_eq!(Client::new(&endpoint).url, "http://localhost/v1/responses");
    }
}
