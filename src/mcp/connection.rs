//! A connection to one server: the protocol era it speaks, its tools, and
//! requests to it.
//!
//! Modern servers (revision 2026-07-28) keep no session: every request
//! carries its protocol version and capabilities in `_meta`. Legacy servers
//! (2025-11-25 and earlier) are spoken to after an `initialize` handshake.
//! As the specification advises, a connection probes with `server/discover`
//! and falls back to `initialize` on any answer but a modern error.

use std::fmt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{Server, Transport, http, lock, schema, stdio};

/// The modern protocol version agt speaks.
pub(super) const MODERN: &str = "2026-07-28";
/// The legacy version agt asks for, and those it accepts in reply.
const LEGACY: &str = "2025-11-25";
const LEGACY_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Errors only modern servers send, which rule out falling back.
const MODERN_ERRORS: [i64; 3] = [-32020, -32021, -32022];
const UNSUPPORTED_VERSION: i64 = -32022;
const HEADER_MISMATCH: i64 = -32020;
/// How long a server may take to start and answer its first request.
const HANDSHAKE: Duration = Duration::from_secs(30);
/// Pages of tools read at most, against a server that never stops paging.
const MAX_PAGES: usize = 100;

/// The two eras of the protocol.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Era {
    Modern,
    Legacy,
}

/// Why a request failed.
#[derive(Debug, PartialEq)]
pub(super) enum Error {
    /// The server answered with a JSON-RPC error.
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// An HTTP status that carried no JSON-RPC error.
    Status(u16, String),
    /// A legacy HTTP server no longer knows the session.
    Expired,
    /// The server could not be reached, stopped, or broke the protocol.
    Failed(String),
    TimedOut,
    Cancelled,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc { code, message, .. } => write!(f, "{message} (error {code})"),
            Self::Status(status, body) if body.is_empty() => write!(f, "HTTP {status}"),
            Self::Status(status, body) => write!(f, "HTTP {status}: {body}"),
            Self::Expired => f.write_str("the server ended the session"),
            Self::Failed(reason) => f.write_str(reason),
            Self::TimedOut => f.write_str("the server did not answer in time"),
            Self::Cancelled => f.write_str("the call was cancelled"),
        }
    }
}

/// When a request gives up waiting.
#[derive(Clone, Copy)]
pub(super) struct Wait<'a> {
    pub(super) until: Option<Instant>,
    /// Set when the caller no longer wants the answer.
    pub(super) cancel: &'a AtomicBool,
}

impl Wait<'_> {
    pub(super) fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// The result of a JSON-RPC response: its `result`, or its error.
pub(super) fn outcome(message: &Value) -> Result<Value, Error> {
    match message.get("error") {
        Some(error) => Err(Error::Rpc {
            code: error["code"].as_i64().unwrap_or_default(),
            message: error["message"].as_str().unwrap_or("error").to_owned(),
            data: error.get("data").cloned(),
        }),
        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
    }
}

/// The answer to a request a legacy server sends agt, which offers no client
/// features: a ping gets its empty result, and anything else an error.
pub(super) fn answer(id: &Value, method: &str) -> Value {
    match method {
        "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        method => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("agt does not offer {method}") },
        }),
    }
}

/// A tool as `tools/list` describes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Tool {
    pub(super) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    pub(super) input_schema: Value,
}

/// What a server says about itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Info {
    pub(super) title: Option<String>,
    pub(super) description: Option<String>,
    /// Its guidance for models on using it.
    pub(super) instructions: Option<String>,
}

impl Info {
    fn read(implementation: &Value, instructions: &Value) -> Self {
        let text = |value: &Value| {
            value.as_str().map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned)
        };
        Self {
            title: text(&implementation["title"]).or_else(|| text(&implementation["name"])),
            description: text(&implementation["description"]),
            instructions: text(instructions),
        }
    }

    /// What the server is for, in a paragraph: its description, or the first
    /// paragraph of its instructions.
    pub(super) fn about(&self) -> Option<String> {
        let paragraph = |text: &String| text.split("\n\n").next().unwrap_or(text).trim().to_owned();
        self.description.as_ref().or(self.instructions.as_ref()).map(paragraph)
    }
}

/// How messages reach the server.
enum Channel {
    Stdio(stdio::Process),
    Http(http::Endpoint),
}

impl Channel {
    fn request(
        &self,
        method: &str,
        params: Value,
        headers: &[(String, String)],
        era: Era,
        wait: Wait<'_>,
    ) -> Result<Value, Error> {
        match self {
            Self::Stdio(process) => process.request(method, params, wait),
            Self::Http(endpoint) => endpoint.request(method, params, headers, era, wait),
        }
    }

    fn notify(&self, method: &str) -> Result<(), Error> {
        match self {
            Self::Stdio(process) => process.notify(method),
            Self::Http(endpoint) => endpoint.notify(method),
        }
    }
}

