//! Agent Client Protocol, version 1, over stdio.
//!
//! Messages are newline-delimited JSON-RPC 2.0. Each ACP session is an
//! `Agent`: its updates become `session/update` notifications, and the end of
//! its turn answers the pending `session/prompt` request. agt runs commands in
//! its own terminals and has no approvals, so it never calls the client's file
//! system, terminal or permission methods. MCP servers the client offers join
//! the session's own, which the model reaches with `agt mcp`.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::agent::{self, Agent, Delivery, Message, Notify, Open, Origin, Stop, Update, Usage};
use crate::auth::Auth;
use crate::bash::{Outcome, ToolCall};
use crate::config::{Config, Settings};
use crate::models::{self, Effort, Model};
use crate::{image, item, mcp, store};

const PROTOCOL_VERSION: u64 = 1;

enum Input {
    Agent(usize, agent::Event),
    Line(Vec<u8>),
    /// The models the provider offers, or `None` when it cannot list them.
    Models(Option<Vec<Model>>),
    /// A prompt whose images were prepared on a thread.
    Prompt {
        /// The index of the session it was sent to.
        index: usize,
        /// The id of its `session/prompt` request.
        request: Value,
        message: Result<Message, RpcError>,
    },
    Closed,
}

#[derive(Debug)]
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn invalid_params(message: impl Into<String>) -> Self {
        Self { code: -32602, message: message.into() }
    }

    /// The error that sends a client to the advertised sign-in.
    fn auth_required() -> Self {
        Self {
            code: -32000,
            message: "Authentication required: sign in and choose a model with `agt login`".into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self { code: -32002, message: message.into() }
    }

    fn internal(message: impl ToString) -> Self {
        Self { code: -32603, message: message.to_string() }
    }
}

/// The parameters of methods that take only a session.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionParams {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewParams {
    cwd: PathBuf,
    #[serde(default)]
    mcp_servers: Vec<McpServerParams>,
}

/// An MCP server a client offers a session.
#[derive(Deserialize)]
struct McpServerParams {
    name: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Vec<NameValue>,
    url: Option<String>,
    #[serde(default)]
    headers: Vec<NameValue>,
}

/// An environment variable or header of an MCP server.
#[derive(Deserialize)]
struct NameValue {
    name: String,
    value: String,
}

impl McpServerParams {
    /// The server, or why agt cannot use it. Its name is made one the model
    /// can type in a command.
    fn server(&self) -> Result<mcp::Server, String> {
        let pairs = |list: &[NameValue]| {
            list.iter().map(|pair| (pair.name.clone(), pair.value.clone())).collect()
        };
        let transport = match (self.kind.as_deref(), &self.command, &self.url) {
            (None | Some("stdio"), Some(command), _) => mcp::Transport::Stdio {
                command: command.clone(),
                args: self.args.clone(),
                env: pairs(&self.env),
                cwd: None,
            },
            (Some("http"), _, Some(url)) => {
                mcp::Transport::Http { url: url.clone(), headers: pairs(&self.headers) }
            }
            (Some("sse"), ..) => {
                return Err(format!(
                    "MCP server {} uses SSE, which agt does not support",
                    self.name
                ));
            }
            _ => {
                return Err(format!(
                    "MCP server {} gives no command or address agt can use",
                    self.name
                ));
            }
        };
        let name: String = self
            .name
            .chars()
            .map(
                |c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' },
            )
            .collect();
        if !mcp::valid_name(&name) {
            return Err(format!("MCP server {:?} has no name agt can use", self.name));
        }
        Ok(mcp::Server { name, transport })
    }
}

/// The parameters of `session/load` and `session/resume`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadParams {
    session_id: String,
    #[serde(flatten)]
    new: NewParams,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptParams {
    session_id: String,
    prompt: Vec<Block>,
}

#[derive(Deserialize)]
struct ListParams {
    cwd: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigParams {
    session_id: String,
    config_id: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CancelRequestParams {
    request_id: Value,
}

/// A content block of a prompt.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text {
        text: String,
    },
    Image {
        /// The image file, base64 encoded.
        data: String,
    },
    ResourceLink {
        name: String,
        uri: String,
    },
    Resource {
        resource: Resource,
    },
}

/// A resource embedded in a prompt.
#[derive(Deserialize)]
struct Resource {
    uri: String,
    /// The contents of a text resource; `None` for a binary one.
    text: Option<String>,
}

/// Reads a method's parameters.
fn parse<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|error| RpcError::invalid_params(error.to_string()))
}

