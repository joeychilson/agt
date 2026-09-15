//! MCP servers end to end: set up and called with `agt mcp` in and out of
//! sessions, over stdio and HTTP, in both eras of the protocol.

mod support;

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use serde_json::json;
use support::{Server, agt, call, golden, text};

/// A fake server that speaks either era over stdio or HTTP.
const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/mcp_server.py");

/// What `agt mcp` exited with and printed.
type Run = (Option<i32>, String, String);

/// Runs `agt mcp` in `home` with `args`.
fn mcp(home: &Path, model: &Server, args: &[&str]) -> Run {
    let output = agt(home, model)
        .env("FAKE_TOKEN", "t0ken")
        .arg("mcp")
        .args(args)
        .output()
        .expect("agt runs");
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    (output.status.code(), text(&output.stdout), text(&output.stderr))
}

fn ok(stdout: &str) -> Run {
    (Some(0), stdout.to_owned(), String::new())
}

fn failed(stdout: &str, stderr: &str) -> Run {
    (Some(1), stdout.to_owned(), stderr.to_owned())
}

#[test]
fn servers_are_added_listed_called_and_removed() {
    let home = tempfile::tempdir().expect("temp dir");
    let model = Server::start(Vec::new());
    let mcp = |args: &[&str]| mcp(home.path(), &model, args);
    assert_eq!(
        mcp(&["add", "old", "python3", FAKE, "stdio", "legacy"]),
        ok(&format!("added old: python3 {FAKE} stdio legacy\n"))
    );
    assert_eq!(mcp(&["add", "new", "--", "python3", FAKE, "stdio", "modern"]).0, Some(0));
    let settings = fs::metadata(home.path().join(".agt/mcp.json")).expect("settings");
    assert_eq!(settings.permissions().mode() & 0o777, 0o600, "settings may hold tokens");
    assert_eq!(
        mcp(&["list"]),
        ok(&format!(
            "new  stdio  python3 {FAKE} stdio modern\nold  stdio  python3 {FAKE} stdio legacy\n"
        ))
    );

    for server in ["old", "new"] {
        let (code, tools, stderr) = mcp(&["tools", server]);
        assert_eq!(code, Some(0), "{stderr}");
        assert!(
            tools.starts_with(&format!("{server} · Fake Server\nTools for testing agt.")),
            "{tools}"
        );
        assert!(
            tools.contains("\necho(region?: string, text: string)\n  Returns its text"),
            "{tools}"
        );
        // A command outside a session starts a server of its own.
        assert_eq!(mcp(&["call", server, "count"]), ok("count 1\n"));
        assert_eq!(mcp(&["call", server, "count", "{}"]), ok("count 1\n"));
    }

    assert_eq!(mcp(&["call", "new", "fail"]), failed("it failed\n", ""));
    assert_eq!(
        mcp(&["call", "new", "image"]),
        ok("a pixel\n[image image/png: shown only in an agt session]\n")
    );
    assert_eq!(
        mcp(&["call", "new", "echo", "{}"]),
        failed("", "agt mcp: echo needs text: echo(region?: string, text: string)\n")
    );
    assert_eq!(
        mcp(&["call", "new", "coun"]),
        failed("", "agt mcp: new has no tool \"coun\"; did you mean count?\n")
    );
    assert_eq!(
        mcp(&["call", "gone", "count"]),
        failed("", "agt mcp: there is no MCP server \"gone\"; the servers are new, old\n")
    );
    let (code, _, stderr) = mcp(&["call", "new", "echo", "[1]"]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("the arguments must be a JSON object"), "{stderr}");

    assert_eq!(mcp(&["remove", "old"]), ok("removed old\n"));
    assert_eq!(mcp(&["list"]), ok(&format!("new  stdio  python3 {FAKE} stdio modern\n")));
}

#[test]
fn sessions_keep_servers_running_and_attach_the_images_tools_return() {
    let home = tempfile::tempdir().expect("temp dir");
    fs::copy(FAKE, home.path().join("mcp_server.py")).expect("copy the fake server");
    let fake = json!({ "command": "python3", "args": ["mcp_server.py", "stdio", "legacy"] });
    fs::write(home.path().join(".mcp.json"), json!({ "mcpServers": { "fake": fake } }).to_string())
        .expect("project settings");
    let calls = "agt mcp call fake count && agt mcp call fake count && agt mcp list";
    let model = Server::start(vec![
        call("call_1", "bash", json!({ "command": calls }), 100),
        call("call_2", "bash", json!({ "command": "agt mcp call fake image" }), 100),
        text("done"),
    ]);
    // Tools listed once are named in the instructions of later sessions.
    assert_eq!(mcp(home.path(), &model, &["tools", "fake"]).0, Some(0));
    let output =
        agt(home.path(), &model).args(["-p", "use the fake server"]).output().expect("agt runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    // The count carries from one call to the next, and the image reaches the model.
    golden("mcp-session.json", home.path(), &model.exchanges());
}

/// A fake server over HTTP, stopped when dropped.
struct Remote {
    child: Child,
    url: String,
}

impl Remote {
    fn start(era: &str) -> Self {
        let mut child = Command::new("python3")
            .args([FAKE, "http", era])
            .stdout(Stdio::piped())
            .spawn()
            .expect("python3 runs");
        let mut url = String::new();
        let stdout = child.stdout.take().expect("stdout");
        BufReader::new(stdout).read_line(&mut url).expect("the server's address");
        Self { child, url: url.trim().to_owned() }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn http_servers_are_reached_in_either_era() {
    let home = tempfile::tempdir().expect("temp dir");
    let model = Server::start(Vec::new());
    let mcp = |args: &[&str]| mcp(home.path(), &model, args);
    let (modern, legacy) = (Remote::start("modern"), Remote::start("legacy"));
    let token = "Authorization: Bearer ${FAKE_TOKEN}";
    assert_eq!(mcp(&["add", "new", &modern.url, "--header", token]).0, Some(0));
    assert_eq!(mcp(&["add", "old", &legacy.url]).0, Some(0));
    let settings = fs::read_to_string(home.path().join(".agt/mcp.json")).expect("settings");
    assert!(settings.contains("Bearer ${FAKE_TOKEN}"), "the token stays in the environment");

    // Modern requests mirror their method, tool and annotated arguments into
    // headers, which the server checks.
    assert_eq!(
        mcp(&["call", "new", "echo", r#"{"text": "hi", "region": "us-west1"}"#]),
        ok("hi\nauthorization: Bearer t0ken\nmcp-method: tools/call\nmcp-name: echo\n\
            mcp-param-region: us-west1\nmcp-protocol-version: 2026-07-28\n")
    );
    // The legacy server forgets the session after listing the tools the call
    // checks, so the call is made in a second one.
    assert_eq!(
        mcp(&["call", "old", "echo", r#"{"text": "hi"}"#]),
        ok("hi\nmcp-protocol-version: 2025-06-18\nmcp-session-id: session-2\n")
    );
}