/// A server spoken to in the era it speaks.
pub(super) struct Connection {
    channel: Channel,
    pub(super) era: Era,
    pub(super) info: Info,
    /// The tools last listed, whose schemas calls check and mirror.
    tools: Mutex<Option<Vec<Tool>>>,
}

impl Connection {
    /// Starts or reaches `server` and learns the era it speaks, trying the
    /// era agt `remembered` first. A program runs in `cwd` unless its
    /// settings name another, and its standard error goes to `log`.
    pub(super) fn open(
        server: &Server,
        cwd: &Path,
        remembered: Option<Era>,
        log: Option<&Path>,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        let channel = match &server.transport {
            Transport::Stdio { command, args, env, cwd: dir } => Channel::Stdio(
                stdio::Process::spawn(command, args, env, dir.as_deref().unwrap_or(cwd), log)?,
            ),
            Transport::Http { url, headers } => Channel::Http(http::Endpoint::new(url, headers)),
        };
        let wait = Wait { until: Some(Instant::now() + HANDSHAKE), cancel };
        let (era, info) = match remembered {
            Some(Era::Legacy) => match initialize(&channel, wait) {
                Ok(info) => (Era::Legacy, info),
                // A server that did not answer would not answer a probe either.
                Err(error @ (Error::TimedOut | Error::Cancelled)) => return Err(error.to_string()),
                // One that refused the era, or a version of it, is probed again.
                Err(_) => negotiate(&channel, wait)?,
            },
            _ => negotiate(&channel, wait)?,
        };
        Ok(Self { channel, era, info, tools: Mutex::new(None) })
    }

    /// Whether the server can still take requests.
    pub(super) fn alive(&self) -> bool {
        match &self.channel {
            Channel::Stdio(process) => process.alive(),
            Channel::Http(_) => true,
        }
    }

    fn request(
        &self,
        method: &str,
        params: Map<String, Value>,
        headers: &[(String, String)],
        wait: Wait<'_>,
    ) -> Result<Value, Error> {
        let params = match self.era {
            Era::Modern => with_meta(params),
            Era::Legacy => Value::Object(params),
        };
        let result = match self.channel.request(method, params.clone(), headers, self.era, wait) {
            // A legacy HTTP server that forgot the session gets a new one.
            Err(Error::Expired) => {
                initialize(&self.channel, wait)?;
                self.channel.request(method, params, headers, self.era, wait)?
            }
            result => result?,
        };
        match result.get("resultType").and_then(Value::as_str) {
            None | Some("complete") => Ok(result),
            Some("input_required") => {
                let asked: Vec<&str> = result["inputRequests"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(_, request)| request["method"].as_str())
                    .collect();
                Err(Error::Failed(format!(
                    "the server needs input agt cannot give ({})",
                    asked.join(", ")
                )))
            }
            Some(other) => {
                Err(Error::Failed(format!("the server sent a result of unknown type {other:?}")))
            }
        }
    }

    /// Lists the server's tools, and remembers them for calls. On HTTP a tool
    /// whose header annotations are invalid is left out, with the reason.
    pub(super) fn list_tools(&self, wait: Wait<'_>) -> Result<(Vec<Tool>, Vec<String>), Error> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut params = Map::new();
            if let Some(cursor) = cursor.take() {
                params.insert("cursor".into(), cursor.into());
            }
            let result = self.request("tools/list", params, &[], wait)?;
            let listed = result["tools"].as_array().into_iter().flatten();
            tools.extend(listed.filter_map(|tool| Tool::deserialize(tool).ok()));
            match result["nextCursor"].as_str() {
                Some(next) if !next.is_empty() => cursor = Some(next.to_owned()),
                _ => break,
            }
        }
        let mut left_out = Vec::new();
        if matches!(self.channel, Channel::Http(_)) {
            tools.retain(|tool| match schema::header_params(&tool.input_schema) {
                Ok(_) => true,
                Err(reason) => {
                    left_out.push(format!("{}: {reason}", tool.name));
                    false
                }
            });
        }
        *lock(&self.tools) = Some(tools.clone());
        Ok((tools, left_out))
    }

    /// The tools last listed, listing them first if they never were.
    pub(super) fn tools(&self, wait: Wait<'_>) -> Result<Vec<Tool>, Error> {
        if let Some(tools) = lock(&self.tools).clone() {
            return Ok(tools);
        }
        self.list_tools(wait).map(|(tools, _)| tools)
    }

    /// Calls tool `name` with `arguments`, returning its result.
    pub(super) fn call_tool(
        &self,
        name: &str,
        arguments: &Value,
        wait: Wait<'_>,
    ) -> Result<Value, Error> {
        let mut params = Map::new();
        params.insert("name".into(), name.into());
        params.insert("arguments".into(), arguments.clone());
        let modern_http = self.era == Era::Modern && matches!(self.channel, Channel::Http(_));
        if !modern_http {
            return self.request("tools/call", params, &[], wait);
        }
        let headers = |tools: &[Tool]| {
            let mut headers = vec![("Mcp-Name".to_owned(), schema::header_value(name))];
            let tool = tools.iter().find(|tool| tool.name == name);
            if let Some(params) =
                tool.and_then(|tool| schema::header_params(&tool.input_schema).ok())
            {
                headers.extend(schema::param_headers(&params, arguments));
            }
            headers
        };
        match self.request("tools/call", params.clone(), &headers(&self.tools(wait)?), wait) {
            // The tool's annotations changed since they were listed.
            Err(Error::Rpc { code: HEADER_MISMATCH, .. }) => {
                let (tools, _) = self.list_tools(wait)?;
                self.request("tools/call", params, &headers(&tools), wait)
            }
            result => result,
        }
    }
}

