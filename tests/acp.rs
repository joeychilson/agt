//! Agent Client Protocol end to end over stdio.

mod support;

use std::io::{BufRead, BufReader, Cursor, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use support::{Server, agt, call, text};

/// An ACP client driving `agt acp` over pipes.
struct Client {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    /// Messages received before a response that `wait_for` read ahead.
    pending: Vec<Value>,
    /// Every request sent and message received, apart from those whose
    /// arrival depends on timing: model listings and live tool output.
    transcript: Vec<Value>,
}

impl Client {
    fn spawn(mut command: Command) -> Self {
        let mut child = command
            .arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("agt acp starts");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        Self { child, stdin, lines, next_id: 0, pending: Vec::new(), transcript: Vec::new() }
    }

    /// Sends a request and returns the notifications before its response, and the response.
    fn request(&mut self, method: &str, params: Value) -> (Vec<Value>, Value) {
        let id = self.begin(method, params);
        self.response(id)
    }

    fn begin(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(self.stdin, "{message}").expect("write request");
        self.transcript.push(message);
        id
    }

    fn response(&mut self, id: u64) -> (Vec<Value>, Value) {
        let mut notifications = std::mem::take(&mut self.pending);
        loop {
            let message = self.receive();
            if message["id"] == id {
                return (notifications, message);
            }
            notifications.push(message);
        }
    }

    fn receive(&mut self) -> Value {
        let line = self.lines.recv_timeout(Duration::from_secs(30)).expect("agt answers in time");
        let message: Value = serde_json::from_str(&line).expect("one JSON message per line");
        let update = &message["params"]["update"];
        let timed = update["sessionUpdate"] == "config_option_update"
            || (update["sessionUpdate"] == "tool_call_update" && update.get("status").is_none());
        if !timed {
            self.transcript.push(message.clone());
        }
        message
    }

    /// Reads until a session update of `kind`, keeping what it read for the
    /// next response.
    fn wait_for(&mut self, kind: &str) {
        loop {
            let message = self.receive();
            let found = message["params"]["update"]["sessionUpdate"] == kind;
            self.pending.push(message);
            if found {
                return;
            }
        }
    }

    /// Checks everything exchanged so far against golden `name`.
    fn golden(&self, name: &str, home: &Path) {
        let transcript = serde_json::to_string_pretty(&self.transcript).expect("JSON");
        support::golden(name, home, &transcript);
    }

    fn close(self) {
        let Self { mut child, stdin, .. } = self;
        drop(stdin);
        assert!(child.wait().expect("agt exits").success());
    }
}

#[test]
fn sessions_are_created_prompted_reloaded_and_deleted() {
    let home = tempfile::tempdir().expect("temp dir");
    let cwd = home.path().to_str().expect("utf-8 path");
    let server = Server::start(vec![
        call("call_1", "bash", json!({ "command": "echo acp-tool" }), 100),
        text("hello from agt"),
    ]);
    let prompt = |session: &str, text: &str| {
        json!({
            "sessionId": session,
            "prompt": [{ "type": "text", "text": text }],
        })
    };

    let mut client = Client::spawn(agt(home.path(), &server));
    // A client that asks for a newer version is answered with the one agt speaks.
    client.request("initialize", json!({ "protocolVersion": 2 }));
    // The MCP servers the client offers are listed in the instructions, and
    // one agt cannot reach is noticed.
    let servers = json!([
        { "name": "files", "command": "mcp-files", "args": [], "env": [] },
        { "type": "http", "name": "docs", "url": "https://docs.example/mcp", "headers": [] },
        { "type": "sse", "name": "events", "url": "https://events.example/sse", "headers": [] },
    ]);
    let (_, response) = client.request("session/new", json!({ "cwd": cwd, "mcpServers": servers }));
    let session = response["result"]["sessionId"].as_str().expect("session id").to_owned();
    // Later options list the models once the listing is in.
    client.wait_for("config_option_update");
    client.request(
        "session/set_config_option",
        json!({ "sessionId": session, "configId": "reasoning", "value": "high" }),
    );
    client.request("session/prompt", prompt(&session, "run it"));
    client.request("session/list", json!({ "cwd": cwd }));
    client.request("session/close", json!({ "sessionId": session }));
    client.request("session/prompt", prompt(&session, "again"));
    client.request("session/unknown", json!({}));
    client.golden("acp-session.json", home.path());
    client.close();

    let mut client = Client::spawn(agt(home.path(), &server));
    client.request("initialize", json!({ "protocolVersion": 1 }));
    client.request("session/load", json!({ "sessionId": session, "cwd": cwd, "mcpServers": [] }));
    client.request("session/list", json!({}));
    client.request("session/load", json!({ "sessionId": "123-abc", "cwd": cwd, "mcpServers": [] }));
    client.golden("acp-load.json", home.path());
    client.close();

    let mut client = Client::spawn(agt(home.path(), &server));
    client.request("initialize", json!({ "protocolVersion": 1 }));
    client.request("session/resume", json!({ "sessionId": session, "cwd": cwd, "mcpServers": [] }));
    client.request("session/delete", json!({ "sessionId": session }));
    client.request("session/list", json!({}));
    client.request("session/delete", json!({ "sessionId": session }));
    client.golden("acp-resume.json", home.path());
    client.close();
    support::golden("acp-requests.json", home.path(), &server.exchanges());
}

#[test]
fn clients_are_sent_to_sign_in_until_agt_is_set_up() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(Vec::new());
    let mut command = agt(home.path(), &server);
    command.env_remove("AGT_API_KEY");
    let mut client = Client::spawn(command);
    let (_, response) = client.request(
        "initialize",
        json!({ "protocolVersion": 1, "clientCapabilities": { "auth": { "terminal": true } } }),
    );
    let method = &response["result"]["authMethods"][0];
    assert_eq!(method["type"], "terminal");
    assert_eq!(method["args"], json!(["login"]));
    let (_, response) =
        client.request("session/new", json!({ "cwd": home.path(), "mcpServers": [] }));
    assert_eq!(response["error"]["code"], -32000, "{response}");
    client.close();
    assert!(server.requests().is_empty());
}

