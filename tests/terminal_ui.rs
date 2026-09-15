//! The terminal UI end to end, in a pseudo-terminal rendered by an emulator.

mod support;

use std::fs;
use std::io::{Read, Write};
use std::process::Child;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use pty_process::blocking::{Command, Pty};
use serde_json::{Value, json};
use support::{Server, agt, call, golden, text};

const ROWS: u16 = 30;
const COLS: u16 = 100;

/// agt running in a pseudo-terminal, with its output rendered by vt100.
struct Terminal {
    pty: Arc<Pty>,
    output: Receiver<Vec<u8>>,
    screen: vt100::Parser,
    child: Child,
}

impl Terminal {
    /// Runs `template`'s program with its environment and working directory.
    fn spawn(template: &std::process::Command) -> Self {
        let (pty, pts) = pty_process::blocking::open().expect("open a terminal");
        pty.resize(pty_process::Size::new(ROWS, COLS)).expect("size the terminal");
        let mut command = Command::new(template.get_program()).args(template.get_args());
        if let Some(dir) = template.get_current_dir() {
            command = command.current_dir(dir);
        }
        for (key, value) in template.get_envs() {
            command = match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        let child = command.spawn(pts).expect("agt starts");
        let pty = Arc::new(pty);
        let (sender, output) = mpsc::channel();
        let reader = Arc::clone(&pty);
        thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(read @ 1..) = (&*reader).read(&mut buf) {
                if sender.send(buf[..read].to_vec()).is_err() {
                    return;
                }
            }
        });
        Self { pty, output, screen: vt100::Parser::new(ROWS, COLS, 0), child }
    }

    fn keys(&self, keys: &str) {
        (&*self.pty).write_all(keys.as_bytes()).expect("type into the terminal");
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.screen.screen_mut().set_size(rows, cols);
        self.pty.resize(pty_process::Size::new(rows, cols)).expect("resize terminal");
    }

    /// Renders output until the screen contains every one of `texts`.
    fn wait_for(&mut self, texts: &[&str]) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let contents = self.screen.screen().contents();
            if texts.iter().all(|text| contents.contains(text)) {
                return contents;
            }
            match self.output.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(bytes) => self.screen.process(&bytes),
                Err(_) => panic!("timed out waiting for {texts:?}; screen:\n{contents}"),
            }
        }
    }

    /// Renders output until none arrives for half a second, so the screen
    /// shows the last frame drawn.
    fn settle(&mut self) {
        while let Ok(bytes) = self.output.recv_timeout(Duration::from_millis(500)) {
            self.screen.process(&bytes);
        }
    }

    /// The screen's rows with their styles, written as the escape sequence
    /// that selects each style (`\e[0;2m`) where it changes. Cells are read
    /// one by one, so a row reads the same however many frames drew it.
    fn styled_rows(&self) -> String {
        let screen = self.screen.screen();
        let (rows, cols) = screen.size();
        let mut out = Vec::new();
        for row in 0..rows {
            let mut line = String::new();
            let mut current = String::from("0");
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else { continue };
                if cell.is_wide_continuation() {
                    continue;
                }
                let contents = if cell.has_contents() { cell.contents() } else { " " };
                let mut style = String::from("0");
                if contents != " " {
                    for (on, code) in [(cell.bold(), "1"), (cell.dim(), "2"), (cell.italic(), "3")]
                    {
                        if on {
                            style.push(';');
                            style.push_str(code);
                        }
                    }
                    if let vt100::Color::Idx(index) = cell.fgcolor() {
                        style.push_str(&format!(";{}", 30 + u16::from(index)));
                    }
                }
                if style != current {
                    line.push_str(&format!("\\e[{style}m"));
                    current = style;
                }
                line.push_str(contents);
            }
            let trimmed = line.trim_end_matches(' ');
            out.push(
                trimmed.strip_suffix("\\e[0m").unwrap_or(trimmed).trim_end_matches(' ').to_owned(),
            );
        }
        out.join("\n")
    }

    /// Clicks where the screen first shows `text`, which is ASCII.
    fn click(&mut self, text: &str) {
        self.wait_for(&[text]);
        let screen = self.screen.screen();
        let (rows, cols) = screen.size();
        let spot = (0..rows).find_map(|row| {
            let cells: Vec<char> = (0..cols)
                .map(|col| {
                    screen
                        .cell(row, col)
                        .and_then(|cell| cell.contents().chars().next())
                        .unwrap_or(' ')
                })
                .collect();
            let wanted: Vec<char> = text.chars().collect();
            let col = cells.windows(wanted.len()).position(|window| window == wanted)?;
            Some((usize::from(row) + 1, col + 1))
        });
        let (y, x) = spot.unwrap_or_else(|| panic!("{text:?} is not on the screen"));
        self.keys(&format!("\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m"));
    }

    fn quit(mut self) {
        self.keys("\x04");
        self.wait_for(&["resume with: agt -r"]);
        assert!(self.child.wait().expect("agt exits").success());
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_turn_and_the_effort_menu_look_as_they_did() {
    let home = tempfile::tempdir().expect("temp dir");
    image::RgbImage::new(4, 2).save(home.path().join("shot.png")).expect("image");
    // One call per response, so their rows appear in a fixed order.
    let mut script = vec![
        call("call_0", "bash", json!({ "command": "printf 'one\\ntwo\\n'" }), 100),
        call("call_1", "bash", json!({ "command": "echo broken; exit 3" }), 100),
        call("call_2", "bash", json!({ "command": "agt view shot.png" }), 100),
    ];
    let mut answer = text(
        "## Result\n\nThe **header** uses `flex`:\n- one\n- two\n\n```sh\ncargo test\n```\n> done",
    );
    answer.insert(
        0,
        json!({ "type": "response.reasoning_summary_text.delta", "delta": "**Checking**\n\nthe layout" }),
    );
    script.push(answer);
    let server = Server::start(script);
    let mut terminal = Terminal::spawn(&agt(home.path(), &server));
    terminal.wait_for(&["Ask anything"]);
    terminal.keys("check the header\r");
    terminal.wait_for(&["│ done", "Ask anything"]);
    terminal.settle();
    golden("tui-turn.txt", home.path(), &terminal.styled_rows());
    terminal.keys("/effort\r");
    terminal.wait_for(&["Reasoning effort"]);
    terminal.settle();
    golden("tui-effort-menu.txt", home.path(), &terminal.styled_rows());
    terminal.keys("\x1b");
    // The menu covers the reply's end, so seeing it again means Esc closed the
    // menu. Ctrl-D sent sooner can arrive with the Esc and read as Alt-Ctrl-D.
    terminal.wait_for(&["│ done", "Ask anything"]);
    terminal.quit();
    golden("tui-requests.json", home.path(), &server.exchanges());
}

#[test]
fn a_new_session_opens_on_its_skills_and_mcp_servers() {
    let home = tempfile::tempdir().expect("temp dir");
    // More skills than the welcome lists, so the rest are counted.
    for n in 1..=10 {
        let dir = home.path().join(format!(".agents/skills/skill-{n:02}"));
        fs::create_dir_all(&dir).expect("skill directory");
        let skill = format!(
            "---\nname: skill-{n:02}\ndescription: Does task {n} as this project does it.\n---\n"
        );
        fs::write(dir.join("SKILL.md"), skill).expect("skill");
    }
    // A server agt has used says what it is for; one never used, what serves it.
    let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/mcp_server.py");
    let servers = json!({ "mcpServers": {
        "docs": { "url": "https://docs.example/mcp" },
        "fake": { "command": "python3", "args": [fake, "stdio", "modern"] },
    }});
    fs::create_dir_all(home.path().join(".agt")).expect("agt home");
    fs::write(home.path().join(".agt/mcp.json"), servers.to_string()).expect("MCP settings");
    let server = Server::start(Vec::new());
    let listed =
        agt(home.path(), &server).args(["mcp", "tools", "fake"]).output().expect("agt runs");
    assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
    let mut terminal = Terminal::spawn(&agt(home.path(), &server));
    terminal.wait_for(&["Ask anything", "skill-01", "Tools for testing agt."]);
    terminal.settle();
    // The turn's golden pins the header. Before a request, its context estimate
    // counts the temporary paths in the skill catalog, which differ by machine.
    let screen = terminal.styled_rows();
    let (_, welcome) = screen.split_once('\n').expect("the screen has a header row");
    golden("tui-welcome.txt", home.path(), welcome);
    terminal.quit();
}

#[test]
fn resumed_sessions_show_their_transcript() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(vec![
        call("call_1", "bash", json!({ "command": "echo from-tool" }), 100),
        text("the **first** answer"),
    ]);
    let output = agt(home.path(), &server).args(["-p", "run it"]).output().expect("agt runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let mut command = agt(home.path(), &server);
    command.arg("-c");
    let mut terminal = Terminal::spawn(&command);
    terminal.wait_for(&["❯ run it", "▸ Worked · 1 command", "the first answer", "Ask anything"]);
    terminal.quit();
    assert_eq!(server.requests().len(), 2, "resuming sends nothing");
}

#[test]
fn messages_sent_while_working_reach_the_next_step_or_wait_until_done() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(vec![
        call("call_1", "bash", json!({ "command": "sleep 30" }), 100),
        text("heard you"),
        text("later handled"),
    ]);
    let mut terminal = Terminal::spawn(&agt(home.path(), &server));
    terminal.wait_for(&["Ask anything"]);
    terminal.keys("wait a while\r");
    terminal.wait_for(&["Working…"]);
    terminal.keys("afterwards\t");
    terminal.wait_for(&["↳ afterwards", "when done"]);
    let sent = Instant::now();
    terminal.keys("change of plan\r");
    terminal.wait_for(&["heard you", "later handled"]);
    // The command waits up to 30 seconds, but not with a message waiting on it.
    assert!(sent.elapsed() < Duration::from_secs(10), "took {:?}", sent.elapsed());
    terminal.quit();

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let input =
        |index: usize| requests[index]["body"]["input"].as_array().cloned().unwrap_or_default();
    let last_text =
        |index: usize| input(index).last().map(|item| item["content"][0]["text"].clone());
    let output = input(1)
        .into_iter()
        .find(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].clone());
    assert!(
        output
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|text| text.starts_with("[id 1 · running")),
        "the command is still running when the message goes out: {output:?}"
    );
    assert_eq!(last_text(1), Some(json!("change of plan")));
    assert!(
        !requests[1]["body"].to_string().contains("afterwards"),
        "a later message waits for the turn to end"
    );
    assert_eq!(last_text(2), Some(json!("afterwards")));
}