struct Session {
    agent: Agent,
    /// The id of the `session/prompt` request awaiting the end of the turn.
    prompt: Option<Value>,
    /// The turn's latest error, which answers the prompt if the turn fails.
    error: Option<String>,
    /// Notices raised outside a prompt, shown with the next one.
    notices: Vec<String>,
    /// Whether the client has been sent the session's title.
    titled: bool,
}

struct Server<'a> {
    config: &'a Config,
    sender: mpsc::Sender<Input>,
    /// Sessions by the index their events carry; closed sessions leave `None`.
    sessions: Vec<Option<Session>>,
    /// Models the provider offers, fetched when first needed: empty while the
    /// listing runs, and `None` again after it fails so it is retried.
    models: Option<Vec<Model>>,
}

/// Serves ACP over stdin and stdout until the client closes stdin.
pub(crate) fn run(config: &Config) -> io::Result<()> {
    let (sender, events) = mpsc::channel();
    let reader = sender.clone();
    thread::Builder::new().name("agt-acp-stdin".into()).spawn(move || {
        for line in io::stdin().lock().split(b'\n') {
            let Ok(line) = line else { break };
            if reader.send(Input::Line(line)).is_err() {
                return;
            }
        }
        let _ = reader.send(Input::Closed);
    })?;
    let mut server = Server { config, sender, sessions: Vec::new(), models: None };
    loop {
        let deadline =
            server.sessions.iter_mut().flatten().filter_map(|session| session.agent.poll()).min();
        for index in 0..server.sessions.len() {
            server.flush(index)?;
        }
        match crate::recv(&events, deadline) {
            Ok(Some(Input::Line(line))) => server.message(&line)?,
            Ok(Some(Input::Agent(index, event))) => {
                if let Some(Some(session)) = server.sessions.get_mut(index) {
                    session.agent.handle(event);
                }
            }
            Ok(Some(Input::Models(models))) => {
                server.models = models;
                let Some(models) = &server.models else {
                    continue;
                };
                for session in server.sessions.iter().flatten() {
                    send(&notification(
                        session.agent.session_id(),
                        json!({
                            "sessionUpdate": "config_option_update",
                            "configOptions": config_options(models, &session.agent),
                        }),
                    ))?;
                }
            }
            Ok(Some(Input::Prompt { index, request, message })) => {
                server.prepared(index, request, message)?
            }
            Ok(Some(Input::Closed)) | Err(_) => return Ok(()),
            Ok(None) => {}
        }
    }
}