#[test]
fn invalid_config_values_and_prompt_blocks_are_rejected() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(Vec::new());
    let mut client = Client::spawn(agt(home.path(), &server));
    client.request("initialize", json!({ "protocolVersion": 1 }));
    let (_, response) =
        client.request("session/new", json!({ "cwd": home.path(), "mcpServers": [] }));
    let session = response["result"]["sessionId"].as_str().expect("session").to_owned();
    for (config, value) in [("model", ""), ("model", "unknown"), ("reasoning", "arbitrary")] {
        let (_, response) = client.request(
            "session/set_config_option",
            json!({ "sessionId": session, "configId": config, "value": value }),
        );
        assert_eq!(response["error"]["code"], -32602, "{config} {value:?}: {response}");
    }
    let untexted = json!({ "type": "text" });
    let audio = json!({ "type": "audio", "mimeType": "audio/wav", "data": "AAAA" });
    for block in [untexted, audio] {
        let (_, response) =
            client.request("session/prompt", json!({ "sessionId": session, "prompt": [block] }));
        assert_eq!(response["error"]["code"], -32602, "{block}: {response}");
    }
    client.close();
    assert!(server.requests().is_empty());
}

#[test]
fn slow_model_discovery_does_not_block_sessions_or_cancellation() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::with_model_delay(Vec::new(), Duration::from_secs(4));
    let mut client = Client::spawn(agt(home.path(), &server));
    client.request("initialize", json!({"protocolVersion":1}));
    let start = Instant::now();
    let (_, response) = client.request("session/new", json!({"cwd":home.path(),"mcpServers":[]}));
    let session = response["result"]["sessionId"].as_str().expect("session").to_owned();
    assert!(start.elapsed() < Duration::from_secs(2), "opening waited for model discovery");
    let prompt = client.begin(
        "session/prompt",
        json!({"sessionId":session,"prompt":[{"type":"text","text":"wait"}]}),
    );
    let cancel = json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}});
    writeln!(client.stdin, "{cancel}").expect("cancel notification");
    let (_, response) = client.response(prompt);
    assert_eq!(response["result"]["stopReason"], "cancelled");
    assert!(start.elapsed() < Duration::from_secs(2), "cancellation waited for model discovery");
    client.close();
}