#[test]
fn escape_interrupts_the_turn_and_puts_waiting_messages_back() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(vec![call("call_1", "bash", json!({ "command": "sleep 30" }), 100)]);
    let mut terminal = Terminal::spawn(&agt(home.path(), &server));
    terminal.wait_for(&["Ask anything"]);
    terminal.keys("wait a while\r");
    terminal.wait_for(&["Working…", "esc to stop"]);
    terminal.keys("first\t");
    terminal.keys("second\t");
    terminal.wait_for(&["↳ first", "↳ second"]);
    // Up in an empty input takes back the newest waiting message.
    terminal.keys("\x1b[A");
    let screen = terminal.wait_for(&["❯ second", "↳ first"]);
    assert!(!screen.contains("↳ second"), "still waiting:\n{screen}");
    terminal.keys("\x1b");
    terminal.wait_for(&["1 command · interrupted", "❯ first", "  second"]);
    // Ctrl-C clears the input, so Ctrl-D can quit.
    terminal.keys("\x03");
    terminal.wait_for(&["Ask anything"]);
    terminal.quit();
    assert_eq!(server.requests().len(), 1, "waiting messages are not sent after an interruption");
}

#[test]
fn the_status_line_says_in_a_word_what_the_agent_is_doing() {
    let home = tempfile::tempdir().expect("temp dir");
    let mut slow = call("call_1", "bash", json!({ "command": "sleep 2" }), 100);
    slow.insert(0, json!({ "delay_ms": 1500 }));
    let server = Server::start(vec![
        vec![json!({ "status": 429, "error": { "message": "slow down" } })],
        slow,
        text("done"),
    ]);
    let mut terminal = Terminal::spawn(&agt(home.path(), &server));
    terminal.wait_for(&["Ask anything"]);
    terminal.keys("go\r");
    for word in ["Retrying…", "Thinking…", "Working…"] {
        let screen = terminal.wait_for(&[word]);
        let status = screen.lines().last().unwrap_or_default();
        assert!(status.contains(word) && !status.contains("sleep"), "status line: {status}");
    }
    terminal.wait_for(&["done", "Ask anything"]);
    terminal.quit();
}