impl Server<'_> {
    fn message(&mut self, line: &[u8]) -> io::Result<()> {
        if line.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let Ok(mut message) = serde_json::from_slice::<Value>(line) else {
            let error = RpcError { code: -32700, message: "parse error".into() };
            return respond(Value::Null, Err(error));
        };
        let valid_id = message
            .get("id")
            .is_none_or(|id| id.is_null() || id.is_string() || id.is_i64() || id.is_u64());
        if !message.is_object() || message["jsonrpc"] != "2.0" || !valid_id {
            let error = RpcError { code: -32600, message: "invalid JSON-RPC request".into() };
            return respond(Value::Null, Err(error));
        }
        let id = message.get("id").cloned();
        let params = match message.get_mut("params").map(Value::take) {
            None | Some(Value::Null) => json!({}),
            Some(params) => params,
        };
        // Responses to requests agt never sends carry no method.
        let Some(method) = message["method"].as_str() else {
            return Ok(());
        };
        // Only cancellations mean something as notifications.
        if id.is_none() && !matches!(method, "session/cancel" | "$/cancel_request") {
            return Ok(());
        }
        let mut opened = None;
        let result = match method {
            "initialize" => {
                // Sign-in runs in a terminal, for clients that can open one.
                let terminal = params.pointer("/clientCapabilities/auth/terminal");
                let methods = if terminal == Some(&Value::Bool(true)) {
                    json!([{
                        "id": "login",
                        "name": "Sign in",
                        "description": "Choose a provider, sign in or paste an API key, and choose a model",
                        "type": "terminal",
                        "args": ["login"],
                    }])
                } else {
                    json!([])
                };
                Ok(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "agentCapabilities": {
                        "loadSession": true,
                        "mcpCapabilities": { "http": true, "sse": false },
                        "promptCapabilities": { "image": true, "audio": false, "embeddedContext": true },
                        "sessionCapabilities": { "list": {}, "close": {}, "resume": {}, "delete": {} },
                    },
                    "agentInfo": { "name": "agt", "title": "agt", "version": env!("CARGO_PKG_VERSION") },
                    "authMethods": methods,
                }))
            }
            "authenticate" => {
                Err(RpcError::invalid_params("agt signs in from a terminal: run `agt login`"))
            }
            "session/new" | "session/load" | "session/resume" => {
                let session = if method == "session/new" {
                    parse(params).and_then(|params: NewParams| self.open(&params, None, false))
                } else {
                    parse(params).and_then(|params: LoadParams| {
                        self.open(&params.new, Some(&params.session_id), method == "session/load")
                    })
                };
                session.map(|(result, index, title)| {
                    opened = Some((index, title));
                    result
                })
            }
            "session/prompt" => {
                let request = id.clone().unwrap_or_default();
                match parse(params).and_then(|params| self.prompt(request, params)) {
                    Ok(()) => return Ok(()),
                    Err(error) => Err(error),
                }
            }
            "session/cancel" => {
                if let Ok(params) = parse::<SessionParams>(params)
                    && let Ok(index) = self.index(&params.session_id)
                {
                    self.cancel(index)?;
                }
                Ok(Value::Null)
            }
            "$/cancel_request" => {
                if let Ok(CancelRequestParams { request_id }) = parse(params) {
                    let pending = self.sessions.iter().position(|session| {
                        session
                            .as_ref()
                            .is_some_and(|session| session.prompt.as_ref() == Some(&request_id))
                    });
                    if let Some(index) = pending {
                        self.cancel(index)?;
                    }
                }
                Ok(Value::Null)
            }
            "session/close" => parse(params).and_then(|params: SessionParams| {
                self.close(&params.session_id).map(|()| json!({}))
            }),
            "session/delete" => {
                parse(params).and_then(|params: SessionParams| self.delete(&params.session_id))
            }
            "session/list" => {
                parse(params).and_then(|params: ListParams| self.list(params.cwd.as_deref()))
            }
            "session/set_config_option" => {
                parse(params).and_then(|params| self.set_config_option(params))
            }
            _ => Err(RpcError { code: -32601, message: format!("method not found: {method}") }),
        };
        let Some(id) = id else {
            return Ok(());
        };
        respond(id, result)?;
        match opened {
            Some((index, title)) => self.announce(index, title),
            None => Ok(()),
        }
    }

    /// The index of open session `id`.
    fn index(&self, id: &str) -> Result<usize, RpcError> {
        self.sessions
            .iter()
            .position(|session| {
                session.as_ref().is_some_and(|session| session.agent.session_id() == id)
            })
            .ok_or_else(|| RpcError::not_found("unknown sessionId"))
    }

    /// Creates a session, or opens session `resume`, replaying its history
    /// when `load`ing it. Returns the result, the session's index and, for a
    /// loaded session, its title.
    fn open(
        &mut self,
        params: &NewParams,
        resume: Option<&str>,
        load: bool,
    ) -> Result<(Value, usize, Option<String>), RpcError> {
        if self.config.model.is_none() || self.config.endpoint.auth == Auth::None {
            return Err(RpcError::auth_required());
        }
        let cwd = params.cwd.as_path();
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err(RpcError::invalid_params("cwd must be an existing absolute directory"));
        }
        if resume.is_some_and(|id| self.index(id).is_ok()) {
            return Err(RpcError::invalid_params("session is already open"));
        }
        let index = self.sessions.len();
        let notify: Notify = {
            let sender = self.sender.clone();
            Arc::new(move |event| {
                let _ = sender.send(Input::Agent(index, event));
            })
        };
        let mut replay_error = None;
        let mut title = None;
        let dir = resume.and_then(|id| store::session_dir(&self.config.home, id).ok());
        let mut replay = |update: Update| {
            if let Update::User { text, .. } = &update
                && title.is_none()
            {
                title = Some(store::title(text));
            }
            let (Some(id), Some(dir)) = (resume, &dir) else {
                return;
            };
            for payload in payloads(update, dir) {
                if let Err(error) = send(&notification(id, payload)) {
                    replay_error.get_or_insert(error);
                }
            }
        };
        let replay = load.then_some(&mut replay as &mut dyn FnMut(Update));
        let mut unusable = Vec::new();
        let servers: Vec<mcp::Server> = params
            .mcp_servers
            .iter()
            .filter_map(|server| server.server().map_err(|problem| unusable.push(problem)).ok())
            .collect();
        let open = Open { cwd, resume, replay, autowake: false, servers: &servers };
        let mut agent =
            Agent::open(self.config, open, notify).map_err(|error| match error.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::InvalidInput if resume.is_some() => {
                    RpcError::not_found(format!("cannot load session: {error}"))
                }
                _ => RpcError::internal(format!("cannot open session: {error}")),
            })?;
        if let Some(error) = replay_error {
            return Err(RpcError::internal(format!("cannot replay session: {error}")));
        }
        let mut notices = Vec::new();
        if !load {
            // Only a loaded session shows the calls a crash left open, since
            // only its client saw them start.
            for update in agent.drain() {
                if let Update::Notice(text) | Update::Error(text) = update {
                    notices.push(text);
                }
            }
        }
        notices.extend(unusable);
        let session =
            Session { agent, prompt: None, error: None, notices, titled: resume.is_some() };
        self.load_models();
        let options = config_options(self.models.as_deref().unwrap_or_default(), &session.agent);
        let id = session.agent.session_id().to_owned();
        self.sessions.push(Some(session));
        if load {
            // A loaded session is replayed in full before the response,
            // including the calls it closes because agt stopped during them.
            self.flush(index).map_err(RpcError::internal)?;
        }
        let result = match resume {
            Some(_) => json!({ "configOptions": options }),
            None => json!({ "sessionId": id, "configOptions": options }),
        };
        Ok((result, index, title))
    }

    /// Sends what a client needs once a session is open: its commands, its
    /// context usage and, for a loaded session, its title.
    fn announce(&self, index: usize, title: Option<String>) -> io::Result<()> {
        let Some(Some(session)) = self.sessions.get(index) else {
            return Ok(());
        };
        let agent = &session.agent;
        let id = agent.session_id();
        let mut commands = vec![json!({
            "name": "compact",
            "description": "Summarize the conversation to free context",
            "input": { "hint": "optional focus for the summary" },
        })];
        commands.extend(agent.skills().iter().map(|skill| {
            json!({
                "name": skill.name,
                "description": skill.description,
                "input": { "hint": "optional request" },
            })
        }));
        send(&notification(
            id,
            json!({ "sessionUpdate": "available_commands_update", "availableCommands": commands }),
        ))?;
        if let Some(title) = title {
            send(&notification(
                id,
                json!({ "sessionUpdate": "session_info_update", "title": title }),
            ))?;
        }
        send(&notification(id, usage_update(agent.usage())))
    }

    fn load_models(&mut self) {
        if self.models.is_none() {
            self.models = Some(Vec::new());
            let sender = self.sender.clone();
            let endpoint = &self.config.endpoint;
            let (provider, url) = (endpoint.provider, endpoint.url.clone());
            // Discovery must not hold up prompts, cancellations, or other sessions.
            let _ = thread::Builder::new().name("agt-acp-models".into()).spawn(move || {
                let _ = sender.send(Input::Models(models::list(provider, &url).ok()));
            });
        }
    }

    /// Starts the turn for `request`, which `session/prompt` sent.
    fn prompt(&mut self, request: Value, params: PromptParams) -> Result<(), RpcError> {
        let PromptParams { session_id, prompt: blocks } = params;
        let index = self.index(&session_id)?;
        let typed = blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        let Some(session) = &mut self.sessions[index] else {
            return Err(RpcError::not_found("unknown sessionId"));
        };
        if session.prompt.is_some() {
            return Err(RpcError::invalid_params("a prompt is already running in this session"));
        }
        let images = blocks.iter().any(|block| matches!(block, Block::Image { .. }));
        if images && !session.agent.sees_images() {
            return Err(RpcError::invalid_params(
                "the current model cannot see images; choose one that can",
            ));
        }
        let dir = session.agent.dir().to_path_buf();
        // A prompt without images is converted before anything is sent.
        let parts = if images { Vec::new() } else { content(&blocks, &dir)? };
        for notice in std::mem::take(&mut session.notices) {
            send(&notification(
                &session_id,
                chunk("agent_thought_chunk", format!("[agt] {notice}\n")),
            ))
            .map_err(RpcError::internal)?;
        }
        if !session.titled && !typed.trim().is_empty() {
            session.titled = true;
            send(&notification(
                &session_id,
                json!({ "sessionUpdate": "session_info_update", "title": store::title(&typed) }),
            ))
            .map_err(RpcError::internal)?;
        }
        session.prompt = Some(request.clone());
        session.error = None;
        match agent::compact_command(&typed) {
            Some(focus) => session.agent.compact(focus),
            // Decoding and scaling images would hold up every session, so they
            // are prepared on a thread and the prompt is submitted after.
            _ if images => {
                let sender = self.sender.clone();
                let spawned =
                    thread::Builder::new().name("agt-acp-images".into()).spawn(move || {
                        let message = content(&blocks, &dir).map(|content| Message {
                            content,
                            typed,
                            origin: Origin::User,
                        });
                        let _ = sender.send(Input::Prompt { index, request, message });
                    });
                if let Err(error) = spawned {
                    session.prompt = None;
                    return Err(RpcError::internal(format!("cannot prepare the images: {error}")));
                }
            }
            // One prompt runs at a time, so a prompt is never delivered later.
            None => {
                let message = Message { content: parts, typed, origin: Origin::User };
                session.agent.submit(message, Delivery::Next);
            }
        }
        Ok(())
    }

    /// Submits a prompt whose images are prepared, unless it was cancelled
    /// meanwhile.
    fn prepared(
        &mut self,
        index: usize,
        request: Value,
        message: Result<Message, RpcError>,
    ) -> io::Result<()> {
        let Some(Some(session)) = self.sessions.get_mut(index) else {
            return Ok(());
        };
        if session.prompt.as_ref() != Some(&request) {
            return Ok(());
        }
        match message {
            Ok(message) => {
                session.agent.submit(message, Delivery::Next);
                Ok(())
            }
            Err(error) => {
                session.prompt = None;
                respond(request, Err(error))
            }
        }
    }

    /// Cancels a session's turn. A prompt still preparing its images has no
    /// turn yet, so it is answered here.
    fn cancel(&mut self, index: usize) -> io::Result<()> {
        let Some(Some(session)) = self.sessions.get_mut(index) else {
            return Ok(());
        };
        if session.agent.busy() {
            session.agent.cancel();
        } else if let Some(request) = session.prompt.take() {
            return respond(request, Ok(json!({ "stopReason": "cancelled" })));
        }
        Ok(())
    }

    /// Cancels session `id`'s work, answers its pending prompt and frees it.
    fn close(&mut self, id: &str) -> Result<(), RpcError> {
        let index = self.index(id)?;
        self.cancel(index).map_err(RpcError::internal)?;
        self.flush(index).map_err(RpcError::internal)?;
        self.sessions[index] = None;
        Ok(())
    }

    /// Deletes session `id` and its files, closing it first when it is open.
    /// A session that does not exist counts as deleted.
    fn delete(&mut self, id: &str) -> Result<Value, RpcError> {
        if self.index(id).is_ok() {
            self.close(id)?;
        }
        store::delete(&self.config.home, id).map_err(|error| match error.kind() {
            io::ErrorKind::InvalidInput => RpcError::invalid_params(error.to_string()),
            _ => RpcError::internal(format!("cannot delete session: {error}")),
        })?;
        Ok(json!({}))
    }

    /// The saved sessions, only those in `cwd` when it is given.
    fn list(&self, cwd: Option<&Path>) -> Result<Value, RpcError> {
        let sessions: Vec<Value> = store::list(&self.config.home)
            .map_err(RpcError::internal)?
            .into_iter()
            .filter(|session| cwd.is_none_or(|cwd| session.cwd == cwd))
            .map(|session| {
                json!({
                    "sessionId": session.id,
                    "cwd": session.cwd.to_string_lossy(),
                    "title": (!session.title.is_empty()).then_some(session.title),
                })
            })
            .collect();
        Ok(json!({ "sessions": sessions }))
    }

    fn set_config_option(&mut self, params: ConfigParams) -> Result<Value, RpcError> {
        let index = self.index(&params.session_id)?;
        let value = params.value.as_str();
        self.load_models();
        let models = self.models.as_deref().unwrap_or_default();
        let Some(session) = &mut self.sessions[index] else {
            return Err(RpcError::not_found("unknown sessionId"));
        };
        let current = session.agent.settings();
        let settings = match params.config_id.as_str() {
            "model" => {
                let model = match models.iter().find(|model| model.id == value) {
                    Some(model) => model.clone(),
                    None if value == current.model.id => current.model.clone(),
                    None => {
                        return Err(RpcError::invalid_params(
                            "model must be one of the advertised options",
                        ));
                    }
                };
                Settings { model, ..current.clone() }
            }
            "reasoning" => {
                let reasoning = match value {
                    "default" => None,
                    name => {
                        let advertised = Effort::parse(name).filter(|effort| {
                            models::efforts(Some(&current.model)).contains(effort)
                                || current.reasoning == Some(*effort)
                        });
                        Some(advertised.ok_or_else(|| {
                            RpcError::invalid_params(
                                "reasoning must be one of the advertised options",
                            )
                        })?)
                    }
                };
                Settings { reasoning, ..current.clone() }
            }
            _ => return Err(RpcError::invalid_params("unknown configId")),
        };
        if !session.agent.configure(settings) {
            return Err(RpcError::invalid_params(
                "settings can change once the current turn finishes",
            ));
        }
        let options = config_options(models, &session.agent);
        Ok(json!({ "configOptions": options }))
    }

    /// Sends a session's pending updates, answering its prompt when the turn ends.
    fn flush(&mut self, index: usize) -> io::Result<()> {
        let Some(Some(session)) = self.sessions.get_mut(index) else {
            return Ok(());
        };
        let id = session.agent.session_id().to_owned();
        let dir = session.agent.dir().to_path_buf();
        let updates: Vec<Update> = session.agent.drain().collect();
        for update in updates {
            // A compaction starting and ending shows as the notice it logs.
            let update = match update {
                Update::Compacting | Update::Compacted { .. } => {
                    Update::Notice(update.notice().expect("compactions are told in words"))
                }
                update => update,
            };
            let payload = match update {
                Update::Notice(text) if session.prompt.is_some() => {
                    chunk("agent_thought_chunk", format!("\n[agt] {text}\n"))
                }
                Update::Notice(text) => {
                    session.notices.push(text);
                    continue;
                }
                Update::Error(message) if session.prompt.is_some() => {
                    // Shown now, and kept in case the turn fails because of it.
                    let payload =
                        chunk("agent_thought_chunk", format!("\n[agt] error: {message}\n"));
                    session.error = Some(message);
                    payload
                }
                Update::Error(message) => {
                    session.notices.push(message);
                    continue;
                }
                Update::TurnEnd(stop) => {
                    let Some(request) = session.prompt.take() else {
                        continue;
                    };
                    let result = match stop_reason(stop) {
                        Some(reason) => Ok(json!({ "stopReason": reason })),
                        None => Err(RpcError::internal(
                            session.error.take().as_deref().unwrap_or("the turn failed"),
                        )),
                    };
                    respond(request, result)?;
                    continue;
                }
                // The client shows the prompts it sends, but not those `agt
                // send` delivers.
                Update::User { origin: Origin::User, .. } => continue,
                update => {
                    for payload in payloads(update, &dir) {
                        send(&notification(&id, payload))?;
                    }
                    continue;
                }
            };
            send(&notification(&id, payload))?;
        }
        Ok(())
    }
}

