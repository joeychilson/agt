//! Print mode end to end: what reaches stdout and stderr, and how a run ends
//! when something goes wrong.

mod support;

use serde_json::json;
use support::{Server, agt, call, golden, text};

#[test]
fn the_reply_goes_to_stdout_and_the_work_with_its_logs_to_stderr() {
    let home = tempfile::tempdir().expect("temp dir");
    let failing = "for n in 1 2 3 4 5 6; do echo line $n; done; exit 2";
    let server = Server::start(vec![
        call("call_1", "bash", json!({ "command": failing }), 100),
        call("call_2", "bash", json!({ "command": "echo fine" }), 100),
        text("all done"),
    ]);
    let output = agt(home.path(), &server).args(["-p", "say hi"]).output().expect("agt runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "all done\n");
    golden("print-transcript.txt", home.path(), &stderr);
    assert_eq!(server.requests().len(), 3);
}

#[test]
fn provider_errors_fail_the_run() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(Vec::new());
    let output = agt(home.path(), &server).args(["-p", "hello"]).output().expect("agt runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("HTTP 400: unscripted request"), "{stderr}");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn completed_text_is_printed_when_the_provider_omits_deltas() {
    let home = tempfile::tempdir().expect("temp dir");
    let mut response = text("complete answer");
    response.remove(0);
    let server = Server::start(vec![response]);
    let output = agt(home.path(), &server).args(["-p", "hello"]).output().expect("agt runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "complete answer\n");
}

#[test]
fn a_dropped_stream_never_leaks_its_draft_into_printed_output() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(vec![
        vec![json!({"type":"response.output_text.delta","delta":"discard this draft\n"})],
        text("the completed answer"),
    ]);
    let output = agt(home.path(), &server).args(["-p", "hello"]).output().expect("agt runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "the completed answer\n");
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn commands_do_not_run_once_the_session_log_cannot_be_written() {
    let home = tempfile::tempdir().expect("temp dir");
    let marker = home.path().join("ran");
    // The call's log record is larger than the file size limit leaves room for.
    let command = format!("touch {} # {}", marker.display(), "x".repeat(2000));
    let server = Server::start(vec![call("call_1", "bash", json!({ "command": command }), 100)]);
    let template = agt(home.path(), &server);
    let mut limited = std::process::Command::new("sh");
    limited
        .args(["-c", "trap '' XFSZ; ulimit -f 2; exec \"$0\" -p 'run it'"])
        .arg(template.get_program())
        .current_dir(home.path());
    for (key, value) in template.get_envs() {
        match value {
            Some(value) => limited.env(key, value),
            None => limited.env_remove(key),
        };
    }
    let output = limited.output().expect("agt runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("cannot write the session log"), "{stderr}");
    assert!(!marker.exists(), "the command ran");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn images_a_provider_rejects_are_left_out_and_the_turn_goes_on() {
    let home = tempfile::tempdir().expect("temp dir");
    image::RgbImage::new(16, 16).save(home.path().join("shot.png")).expect("image");
    let view = call("view_1", "bash", json!({ "command": "agt view shot.png" }), 100);
    let rejection =
        vec![json!({ "status": 400, "error": { "message": "Invalid image: unsupported" } })];
    let server = Server::start(vec![view, rejection, text("described without it")]);
    let output =
        agt(home.path(), &server).args(["-p", "look at shot.png"]).output().expect("agt runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("rejected an image"), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "described without it\n");
    // The request after the view carries the image, and the retry a note in its place.
    golden("image-rejected.json", home.path(), &server.exchanges());
}