#[test]
fn background_exits_reach_the_model_before_the_next_prompt() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(vec![
        call("call_1", "bash", json!({ "command": "echo background-done", "wait": 0 }), 100),
        text("started"),
        text("noted"),
    ]);
    let mut client = Client::spawn(agt(home.path(), &server));
    client.request("initialize", json!({ "protocolVersion": 1 }));
    let (_, response) =
        client.request("session/new", json!({ "cwd": home.path(), "mcpServers": [] }));
    let session = response["result"]["sessionId"].as_str().expect("session").to_owned();
    let prompt =
        |text: &str| json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] });
    client.request("session/prompt", prompt("start it"));
    // The process exits while no prompt runs.
    thread::sleep(Duration::from_secs(1));
    client.request("session/prompt", prompt("what happened?"));
    client.close();

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let input = requests[2]["body"]["input"].as_array().expect("input");
    let texts: Vec<&str> = input[input.len() - 2..]
        .iter()
        .filter_map(|item| item["content"][0]["text"].as_str())
        .collect();
    assert_eq!(texts.len(), 2, "{input:?}");
    let notice =
        texts[0].strip_prefix("<background>\n[").and_then(|rest| rest.split_once(" UTC] "));
    assert!(
        notice.is_some_and(|(_, notice)| notice
            .starts_with("process 1 (echo background-done) finished with exit 0")),
        "{texts:?}"
    );
    assert_eq!(texts[1], "what happened?");
}

#[test]
fn attached_images_reach_the_model_and_replay_when_loaded() {
    let home = tempfile::tempdir().expect("temp dir");
    let cwd = home.path().to_str().expect("utf-8 path");
    let server = Server::start(vec![text("a small image")]);
    let mut png = Vec::new();
    image::DynamicImage::new_rgb8(64, 32)
        .write_to(Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("png");
    let data = STANDARD.encode(&png);

    let mut client = Client::spawn(agt(home.path(), &server));
    client.request("initialize", json!({ "protocolVersion": 1 }));
    let (_, response) = client.request("session/new", json!({ "cwd": cwd, "mcpServers": [] }));
    let session = response["result"]["sessionId"].as_str().expect("session").to_owned();
    client.request(
        "session/prompt",
        json!({ "sessionId": session, "prompt": [
            { "type": "text", "text": "what is this?" },
            { "type": "image", "mimeType": "image/png", "data": data },
        ] }),
    );
    // An image that cannot be decoded rejects its prompt.
    client.request(
        "session/prompt",
        json!({ "sessionId": session, "prompt": [{ "type": "image", "data": "AAAA" }] }),
    );
    client.golden("acp-images.json", home.path());
    client.close();

    let content = &server.requests()[0]["body"]["input"][0]["content"];
    assert_eq!(content[0]["text"], "what is this?");
    assert!(
        content[1]["text"].as_str().is_some_and(|text| text.contains(" · 64x32]")),
        "{content}"
    );
    assert_eq!(content[2]["image_url"], format!("data:image/png;base64,{data}"));

    let mut client = Client::spawn(agt(home.path(), &server));
    client.request("initialize", json!({ "protocolVersion": 1 }));
    client.request("session/load", json!({ "sessionId": session, "cwd": cwd, "mcpServers": [] }));
    client.golden("acp-images-load.json", home.path());
    client.close();
}
