//! The bytes of a request, shaped for its provider's dialect.
//!
//! The input is spliced into the body item by item without cloning it, apart
//! from the few items changed on the way: images read in from their saved
//! files, reasoning and compaction items another model made, and the ids that
//! reasoning paired with.

use std::mem;

use serde_json::{Map, Value, json};

use super::Request;
use crate::image;
use crate::item::{Item, Kind, input_text};
use crate::provider::{Caching, Dialect};

/// Stands in for a compaction item the model cannot read, so it knows earlier
/// work happened and where to find it.
const UNREADABLE_COMPACTION: &str = "[Earlier work in this conversation was compacted into a form this model cannot read. log.jsonl in the session directory holds the full transcript.]";
/// Labels the images of tool outputs sent after them in a user message.
const TOOL_IMAGES: &str = "Images from the tool results above, in order:";

/// The body of `request` in `dialect`. A compaction request carries the
/// conversation and the settings that shape it, without the fields of a
/// streamed response.
pub(super) fn build(request: &Request<'_>, dialect: &Dialect, streaming: bool) -> Vec<u8> {
    let mut head = Map::new();
    head.insert("model".into(), request.model.into());
    head.insert("instructions".into(), request.instructions.into());
    if !request.tools.is_empty() {
        head.insert("tools".into(), request.tools.into());
    }
    let mut reasoning = json!({ "summary": "auto" });
    if let Some(effort) = request.reasoning {
        reasoning["effort"] = effort.as_str().into();
    }
    head.insert("reasoning".into(), reasoning);
    if dialect.cache_key {
        head.insert("prompt_cache_key".into(), request.cache_key.into());
    }
    if streaming {
        head.insert("stream".into(), true.into());
        head.insert("store".into(), false.into());
        // Stateless requests replay reasoning as encrypted content.
        head.insert("include".into(), json!(["reasoning.encrypted_content"]));
        if let Some(choice) = request.tool_choice {
            head.insert("tool_choice".into(), choice.into());
        }
        if let Some(limit) = request.max_output_tokens
            && dialect.output_limit
        {
            head.insert("max_output_tokens".into(), limit.into());
        }
        if let Some(threshold) = request.compact_at {
            head.insert(
                "context_management".into(),
                json!([{ "type": "compaction", "compact_threshold": threshold }]),
            );
        }
        // An hour outlives long commands and pauses between turns.
        match dialect.caching {
            Caching::Implicit => {}
            Caching::CacheControl => {
                head.insert("cache_control".into(), json!({ "type": "ephemeral", "ttl": "1h" }));
            }
            Caching::Gateway => {
                head.insert("caching".into(), "auto".into());
                head.insert("cache_ttl".into(), "1h".into());
            }
        }
    }
    let mut body = serde_json::to_vec(&Value::Object(head)).expect("JSON values always serialize");
    body.pop();
    body.extend_from_slice(br#","input":["#);
    let items =
        request.input.iter().enumerate().filter(|(index, item)| {
            *index >= request.reasoning_from || item.kind() != Kind::Reasoning
        });
    let separate = !dialect.tool_images;
    let mut images = Vec::new();
    for (index, item) in items {
        if !images.is_empty() && item.kind() != Kind::Output {
            append(&mut body, &tool_images(mem::take(&mut images)));
        }
        let replayed = index >= request.reasoning_from;
        match item.kind() {
            // Only the model that compacted a conversation can read it.
            Kind::Compaction if !replayed => {
                append(&mut body, &Item::user(vec![input_text(UNREADABLE_COMPACTION)]));
            }
            // OpenAI pairs output item ids with the reasoning that produced
            // them; with that reasoning left out, the ids must go too.
            Kind::Call | Kind::Assistant if !replayed && !item.get("id").is_null() => {
                append(&mut body, &item.without_id());
            }
            _ if item.images().next().is_some() => {
                let mut item = image::inline(item, request.images);
                if separate && item.kind() == Kind::Output {
                    images.extend(item.content_mut().map(take_images).unwrap_or_default());
                }
                append(&mut body, &item);
            }
            _ => append(&mut body, item),
        }
    }
    if !images.is_empty() {
        append(&mut body, &tool_images(images));
    }
    body.extend_from_slice(b"]}");
    body
}

/// Writes `item` into the input array of a request body.
fn append(body: &mut Vec<u8>, item: &Item) {
    if body.last() != Some(&b'[') {
        body.push(b',');
    }
    serde_json::to_writer(body, item).expect("JSON values always serialize");
}

/// Takes the images out of a tool output's parts, leaving it their text.
fn take_images(output: &mut Value) -> Vec<Value> {
    let Some(parts) = output.as_array_mut() else {
        return Vec::new();
    };
    let (images, text): (Vec<Value>, Vec<Value>) =
        parts.drain(..).partition(|part| part["type"] == "input_image");
    if images.is_empty() {
        *parts = text;
    } else {
        let text: String = text.iter().filter_map(|part| part["text"].as_str()).collect();
        *output = text.into();
    }
    images
}

/// The user message that carries the images of a run of tool outputs.
fn tool_images(images: Vec<Value>) -> Item {
    let mut content = vec![input_text(TOOL_IMAGES)];
    content.extend(images);
    Item::user(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;

    fn item(value: Value) -> Item {
        Item::from_value(value).expect("an object")
    }

    fn request<'a>(input: &'a [Item], tools: &'a [Value]) -> Request<'a> {
        Request {
            model: "m",
            reasoning: None,
            instructions: "be brief",
            input,
            reasoning_from: 0,
            tools,
            tool_choice: None,
            cache_key: "s1",
            max_output_tokens: Some(2000),
            compact_at: None,
            images: None,
        }
    }

    fn body(provider: Provider, request: &Request<'_>, streaming: bool) -> Value {
        serde_json::from_slice(&build(request, &provider.spec().dialect, streaming))
            .expect("body is JSON")
    }

    #[test]
    fn compaction_requests_leave_out_the_fields_of_streamed_responses() {
        let input = [Item::user(vec![input_text("hi")])];
        let tools = [json!({ "type": "function", "name": "bash" })];
        let request = Request {
            tool_choice: Some("none"),
            compact_at: Some(1000),
            ..request(&input, &tools)
        };
        let body = body(Provider::Vercel, &request, false);
        assert_eq!(body["input"][0]["content"][0]["text"], "hi");
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["instructions"], "be brief");
        assert_eq!(body["prompt_cache_key"], "s1");
        for field in [
            "stream",
            "store",
            "include",
            "tool_choice",
            "max_output_tokens",
            "caching",
            "context_management",
        ] {
            assert!(body.get(field).is_none(), "{field}");
        }
    }

    #[test]
    fn inline_compaction_is_asked_for_with_its_threshold() {
        let request = Request { compact_at: Some(258_400), ..request(&[], &[]) };
        let body = body(Provider::OpenAi, &request, true);
        assert_eq!(
            body["context_management"],
            json!([{ "type": "compaction", "compact_threshold": 258_400 }])
        );
    }

    #[test]
    fn earlier_models_reasoning_is_not_replayed() {
        let input = [
            item(json!({ "type": "reasoning", "id": "old" })),
            item(json!({ "type": "compaction", "id": "cmp_1" })),
            item(json!({ "type": "function_call", "id": "fc_1", "call_id": "c1" })),
            Item::user(vec![input_text("hi")]),
            item(json!({ "type": "reasoning", "id": "new" })),
        ];
        let request = Request { reasoning_from: 4, ..request(&input, &[]) };
        let body = body(Provider::OpenAi, &request, true);
        let replayed = json!([
            Item::user(vec![input_text(UNREADABLE_COMPACTION)]),
            { "type": "function_call", "call_id": "c1" },
            Item::user(vec![input_text("hi")]),
            { "type": "reasoning", "id": "new" },
        ]);
        assert_eq!(body["input"], replayed);
    }

    #[test]
    fn chatgpt_requests_carry_no_output_limit() {
        assert_eq!(body(Provider::OpenAi, &request(&[], &[]), true)["max_output_tokens"], 2000);
        let codex = body(Provider::Codex, &request(&[], &[]), true);
        assert!(codex.get("max_output_tokens").is_none(), "{codex}");
    }

    #[test]
    fn gateways_get_the_images_of_tool_outputs_in_a_message_after_them() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut png = Vec::new();
        ::image::RgbImage::new(4, 2)
            .write_to(&mut std::io::Cursor::new(&mut png), ::image::ImageFormat::Png)
            .expect("png");
        let parts = image::attach(&png, dir.path()).expect("attached");
        let input = [
            item(
                json!({ "type": "function_call", "call_id": "c1", "name": "bash", "arguments": "{}" }),
            ),
            Item::output("c1", Value::Array(parts)),
        ];
        let request = Request { images: Some(dir.path()), ..request(&input, &[]) };
        let inside = body(Provider::OpenAi, &request, true);
        assert_eq!(inside["input"][1]["output"][1]["type"], "input_image");
        let after = body(Provider::OpenRouter, &request, true);
        assert!(after["input"][1]["output"].is_string(), "{}", after["input"][1]);
        assert_eq!(after["input"][2]["content"][0]["text"], TOOL_IMAGES);
        assert_eq!(after["input"][2]["content"][1]["type"], "input_image");
    }
}