#[test]
fn dragging_over_the_transcript_copies_its_text() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(vec![text("the answer to copy")]);
    let mut terminal = Terminal::spawn(&agt(home.path(), &server));
    terminal.wait_for(&["Ask anything"]);
    terminal.keys("question\r");
    let screen = terminal.wait_for(&["the answer to copy", "Ask anything"]);
    let (row, column) = screen
        .lines()
        .enumerate()
        .find_map(|(row, line)| Some((row + 1, line.find("the answer")? + 1)))
        .expect("the answer is on screen");
    let end = column + "the answer".len() - 1;
    // Press, drag and release the left button, as SGR mouse reports send them.
    terminal.keys(&format!("\x1b[<0;{column};{row}M\x1b[<32;{end};{row}M\x1b[<0;{end};{row}m"));
    terminal.wait_for(&["copied the selection"]);
    terminal.quit();
}

#[test]
fn first_run_setup_saves_the_provider_key_model_and_effort() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(vec![text("connected")]);
    let mut command = agt(home.path(), &server);
    for name in ["AGT_PROVIDER", "AGT_API_KEY", "AGT_MODEL"] {
        command.env_remove(name);
    }
    let mut terminal = Terminal::spawn(&command);
    terminal.wait_for(&["Connect a provider", "Vercel AI Gateway"]);
    terminal.keys("openrouter\r");
    terminal.wait_for(&["Sign in to OpenRouter", "Paste an API key"]);
    terminal.keys("paste\r");
    terminal.wait_for(&["OpenRouter API key"]);
    terminal.keys("secret-key\r");
    let screen = terminal.wait_for(&["Model · OpenRouter", "test-model", "64k", "$1.00", "$2.00"]);
    assert!(!screen.contains("secret-key"), "the key is shown:\n{screen}");
    terminal.keys("test\r");
    terminal.wait_for(&["Reasoning effort"]);
    terminal.keys("high\r");
    terminal.wait_for(&["test-model high"]);
    terminal.keys("hello\r");
    terminal.wait_for(&["connected"]);
    terminal.quit();

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["body"]["model"], "test-model");
    assert_eq!(requests[0]["body"]["reasoning"]["effort"], "high");
    let agt_home = home.path().join(".agt");
    let read = |name: &str| -> Value {
        serde_json::from_slice(&fs::read(agt_home.join(name)).expect("saved")).expect("JSON")
    };
    let settings = read("config.json");
    assert_eq!(settings["model"]["provider"], "openrouter");
    assert_eq!(settings["model"]["id"], "test-model");
    assert_eq!(settings["model"]["window"], 64000);
    assert_eq!(settings["model"]["efforts"], json!(["low", "high"]));
    assert_eq!(settings["reasoning"], "high");
    assert_eq!(read("auth.json"), json!({ "openrouter": "secret-key" }));
}