fn send(message: &Value) -> io::Result<()> {
    // Serialized JSON escapes newlines, so each message is exactly one line.
    let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
    line.push(b'\n');
    let mut stdout = io::stdout().lock();
    stdout.write_all(&line)?;
    stdout.flush()
}

/// Answers request `id` with its result or error.
fn respond(id: Value, result: Result<Value, RpcError>) -> io::Result<()> {
    // Values are moved in, since `json!` would copy them.
    let mut message = json!({ "jsonrpc": "2.0" });
    message["id"] = id;
    match result {
        Ok(result) => message["result"] = result,
        Err(error) => message["error"] = json!({ "code": error.code, "message": error.message }),
    }
    send(&message)
}

fn notification(session_id: &str, update: Value) -> Value {
    let mut message = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": { "sessionId": session_id },
    });
    message["params"]["update"] = update;
    message
}

/// Context use, with the session's cost when it is known.
fn usage_update(usage: Usage) -> Value {
    let mut update =
        json!({ "sessionUpdate": "usage_update", "used": usage.used, "size": usage.budget });
    if let Some(amount) = usage.cost {
        update["cost"] = json!({ "amount": amount, "currency": "USD" });
    }
    update
}

fn chunk(kind: &str, text: String) -> Value {
    json!({ "sessionUpdate": kind, "content": { "type": "text", "text": text } })
}

