//! The commands end to end: listing, following and steering sessions, models,
//! and sign-in, as they print for people and for agents reading their output.

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;

use serde_json::json;
use support::{Server, agt, call, golden, text};

#[test]
fn a_running_session_takes_messages_and_can_be_followed_until_it_is_done() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(vec![
        call("call_1", "bash", json!({ "command": "sleep 2" }), 100),
        text("done"),
    ]);
    let mut running = agt(home.path(), &server)
        .args(["-p", "work a while"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("agt runs");
    let mut first = String::new();
    let mut stderr = BufReader::new(running.stderr.take().expect("stderr"));
    stderr.read_line(&mut first).expect("the first line");
    let id = first
        .strip_prefix("agt: session ")
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_else(|| panic!("the first line names the session: {first}"))
        .to_owned();
    let run = |args: &[&str]| agt(home.path(), &server).args(args).output().expect("agt runs");

    let sent = run(&["send", &id, "also check the docs"]);
    assert_eq!(
        String::from_utf8_lossy(&sent.stdout),
        format!("sent to session {id} for its next step\n"),
        "{}",
        String::from_utf8_lossy(&sent.stderr)
    );
    let resumed = run(&["-p", "-r", &id, "take over"]);
    assert_eq!(
        (resumed.status.code(), String::from_utf8_lossy(&resumed.stderr).into_owned()),
        (
            Some(1),
            format!(
                "agt: session {id} is running in another agt; agt send {id} '<message>' reaches it\n"
            )
        ),
        "a running session is not resumed twice"
    );
    let followed = run(&["sessions", "show", "-f", &id]);
    let transcript = String::from_utf8_lossy(&followed.stdout);
    assert!(
        transcript.contains("❯ [agt send] also check the docs") && transcript.ends_with("\ndone\n"),
        "following ends with the turn: {transcript}"
    );
    let output = running.wait_with_output().expect("agt ends");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "done\n");
    let input = server.requests()[1]["body"]["input"].to_string();
    assert!(input.contains("also check the docs"), "the message reaches the model: {input}");

    let ended = run(&["send", &id, "still there?"]);
    assert_eq!(ended.status.code(), Some(1), "a session that ended takes no messages");
}

#[test]
fn keys_are_saved_and_forgotten_and_models_are_chosen() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(Vec::new());
    let run = |args: &[&str], stdin: &str| {
        let mut child = agt(home.path(), &server)
            .args(args)
            .stdin(if stdin.is_empty() { Stdio::null() } else { Stdio::piped() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("agt runs");
        if let Some(mut input) = child.stdin.take() {
            input.write_all(stdin.as_bytes()).expect("stdin");
        }
        let output = child.wait_with_output().expect("agt ends");
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        (output.status.code(), text(&output.stdout), text(&output.stderr))
    };
    let auth = || fs::read_to_string(home.path().join(".agt/auth.json")).unwrap_or_default();

    let (code, stdout, stderr) = run(&["login", "openrouter", "--key", "-"], "sk-or-test\n");
    assert_eq!(
        (code, stdout.as_str()),
        (Some(0), "signed in to OpenRouter; agt models lists its models\n"),
        "{stderr}"
    );
    assert!(auth().contains("sk-or-test"), "the key is saved");
    let (code, stdout, stderr) = run(&["logout", "openrouter"], "");
    assert_eq!((code, stdout.as_str()), (Some(0), "signed out of OpenRouter\n"), "{stderr}");
    assert!(!auth().contains("sk-or-test"), "the key is forgotten");
    let (code, _, stderr) = run(&["logout", "openrouter"], "");
    assert_eq!(
        (code, stderr.as_str()),
        (Some(1), "agt logout: no sign-in or key for OpenRouter is saved\n")
    );

    let (code, stdout, stderr) =
        run(&["models", "use", "test-model", "--provider", "openrouter", "-e", "high"], "");
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        stdout.starts_with("using test-model at high effort on OpenRouter from now on\n"),
        "{stdout}"
    );
    let settings = fs::read_to_string(home.path().join(".agt/config.json")).expect("settings");
    assert!(settings.contains("test-model") && settings.contains("high"), "{settings}");
    let (code, _, stderr) = run(&["models", "use", "test-model", "-e", "max"], "");
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("test-model takes no max effort"), "{stderr}");
}

#[test]
fn sessions_are_listed_by_directory_and_shown_as_transcripts() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(vec![
        call("call_1", "bash", json!({ "command": "echo from-tool; exit 3" }), 100),
        text("it failed"),
    ]);
    let run = |args: &[&str]| agt(home.path(), &server).args(args).output().expect("agt runs");
    let turn = run(&["-p", "run it"]);
    let stderr = String::from_utf8_lossy(&turn.stderr);
    assert!(turn.status.success(), "{stderr}");
    let id = stderr
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("agt: session "))
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_else(|| panic!("the first line names the session: {stderr}"));

    let listed = run(&["sessions"]);
    assert_eq!(String::from_utf8_lossy(&listed.stdout), format!("{id}       now  run it\n"));
    let elsewhere = home.path().join("elsewhere");
    fs::create_dir(&elsewhere).expect("directory");
    let command = agt(home.path(), &server).current_dir(&elsewhere).args(["sessions"]).output();
    let stdout = String::from_utf8_lossy(&command.expect("agt runs").stdout).into_owned();
    assert!(stdout.starts_with("no sessions in "), "{stdout}");
    let all =
        agt(home.path(), &server).current_dir(&elsewhere).args(["sessions", "--all"]).output();
    let stdout = String::from_utf8_lossy(&all.expect("agt runs").stdout).into_owned();
    assert!(
        stdout.starts_with(&format!("{id}       now  ")) && stdout.ends_with("  run it\n"),
        "{stdout}"
    );

    let shown = run(&["sessions", "show", id]);
    assert!(shown.status.success(), "{}", String::from_utf8_lossy(&shown.stderr));
    golden("sessions-show.txt", home.path(), &String::from_utf8_lossy(&shown.stdout));
    let missing = run(&["sessions", "show", "1-a"]);
    assert_eq!(
        (missing.status.code(), String::from_utf8_lossy(&missing.stderr).into_owned()),
        (Some(1), "agt sessions: there is no session 1-a\n".to_owned())
    );
    assert_eq!(server.requests().len(), 2, "reading sessions asks the model nothing");
}

#[test]
fn models_are_listed_with_their_prices_efforts_and_the_one_in_use() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(Vec::new());
    let output = agt(home.path(), &server).args(["models", "model"]).output().expect("agt runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    golden("models.txt", home.path(), &String::from_utf8_lossy(&output.stdout));
    assert_eq!(server.exchanges(), "[]", "listing models asks the model nothing");
}