#[test]
fn commands_switch_the_model_provider_and_effort_mid_session() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(vec![text("switched"), text("on grok")]);
    let mut terminal = Terminal::spawn(&agt(home.path(), &server));
    terminal.wait_for(&["test-model", "/ 200k"]);
    terminal.keys("/mo");
    terminal.wait_for(&["/model", "choose a model"]);
    terminal.keys("\r");
    // AGT_API_KEY is a credential for every provider, so each has a tab.
    terminal.wait_for(&["OpenAI", "Vercel AI Gateway", "other-model", "tab provider"]);
    terminal.keys("other\r");
    terminal.wait_for(&["using other-model on OpenRouter"]);
    terminal.keys("\x1b[Z");
    terminal.wait_for(&["other-model none"]);
    terminal.keys("hi\r");
    terminal.wait_for(&["switched"]);
    // In the menu Shift-Tab shows the tab before, and a model there switches
    // the provider without signing in again.
    terminal.keys("\x0c");
    terminal.wait_for(&["tab provider"]);
    terminal.keys("\x1b[Z");
    terminal.wait_for(&["grok-4.6"]);
    terminal.keys("4.6\r");
    terminal.wait_for(&["using grok-4.6 on Grok"]);
    terminal.keys("again\r");
    terminal.wait_for(&["on grok"]);
    terminal.quit();

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["body"]["model"], "other-model");
    assert_eq!(requests[0]["body"]["reasoning"]["effort"], "none");
    assert_eq!(requests[1]["body"]["model"], "grok-4.6");
    let headers = &requests[1]["headers"];
    assert!(headers.get("x-grok-conv-id").is_some(), "not sent as Grok: {headers}");
    let settings: Value =
        serde_json::from_slice(&fs::read(home.path().join(".agt/config.json")).expect("saved"))
            .expect("JSON");
    assert_eq!(settings["model"]["provider"], "grok");
    assert_eq!(settings["model"]["id"], "grok-4.6");
    assert_eq!(settings["model"]["window"], 500000);
    assert_eq!(settings["reasoning"], Value::Null, "grok-4.6 takes no effort named none");
}