fn text_content(text: String) -> Value {
    json!({ "type": "content", "content": { "type": "text", "text": text } })
}

/// The update that finishes a tool call, with the images it showed. A
/// cancelled call gets no status, so a client keeps showing it as cancelled
/// rather than failed.
fn tool_end(call_id: String, output: &Value, outcome: Outcome, dir: &Path) -> Value {
    let mut content = vec![text_content(item::content_text(output))];
    content.extend(image::references(output).filter_map(|reference| {
        let mut block = json!({ "type": "content" });
        block["content"] = image(dir, reference)?;
        Some(block)
    }));
    let mut update = json!({ "sessionUpdate": "tool_call_update", "toolCallId": call_id });
    update["content"] = content.into();
    match outcome {
        Outcome::Ok => update["status"] = "completed".into(),
        Outcome::Failed => update["status"] = "failed".into(),
        Outcome::Cancelled => {}
    }
    update
}

/// Model and reasoning selectors for a session.
fn config_options(models: &[Model], agent: &Agent) -> Value {
    let settings = agent.settings();
    let current = settings.model.id.as_str();
    let mut model_values: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    if !model_values.contains(&current) {
        model_values.insert(0, current);
    }
    let effort = settings.reasoning.map_or("default", Effort::as_str);
    let efforts = models::efforts(Some(&settings.model));
    let mut effort_values = vec!["default"];
    effort_values.extend(efforts.iter().map(|effort| effort.as_str()));
    if !effort_values.contains(&effort) {
        effort_values.push(effort);
    }
    let options = |values: &[&str]| -> Vec<Value> {
        values.iter().map(|value| json!({ "value": value, "name": value })).collect()
    };
    json!([
        {
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": current,
            "options": options(&model_values),
        },
        {
            "id": "reasoning",
            "name": "Reasoning effort",
            "category": "thought_level",
            "type": "select",
            "currentValue": effort,
            "options": options(&effort_values),
        },
    ])
}

