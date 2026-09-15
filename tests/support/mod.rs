//! A scripted Responses API server and helpers for end-to-end tests.

// Every test binary compiles this module and uses the part of it it needs.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

/// Lines longer than this are shortened in goldens to their ends and a hash.
const GOLDEN_LINE: usize = 8192;

pub struct Server {
    pub url: String,
    /// Each model request: its method and path, headers and body.
    exchanges: Arc<Mutex<Vec<Value>>>,
}

impl Server {
    /// Answers the n-th request with the n-th list of streamed events, and any
    /// request beyond the script with a non-retryable error. A list holding
    /// one object with a numeric `status` answers with that status and the
    /// object's `error`, and a leading `{"delay_ms": N}` holds the response
    /// back, as a slow model would.
    pub fn start(script: Vec<Vec<Value>>) -> Self {
        Self::with_model_delay(script, Duration::ZERO)
    }

    /// Delays model discovery to check that frontends stay responsive.
    pub fn with_model_delay(script: Vec<Vec<Value>>, delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        let url = format!("http://{}/v1", listener.local_addr().expect("local address"));
        let exchanges = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&exchanges);
        thread::spawn(move || {
            let mut script = script.into_iter();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                    continue;
                }
                let mut length = 0;
                let mut headers = BTreeMap::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        let name = name.trim().to_ascii_lowercase();
                        match name.as_str() {
                            "content-length" => length = value.trim().parse().unwrap_or(0),
                            // The port changes with every run.
                            "host" => {}
                            _ => {
                                headers.insert(name, value.trim().to_owned());
                            }
                        }
                    }
                }
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).is_err() {
                    continue;
                }
                // Model requests are scripted and the model list is fixed;
                // anything else is not found.
                if request_line.starts_with("GET ") && request_line.contains("/models ") {
                    thread::sleep(delay);
                    // Shaped as OpenRouter lists models.
                    let body = json!({ "data": [
                        {
                            "id": "test-model",
                            "context_length": 64000,
                            "supported_parameters": ["reasoning", "tools"],
                            "reasoning": { "supported_efforts": ["high", "low"] },
                            "architecture": { "input_modalities": ["text", "image"] },
                            "pricing": { "prompt": "0.000001", "completion": "0.000002" },
                        },
                        {
                            "id": "other-model",
                            "context_length": 128000,
                            "supported_parameters": ["tools", "reasoning"],
                            "architecture": { "input_modalities": ["text"] },
                        },
                        { "id": "chat-only", "supported_parameters": ["tools"] },
                    ] })
                    .to_string();
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    );
                    continue;
                }
                if !request_line.starts_with("POST ") {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    );
                    continue;
                }
                let target: Vec<&str> = request_line.split(' ').take(2).collect();
                seen.lock().expect("exchanges lock").push(json!({
                    "request": target.join(" "),
                    "headers": headers,
                    "body": serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
                }));
                let compact = request_line.contains("/responses/compact ");
                let response = match script.next() {
                    Some(events) if events.len() == 1 && events[0]["status"].is_u64() => {
                        let body = json!({ "error": events[0]["error"] }).to_string();
                        format!(
                            "HTTP/1.1 {} Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            events[0]["status"],
                            body.len()
                        )
                    }
                    // Compaction answers with one JSON object, not events.
                    Some(events) if compact => {
                        let body = events.first().map(Value::to_string).unwrap_or_default();
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    Some(events) => {
                        if let Some(delay) =
                            events.first().and_then(|event| event["delay_ms"].as_u64())
                        {
                            thread::sleep(Duration::from_millis(delay));
                        }
                        events
                            .iter()
                            .filter(|event| event.get("delay_ms").is_none())
                            .fold(
                                String::from(
                                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                                ),
                                |mut out, event| {
                                    out.push_str(&format!("data: {event}\n\n"));
                                    out
                                },
                            )
                    }
                    None => {
                        let body = r#"{"error":{"message":"unscripted request"}}"#;
                        format!(
                            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self { url, exchanges }
    }

    /// The model requests received so far, each with its method and path,
    /// headers and body.
    pub fn requests(&self) -> Vec<Value> {
        self.exchanges.lock().expect("exchanges lock").clone()
    }

    /// The model requests received so far as pretty-printed JSON, for a golden.
    pub fn exchanges(&self) -> String {
        serde_json::to_string_pretty(&self.requests()).expect("JSON")
    }
}

/// A page's status, headers and body, as a site answers a request.
pub type Answer = (u16, Vec<(String, String)>, Vec<u8>);

/// The paths a site was asked for, each with the request's headers.
pub type Asked = Vec<(String, BTreeMap<String, String>)>;

/// A site for `agt fetch` tests. It answers every request with what `answer`
/// gives for the path and headers, and records what it was asked.
pub struct Site {
    pub url: String,
    asked: Arc<Mutex<Asked>>,
}

impl Site {
    pub fn start(
        answer: impl Fn(&str, &BTreeMap<String, String>) -> Answer + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        let url = format!("http://{}", listener.local_addr().expect("local address"));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&asked);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut request = String::new();
                if reader.read_line(&mut request).unwrap_or(0) == 0 {
                    continue;
                }
                let mut headers = BTreeMap::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
                    }
                }
                let path = request.split(' ').nth(1).unwrap_or("/").to_owned();
                let (status, mut sent, body) = answer(&path, &headers);
                seen.lock().expect("asked lock").push((path, headers));
                sent.push(("content-length".to_owned(), body.len().to_string()));
                sent.push(("connection".to_owned(), "close".to_owned()));
                let head: String =
                    sent.iter().map(|(name, value)| format!("{name}: {value}\r\n")).collect();
                let response = format!("HTTP/1.1 {status} Status\r\n{head}\r\n");
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        Self { url, asked }
    }

    /// The paths asked for so far, each with the request's headers.
    pub fn asked(&self) -> Asked {
        self.asked.lock().expect("asked lock").clone()
    }
}