/// The parameters of a modern request: `params` with the metadata every
/// request carries.
fn with_meta(mut params: Map<String, Value>) -> Value {
    params.insert(
        "_meta".into(),
        json!({
            "io.modelcontextprotocol/protocolVersion": MODERN,
            "io.modelcontextprotocol/clientInfo": client_info(),
            "io.modelcontextprotocol/clientCapabilities": {},
        }),
    );
    Value::Object(params)
}

fn client_info() -> Value {
    json!({ "name": "agt", "version": env!("CARGO_PKG_VERSION") })
}

/// Probes with `server/discover`: a result or a modern error means a modern
/// server, and anything else a legacy one, which is then initialized.
fn negotiate(channel: &Channel, wait: Wait<'_>) -> Result<(Era, Info), String> {
    let probe = channel.request("server/discover", with_meta(Map::new()), &[], Era::Modern, wait);
    match probe {
        Ok(result) => {
            let versions: Vec<&str> = result["supportedVersions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if !versions.contains(&MODERN) {
                return Err(unsupported(&versions));
            }
            let implementation = &result["_meta"]["io.modelcontextprotocol/serverInfo"];
            Ok((Era::Modern, Info::read(implementation, &result["instructions"])))
        }
        Err(Error::Rpc { code: UNSUPPORTED_VERSION, data, .. }) => {
            let supported = data.as_ref().and_then(|data| data["supported"].as_array());
            Err(unsupported(
                &supported.into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>(),
            ))
        }
        Err(Error::Rpc { code, message, .. }) if MODERN_ERRORS.contains(&code) => Err(message),
        Err(error @ (Error::Failed(_) | Error::Cancelled)) => Err(error.to_string()),
        // The handshake gets time of its own after a probe the server left
        // unanswered.
        Err(Error::Rpc { .. } | Error::Status(..) | Error::Expired | Error::TimedOut) => {
            let wait = Wait { until: Some(Instant::now() + HANDSHAKE), ..wait };
            initialize(channel, wait)
                .map(|info| (Era::Legacy, info))
                .map_err(|error| error.to_string())
        }
    }
}

fn unsupported(versions: &[&str]) -> String {
    format!(
        "the server speaks MCP {}, and agt speaks {MODERN} and {LEGACY} and earlier",
        versions.join(", ")
    )
}

/// The legacy handshake.
fn initialize(channel: &Channel, wait: Wait<'_>) -> Result<Info, Error> {
    let params =
        json!({ "protocolVersion": LEGACY, "capabilities": {}, "clientInfo": client_info() });
    let result = channel.request("initialize", params, &[], Era::Legacy, wait)?;
    let version = result["protocolVersion"].as_str().unwrap_or_default();
    if !LEGACY_VERSIONS.contains(&version) {
        return Err(Error::Failed(unsupported(&[version])));
    }
    if let Channel::Http(endpoint) = channel {
        endpoint.begin(version);
    }
    channel.notify("notifications/initialized")?;
    Ok(Info::read(&result["serverInfo"], &result["instructions"]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_give_their_result_or_error() {
        assert_eq!(outcome(&json!({ "id": 1, "result": { "a": 1 } })), Ok(json!({ "a": 1 })));
        assert_eq!(
            outcome(
                &json!({ "id": 1, "error": { "code": -32601, "message": "Method not found" } })
            ),
            Err(Error::Rpc { code: -32601, message: "Method not found".into(), data: None })
        );
        assert_eq!(answer(&json!(7), "ping"), json!({ "jsonrpc": "2.0", "id": 7, "result": {} }));
        assert_eq!(answer(&json!("x"), "sampling/createMessage")["error"]["code"], -32601);
    }

    #[test]
    fn servers_say_what_they_are_for() {
        let info = Info::read(
            &json!({ "name": "github-mcp-server", "title": "GitHub MCP Server" }),
            &json!("Tools for GitHub.\n\nUse list_* tools for broad retrieval."),
        );
        assert_eq!(info.title.as_deref(), Some("GitHub MCP Server"));
        assert_eq!(info.about().as_deref(), Some("Tools for GitHub."));
        let described =
            Info::read(&json!({ "name": "x", "description": "Browser automation." }), &Value::Null);
        assert_eq!(described.about().as_deref(), Some("Browser automation."));
        assert_eq!(Info::read(&Value::Null, &json!("  ")).about(), None);
    }
}