/// The ACP stop reason for a finished turn, or `None` when it failed.
fn stop_reason(stop: Stop) -> Option<&'static str> {
    match stop {
        // `refusal` tells a client the prompt was dropped from the conversation,
        // which agt does not do.
        Stop::EndTurn | Stop::Refusal => Some("end_turn"),
        Stop::Cancelled => Some("cancelled"),
        Stop::MaxTokens => Some("max_tokens"),
        Stop::Error => None,
    }
}

/// Converts ACP content blocks into Responses API input parts. Images are
/// prepared and saved in `dir` as `agt view` prepares them.
fn content(blocks: &[Block], dir: &Path) -> Result<Vec<Value>, RpcError> {
    let mut parts = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            Block::Text { text } => parts.push(item::input_text(text.as_str())),
            Block::ResourceLink { name, uri } => {
                parts.push(item::input_text(format!("[{name}]({})", file_path(uri))));
            }
            Block::Resource { resource } => {
                let uri = file_path(&resource.uri);
                parts.push(item::input_text(match &resource.text {
                    Some(text) => format!("<resource uri=\"{uri}\">\n{text}\n</resource>"),
                    None => format!("[binary resource {uri} omitted]"),
                }));
            }
            Block::Image { data } => {
                let bytes = STANDARD
                    .decode(data)
                    .map_err(|_| RpcError::invalid_params("image data is not base64"))?;
                let image = image::attach(&bytes, dir).map_err(|error| {
                    RpcError::invalid_params(format!("cannot use the image: {error}"))
                })?;
                parts.extend(image);
            }
        }
    }
    Ok(parts)
}

