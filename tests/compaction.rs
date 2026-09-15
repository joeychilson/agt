//! Compaction end to end: providers' own compaction, inline and through its
//! endpoint, summaries on top of the cached prompt, the serialized fallback,
//! compaction between turns, and resuming from a compaction record.

mod support;

use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use support::{Server, agt, call, golden, text};

/// Runs one prompt on `provider` with a window small enough that the first
/// tool result forces compaction.
fn run(provider: &str, script: Vec<Vec<Value>>) -> (Server, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(script);
    let output = agt(home.path(), &server)
        .env("AGT_PROVIDER", provider)
        .env("AGT_CONTEXT_WINDOW", "8000")
        .args(["-p", "count things"])
        .output()
        .expect("agt runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "finished\n");
    (server, home)
}

/// Continues the latest session in `home` on `provider` with another prompt.
fn resume(home: &Path, server: &Server, provider: &str) {
    let output = agt(home, server)
        .env("AGT_PROVIDER", provider)
        .env("AGT_CONTEXT_WINDOW", "8000")
        .args(["-p", "-c", "again"])
        .output()
        .expect("agt runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

/// The log of the only session in `home`.
fn log(home: &Path) -> String {
    let sessions: Vec<_> = fs::read_dir(home.join(".agt/sessions")).expect("sessions").collect();
    assert_eq!(sessions.len(), 1);
    let dir = sessions[0].as_ref().expect("entry").path();
    fs::read_to_string(dir.join("log.jsonl")).expect("log")
}

#[test]
fn openai_compacts_inline_and_requests_continue_from_its_compaction() {
    // The provider compacts while it answers, returning the compaction item
    // with the reply.
    let compacted = vec![
        json!({ "type": "response.output_text.delta", "delta": "finished" }),
        json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [
                    { "type": "compaction", "id": "cmp_1", "encrypted_content": "opaque" },
                    { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "finished" }] },
                ],
                "usage": { "input_tokens": 7000, "output_tokens": 300 },
            },
        }),
    ];
    let (server, home) = run(
        "openai",
        vec![
            call("call_1", "bash", json!({ "command": "seq 1 4000" }), 7000),
            compacted,
            text("resumed"),
        ],
    );
    resume(home.path(), &server, "openai");
    // Every request asks the provider to compact past a threshold, lower at
    // the start of a turn, and the request after a compaction, in a resumed
    // process too, drops everything before the compaction item.
    golden("compaction-inline.json", home.path(), &server.exchanges());
    golden("compaction-inline.jsonl", home.path(), &log(home.path()));
}

#[test]
fn endpoint_compaction_continues_from_the_context_it_returns() {
    let compaction = json!({
        "object": "response.compaction",
        "output": [
            { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "count things" }] },
            { "type": "compaction", "id": "cmp_1", "encrypted_content": "opaque" },
        ],
        "usage": { "input_tokens": 7000, "output_tokens": 300 },
    });
    let (server, home) = run(
        "grok",
        vec![
            call("call_1", "bash", json!({ "command": "seq 1 4000" }), 7000),
            vec![compaction],
            text("finished"),
            text("resumed"),
        ],
    );
    resume(home.path(), &server, "grok");
    // The compaction request carries the session's instructions, tools and
    // history, and the requests after it, in this process and a resumed one,
    // start from the compacted context exactly as it was returned.
    golden("compaction-endpoint.json", home.path(), &server.exchanges());
    golden("compaction-endpoint.jsonl", home.path(), &log(home.path()));
}

#[test]
fn summaries_are_requested_on_top_of_the_cached_prompt() {
    // The first response reports usage near the window, so the tool output
    // that follows pushes the context past the compaction threshold.
    let (server, home) = run(
        "openrouter",
        vec![
            call("call_1", "bash", json!({ "command": "seq 1 4000" }), 7000),
            text("## Goal\nkeep counting"),
            text("finished"),
        ],
    );
    // The summary request is the session's own request with a closing message
    // and no tool calls; the next request starts from a checkpoint holding the
    // user's message and the summary, followed by the recent tool result.
    golden("compaction-cached.json", home.path(), &server.exchanges());
    golden("compaction-cached.jsonl", home.path(), &log(home.path()));
}

#[test]
fn a_summary_that_calls_tools_falls_back_to_a_serialized_request() {
    let (server, home) = run(
        "openrouter",
        vec![
            call("call_1", "bash", json!({ "command": "seq 1 4000" }), 7000),
            call("call_2", "bash", json!({ "command": "true" }), 100),
            text("## Goal\nkeep counting"),
            text("finished"),
        ],
    );
    // The fallback sends the conversation as text, without the session's tools.
    golden("compaction-serialized.json", home.path(), &server.exchanges());
    golden("compaction-serialized.jsonl", home.path(), &log(home.path()));
}

#[test]
fn a_nearly_full_context_compacts_before_the_next_user_turn() {
    let home = tempfile::tempdir().expect("temp dir");
    let server = Server::start(vec![text("read"), text("## Goal\nkeep reading"), text("finished")]);
    // With a 40k window, requests compact above 24k tokens, and user turns
    // above 18k. The paste leaves about 22k tokens of context, so only the
    // next turn compacts, before its message is sent.
    let send = |args: &[&str]| {
        let output = agt(home.path(), &server)
            .env("AGT_CONTEXT_WINDOW", "40000")
            .args(args)
            .output()
            .expect("agt runs");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let paste = "lorem ipsum ".repeat(7000);
    assert_eq!(send(&["-p", &paste]), "read\n");
    assert_eq!(send(&["-p", "-c", "next"]), "finished\n");
    golden("compaction-between-turns.json", home.path(), &server.exchanges());
    golden("compaction-between-turns.jsonl", home.path(), &log(home.path()));
}