/// A response that streams `text` and completes.
pub fn text(text: &str) -> Vec<Value> {
    vec![
        json!({ "type": "response.output_text.delta", "delta": text }),
        json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": text }],
                }],
                "usage": { "input_tokens": 100, "output_tokens": 10 },
            },
        }),
    ]
}

/// A response that calls tool `name` with `arguments`, reporting `input_tokens`
/// of usage.
pub fn call(call_id: &str, name: &str, arguments: Value, input_tokens: u64) -> Vec<Value> {
    vec![json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments.to_string(),
            }],
            "usage": { "input_tokens": input_tokens, "output_tokens": 20 },
        },
    })]
}

/// The agt binary configured against `server`, isolated in `home`.
pub fn agt(home: &Path, server: &Server) -> Command {
    // The working directory agt sees is canonical, and HOME must match it for
    // paths under it to show as `~`.
    let home = home.canonicalize().expect("canonical home");
    let home = home.as_path();
    let mut command = Command::new(env!("CARGO_BIN_EXE_agt"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("AGT_HOME", home.join(".agt"))
        .env("AGT_PROVIDER", "openrouter")
        .env("AGT_BASE_URL", &server.url)
        .env("AGT_API_KEY", "test-key")
        .env("AGT_MODEL", "test-model")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("AI_GATEWAY_API_KEY")
        .env_remove("AGT_CONTEXT_WINDOW")
        .env_remove("AGT_REASONING")
        // Tests may run in an agt session, whose commands reach its own.
        .env_remove("AGT_SESSION_DIR")
        .env_remove("AGT_SOCKET");
    command
}

/// Checks `actual` against `tests/support/golden/<name>` once what changes
/// between runs and machines is replaced, which pins agt's output byte for
/// byte. With `AGT_BLESS=1` the golden is written instead.
pub fn golden(name: &str, home: &Path, actual: &str) {
    let actual = normalize(actual, home);
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/golden").join(name);
    if std::env::var_os("AGT_BLESS").is_some() {
        fs::create_dir_all(path.parent().expect("golden directory")).expect("create goldens");
        fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("no golden {name}; run with AGT_BLESS=1 to write it"));
    if expected != actual {
        let (expected, actual): (Vec<&str>, Vec<&str>) =
            (expected.lines().collect(), actual.lines().collect());
        let line = expected
            .iter()
            .zip(&actual)
            .position(|(expected, actual)| expected != actual)
            .unwrap_or(expected.len().min(actual.len()));
        panic!(
            "{name} differs from its golden at line {}:\nexpected: {}\n  actual: {}\nrun with AGT_BLESS=1 if the change is intended",
            line + 1,
            expected.get(line).unwrap_or(&"<end>"),
            actual.get(line).unwrap_or(&"<end>"),
        );
    }
}

/// `text` with the temporary home, the platform, session ids, durations,
/// dates, timestamps and context estimates (which count the length of
/// temporary paths) replaced by placeholders, and very long lines shortened.
fn normalize(text: &str, home: &Path) -> String {
    let mut text = text.to_owned();
    // The canonical path contains the given one on macOS, so it goes first.
    let real = home.canonicalize().expect("canonical home");
    for path in [real.as_path(), home] {
        text = text.replace(path.to_str().expect("UTF-8 home"), "$HOME");
    }
    let platform = format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH);
    let text = text.replace(&platform, "$PLATFORM");
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(c) = rest.chars().next() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let starts = digits > 0
            && !out
                .chars()
                .next_back()
                .is_some_and(|last| last.is_ascii_alphanumeric() || last == '.');
        let after = rest.as_bytes().get(digits..).unwrap_or_default();
        let ends = |at: usize| !after.get(at).is_some_and(u8::is_ascii_alphanumeric);
        let placeholder = if !starts {
            None
        } else if digits == 10
            && after.first() == Some(&b'-')
            && after.len() >= 9
            && after[1..9].iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            && ends(9)
        {
            Some(("$SESSION", 19))
        } else if after.len() >= 3
            && after[0] == b'.'
            && after[1].is_ascii_digit()
            && after[2] == b's'
            && ends(3)
        {
            Some(("$TIME", digits + 3))
        } else if digits == 4 && is_date(after) {
            // `2026-09-15 14:02 UTC`, as agt writes times for the model.
            Some(("$DATE", digits + 16))
        } else if out.ends_with("\"created\":") || out.ends_with("\"created\": ") {
            Some(("$CREATED", digits))
        } else if out.ends_with("\"at\":") || out.ends_with("\"at\": ") {
            Some(("$AT", digits))
        } else if out.ends_with("\"used\": ") {
            Some(("$USED", digits))
        } else if out.ends_with("context from about ") || out.ends_with("$TOKENS to ") {
            Some(("$TOKENS", digits))
        } else {
            None
        };
        match placeholder {
            Some((placeholder, len)) => {
                out.push_str(placeholder);
                rest = &rest[len..];
            }
            None => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    let mut lines: Vec<String> = out.lines().map(shorten).collect();
    lines.push(String::new());
    lines.join("\n")
}

/// Whether `after`, which follows a year, is the rest of a date as agt
/// writes it: `-09-15 14:02 UTC`.
fn is_date(after: &[u8]) -> bool {
    let Some(rest) = after.get(..16) else {
        return false;
    };
    rest.iter().zip(b"-00-00 00:00 UTC").all(|(byte, shape)| match shape {
        b'0' => byte.is_ascii_digit(),
        shape => byte == shape,
    })
}

/// A long line as its ends, its length and a hash of the whole line.
fn shorten(line: &str) -> String {
    if line.len() <= GOLDEN_LINE {
        return line.to_owned();
    }
    let head = line.floor_char_boundary(512);
    let tail = line.ceil_char_boundary(line.len() - 512);
    let hash = line.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{}…[{} bytes, hash {hash:016x}]…{}", &line[..head], line.len(), &line[tail..])
}