/// A `file://` URI as the path the model can use with bash.
fn file_path(uri: &str) -> &str {
    uri.strip_prefix("file://").unwrap_or(uri)
}

/// The `session/update` payloads that show an update, for the updates that
/// look the same live and replayed. Notices, errors and the end of a turn
/// depend on the prompt being answered, so they give none.
fn payloads(update: Update, dir: &Path) -> Vec<Value> {
    let payload = match update {
        Update::User { text, images, .. } => {
            let text = (!text.is_empty()).then(|| chunk("user_message_chunk", text));
            let shown = images.iter().filter_map(|reference| {
                let mut update = json!({ "sessionUpdate": "user_message_chunk" });
                update["content"] = image(dir, reference)?;
                Some(update)
            });
            return text.into_iter().chain(shown).collect();
        }
        Update::Text(text) => chunk("agent_message_chunk", text),
        Update::Thinking(text) if text.is_empty() => return Vec::new(),
        Update::Thinking(text) => chunk("agent_thought_chunk", text),
        Update::ToolStart(call) => tool_call(call),
        Update::ToolProgress { call_id, lines } => json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": call_id,
            "content": [text_content(lines.join("\n"))],
        }),
        Update::ToolEnd { call_id, output, outcome } => tool_end(call_id, &output, outcome, dir),
        Update::Usage(usage) => usage_update(usage),
        Update::Reset => chunk(
            "agent_thought_chunk",
            "\n[agt] the partial response above was discarded\n".into(),
        ),
        Update::ResponseEnd
        | Update::Notice(_)
        | Update::Error(_)
        | Update::Compacting
        | Update::Compacted { .. }
        | Update::TurnEnd(_) => return Vec::new(),
    };
    vec![payload]
}

