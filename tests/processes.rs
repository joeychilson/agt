//! Processes end to end: commands left running, waiting for them, their text
//! logs, and the exits of background processes reaching the model.

mod support;

use std::fs;
use std::time::{Duration, Instant};

use serde_json::json;
use support::{Server, agt, call, golden, text};

#[test]
fn a_wait_ends_when_a_background_process_exits_and_its_log_is_text() {
    let home = tempfile::tempdir().expect("temp dir");
    let command = r"printf 'step 1/2\rstep 2/2\n'; sleep 1; printf '\033[32mgreen\033[0m\n'";
    let server = Server::start(vec![
        call("call_1", "bash", json!({ "command": command, "wait": 0 }), 100),
        call("call_2", "bash", json!({ "wait": 60 }), 100),
        text("finished"),
    ]);
    let started = Instant::now();
    let output = agt(home.path(), &server).args(["-p", "run it"]).output().expect("agt runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(started.elapsed() < Duration::from_secs(20), "the wait outlasted the exit");
    let requests = server.requests();
    let input = requests[2]["body"]["input"].as_array().expect("input");
    let waited = input.last().and_then(|item| item["output"].as_str()).expect("the wait's result");
    assert!(waited.starts_with("[waited "), "{waited}");
    assert!(waited.contains(" UTC] process 1 (printf") && waited.contains("finished with exit 0"));
    // The log holds what a terminal shows: the overwritten line as it ended,
    // and no escape sequences.
    let sessions: Vec<_> =
        fs::read_dir(home.path().join(".agt/sessions")).expect("sessions").collect();
    let dir = sessions[0].as_ref().expect("entry").path();
    assert_eq!(fs::read_to_string(dir.join("procs/1.log")).expect("log"), "step 2/2\ngreen\n");
}

#[test]
fn a_background_exit_reaches_the_model_with_its_output_images_and_log() {
    let home = tempfile::tempdir().expect("temp dir");
    image::RgbImage::new(4, 2).save(home.path().join("shot.png")).expect("image");
    let command = "sleep 0.3; echo background-done; agt view shot.png";
    let server = Server::start(vec![
        call("call_1", "bash", json!({ "command": command, "wait": 0 }), 100),
        call("call_2", "bash", json!({ "command": "sleep 1.5", "wait": 10 }), 100),
        text("finished"),
    ]);
    let output = agt(home.path(), &server).args(["-p", "run both"]).output().expect("agt runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    // The first command is left running, and its exit notice follows the
    // second command's result with the log, the output it printed and the
    // image it showed.
    golden("background-exit.json", home.path(), &server.exchanges());
}