#[test]
fn the_prompt_can_still_be_edited_after_the_terminal_shrinks() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(vec![text("received")]);
    let mut terminal = Terminal::spawn(&agt(home.path(), &server));
    terminal.wait_for(&["Ask anything"]);
    terminal.resize(10, 44);
    let pasted: String = (0..10).map(|n| format!("row {n}\n")).collect();
    terminal.keys(&format!("\x1b[200~{pasted}\x1b[201~"));
    terminal.wait_for(&["row 9", "╰─"]);
    // Move to the beginning without entering history, then edit the hidden rows.
    terminal.keys(&"\x1b[A".repeat(10));
    terminal.keys("START ");
    let screen = terminal.wait_for(&["START row 0", "╰─"]);
    let (row, col) = terminal.screen.screen().cursor_position();
    assert!(row < 9 && col < 44, "cursor {row},{col}; screen:\n{screen}");
    terminal.resize(ROWS, COLS);
    terminal.keys("\r");
    terminal.wait_for(&["received"]);
    terminal.quit();
    assert_eq!(
        server.requests()[0]["body"]["input"][0]["content"][0]["text"],
        format!("START {pasted}")
    );
}

#[test]
fn background_processes_can_be_inspected_and_stopped_during_a_turn() {
    let home = tempfile::tempdir().expect("temp dir");
    let mut slow = text("turn finished");
    slow.insert(0, json!({ "delay_ms": 5000 }));
    let background = json!({ "command": "echo background-ready; sleep 30", "wait": 0.05 });
    let server = Server::start(vec![call("background", "bash", background, 100), slow]);
    let mut terminal = Terminal::spawn(&agt(home.path(), &server));
    terminal.wait_for(&["Ask anything"]);
    terminal.keys("start a task\r");
    // The second request is still being answered.
    terminal.wait_for(&["Thinking", "1 running"]);
    // A click on the count of running processes lists them.
    terminal.click("1 running");
    terminal.wait_for(&["Processes", "echo background-ready", "running", "ctrl-x stop"]);
    terminal.keys("\r");
    terminal.wait_for(&["Process 1 · running", "Log:", "background-ready"]);
    terminal.keys("/tasks\r");
    terminal.wait_for(&["Processes", "ctrl-x stop"]);
    terminal.keys("\x18");
    terminal.wait_for(&["the user stopped process 1"]);
    terminal.keys("\x1b");
    terminal.wait_for(&["turn finished"]);
    thread::sleep(Duration::from_secs(1));
    terminal.quit();
    let requests = server.requests();
    let tails: Vec<&Value> =
        requests.iter().filter_map(|request| request["body"]["input"].as_array()?.last()).collect();
    assert_eq!(
        requests.len(),
        2,
        "inspecting sends nothing, and the stop is told with the next request: {tails:#?}"
    );
}

#[test]
fn login_runs_setup_and_exits_with_the_choice() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(Vec::new());
    let mut command = agt(home.path(), &server);
    for name in ["AGT_PROVIDER", "AGT_MODEL"] {
        command.env_remove(name);
    }
    command.arg("login");
    let mut terminal = Terminal::spawn(&command);
    terminal.wait_for(&["Connect a provider"]);
    terminal.keys("openrouter\r");
    terminal.wait_for(&["Sign in to OpenRouter", "Use the key found"]);
    terminal.keys("use\r");
    terminal.wait_for(&["Model · OpenRouter", "test-model"]);
    terminal.keys("test\r");
    terminal.wait_for(&["Reasoning effort"]);
    terminal.keys("high\r");
    terminal.wait_for(&["using test-model on OpenRouter"]);
    assert!(terminal.child.wait().expect("agt exits").success());
    assert!(!home.path().join(".agt/sessions").exists(), "setup starts no session");
    assert!(server.requests().is_empty());
}