/// The update that starts a tool call, with its arguments as the model wrote
/// them.
fn tool_call(call: ToolCall) -> Value {
    let mut update = json!({
        "sessionUpdate": "tool_call",
        "toolCallId": call.id,
        "title": call.title(),
        "kind": "execute",
        "status": "in_progress",
    });
    update["rawInput"] =
        serde_json::from_str(&call.arguments).unwrap_or_else(|_| call.arguments.into());
    update
}

/// An image content block for a saved image, unless the file is gone.
fn image(dir: &Path, reference: &str) -> Option<Value> {
    let (mime, data) = image::load(dir, reference).ok()?;
    let mut image = json!({ "type": "image", "mimeType": mime });
    image["data"] = data.into();
    Some(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> String {
        let mut bytes = Vec::new();
        ::image::DynamicImage::new_rgb8(8, 4)
            .write_to(std::io::Cursor::new(&mut bytes), ::image::ImageFormat::Png)
            .expect("png");
        STANDARD.encode(bytes)
    }

    fn blocks(blocks: Value) -> Vec<Block> {
        parse(blocks).expect("valid blocks")
    }

    #[test]
    fn prompt_blocks_become_input_parts() {
        let dir = tempfile::tempdir().expect("temp dir");
        let prompt = blocks(json!([
            { "type": "text", "text": "look at" },
            { "type": "resource_link", "name": "main.rs", "uri": "file:///p/main.rs" },
            { "type": "resource", "resource": { "uri": "file:///p/a.txt", "text": "hello" } },
            { "type": "resource", "resource": { "uri": "file:///p/a.bin", "blob": "AAAA" } },
            { "type": "image", "mimeType": "image/png", "data": png() },
        ]));
        let parts = content(&prompt, dir.path()).expect("valid");
        assert_eq!(parts[0]["text"], "look at");
        assert_eq!(parts[1]["text"], "[main.rs](/p/main.rs)");
        assert_eq!(parts[2]["text"], "<resource uri=\"/p/a.txt\">\nhello\n</resource>");
        assert_eq!(parts[3]["text"], "[binary resource /p/a.bin omitted]");
        let header = parts[4]["text"].as_str().expect("header");
        assert!(header.starts_with(image::HEADER), "{header}");
        let reference = parts[5]["image_url"].as_str().expect("reference");
        assert!(dir.path().join(reference).is_file(), "{reference} is not saved");
        let undecodable = blocks(json!([{ "type": "image", "data": "not base64" }]));
        let error = content(&undecodable, dir.path()).expect_err("rejected");
        assert_eq!(error.message, "image data is not base64");
    }

    #[test]
    fn updates_become_session_updates() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dir = dir.path();
        let invalid = ToolCall::new("c2".into(), "bash", "{oops".into());
        let started = payloads(Update::ToolStart(invalid), dir).remove(0);
        assert_eq!(started["rawInput"], "{oops", "arguments that are not JSON are sent as written");

        let end = |output: Value, outcome| {
            payloads(Update::ToolEnd { call_id: "c1".into(), output, outcome }, dir).remove(0)
        };
        assert_eq!(end("boom".into(), Outcome::Failed)["status"], "failed");
        let cancelled = end("stopped".into(), Outcome::Cancelled);
        assert!(cancelled.get("status").is_none(), "a cancelled call keeps no status: {cancelled}");
        let attached = content(&blocks(json!([{ "type": "image", "data": png() }])), dir);
        let viewed = end(Value::Array(attached.expect("image")), Outcome::Ok);
        assert_eq!(viewed["content"][1]["content"]["type"], "image", "{viewed}");

        for update in [
            Update::Thinking(String::new()),
            Update::Notice("n".into()),
            Update::Error("e".into()),
            Update::ResponseEnd,
            Update::TurnEnd(Stop::EndTurn),
        ] {
            let label = format!("{update:?}");
            assert_eq!(payloads(update, dir), Vec::<Value>::new(), "{label}");
        }
    }

    #[test]
    fn refusals_end_the_turn_normally() {
        assert_eq!(stop_reason(Stop::Refusal), Some("end_turn"));
        assert_eq!(stop_reason(Stop::Cancelled), Some("cancelled"));
        assert_eq!(stop_reason(Stop::Error), None);
    }
}
