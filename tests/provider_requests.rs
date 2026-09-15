//! What each provider receives and what a session logs, byte for byte.

mod support;

use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use support::{Server, agt, call, golden, text};

/// A response that reasons, then runs a command and views an image at once.
fn calls() -> Vec<Value> {
    let mut response = call("call_1", "bash", json!({ "command": "echo hi" }), 100);
    let body = &mut response[0]["response"];
    body["usage"]["cost"] = json!(0.001);
    let output = body["output"].as_array_mut().expect("output");
    output[0]["id"] = json!("fc_1");
    output.insert(
        0,
        json!({ "type": "reasoning", "id": "rs_1", "encrypted_content": "sealed", "summary": [] }),
    );
    output.push(json!({
        "type": "function_call", "id": "fc_2", "call_id": "call_2", "name": "bash",
        "arguments": json!({ "command": "agt view shot.png" }).to_string(),
    }));
    response
}

/// The log of the only session in `home`.
fn log(home: &Path) -> String {
    let sessions: Vec<_> = fs::read_dir(home.join(".agt/sessions")).expect("sessions").collect();
    assert_eq!(sessions.len(), 1);
    let dir = sessions[0].as_ref().expect("entry").path();
    fs::read_to_string(dir.join("log.jsonl")).expect("log")
}

#[test]
fn each_provider_receives_its_documented_request() {
    for provider in ["openai", "codex", "grok", "openrouter", "vercel"] {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dir.path();
        image::RgbImage::from_pixel(4, 2, image::Rgb([200, 40, 40]))
            .save(home.join("shot.png"))
            .expect("image");
        fs::write(home.join("AGENTS.md"), "Run the tests before finishing.").expect("AGENTS.md");
        let skill = home.join(".agents/skills/release");
        fs::create_dir_all(&skill).expect("skill directory");
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: release\ndescription: Cut a release.\n---\nTag it.",
        )
        .expect("SKILL.md");
        let server = Server::start(vec![calls(), text("done"), text("again")]);
        for args in [["-p", "look"].as_slice(), &["-p", "-c", "again"]] {
            let output = agt(home, &server)
                .env("AGT_PROVIDER", provider)
                .args(args)
                .output()
                .expect("agt runs");
            assert!(
                output.status.success(),
                "{provider}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(server.requests().len(), 3, "{provider}");
        golden(&format!("requests-{provider}.json"), home, &server.exchanges());
        if provider == "openai" {
            golden("session-log.jsonl", home, &log(home));
        }
    }
}
