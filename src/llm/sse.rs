//! Reading a streamed response: server-sent events in, text and reasoning
//! deltas out, and the completed response at the terminal event.

use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{Event, Failure, Response, Usage, classify_event, retry};
use crate::item::{Item, Kind};

/// How often a stream that shows nothing still reports that it is alive.
const ALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// Incremental server-sent events parser for one response.
pub(super) struct Parser<'a> {
    on: &'a dyn Fn(Event),
    /// Items from `output_item.done` events, for a terminal event without output.
    items: Vec<Value>,
    alive: Instant,
}

impl<'a> Parser<'a> {
    pub(super) fn new(on: &'a dyn Fn(Event)) -> Self {
        Self { on, items: Vec::new(), alive: Instant::now() }
    }

    pub(super) fn read(
        mut self,
        mut reader: impl BufRead,
        cancel: &AtomicBool,
    ) -> Result<Response, Failure> {
        let mut line = String::new();
        let mut data = String::new();
        loop {
            line.clear();
            let read = reader.read_line(&mut line);
            if cancel.load(Ordering::Relaxed) {
                return Err(Failure::Cancelled);
            }
            match read {
                Ok(0) => {
                    // Some servers close right after the last event, without its blank line.
                    if !data.is_empty()
                        && let Some(response) = self.dispatch(&data)?
                    {
                        return Ok(response);
                    }
                    return Err(retry("stream ended before the response completed".into()));
                }
                Ok(_) => {}
                Err(error) => return Err(retry(error.to_string())),
            }
            if self.alive.elapsed() >= ALIVE_INTERVAL {
                self.alive = Instant::now();
                (self.on)(Event::Alive);
            }
            let field = line.trim_end_matches(['\n', '\r']);
            if field.is_empty() {
                if data.is_empty() {
                    continue;
                }
                let done = self.dispatch(&data)?;
                data.clear();
                if let Some(response) = done {
                    return Ok(response);
                }
            } else if let Some(value) = field.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
            }
        }
    }

    fn dispatch(&mut self, data: &str) -> Result<Option<Response>, Failure> {
        if data == "[DONE]" {
            return Err(retry("stream ended before the response completed".into()));
        }
        // An unreadable event is skipped rather than failing the attempt: the
        // terminal event carries the whole output.
        let Ok(mut event) = serde_json::from_str::<Value>(data) else {
            return Ok(None);
        };
        match event["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" | "response.refusal.delta" => {
                self.delta(&event, Event::Text);
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.delta(&event, Event::Thinking);
            }
            "response.reasoning_summary_part.added"
                if event["summary_index"].as_u64().is_some_and(|index| index > 0) =>
            {
                (self.on)(Event::Thinking("\n\n".into()));
            }
            // A model that keeps its reasoning hidden still shows that it began.
            "response.output_item.added" if event["item"]["type"] == "reasoning" => {
                (self.on)(Event::Thinking(String::new()));
            }
            "response.output_item.done" => {
                if let Some(item) = event.get_mut("item") {
                    self.items.push(item.take());
                }
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                let response = event.get_mut("response").map(Value::take).unwrap_or_default();
                return finish(response, std::mem::take(&mut self.items)).map(Some);
            }
            "response.failed" => return Err(classify_event(&event["response"]["error"])),
            "error" => return Err(classify_event(event.get("error").unwrap_or(&event))),
            _ => {}
        }
        Ok(None)
    }

    fn delta(&self, event: &Value, make: fn(String) -> Event) {
        if let Some(delta) = event["delta"].as_str()
            && !delta.is_empty()
        {
            (self.on)(make(delta.to_owned()));
        }
    }
}

/// Builds the response from a terminal event, or a compaction's whole
/// response. The event's `output` is authoritative; `items` collected from
/// `output_item.done` events cover providers that omit it or send it empty.
pub(super) fn finish(mut response: Value, items: Vec<Value>) -> Result<Response, Failure> {
    if response["status"] == "failed" || !response["error"].is_null() {
        return Err(classify_event(&response["error"]));
    }
    let output = match response.get_mut("output").map(Value::take) {
        Some(Value::Array(output)) if !output.is_empty() => output,
        _ => items,
    };
    // Every item is sent back in later requests, so one the provider would
    // reject there must not enter the history.
    let output = output
        .into_iter()
        .filter_map(Item::from_value)
        .filter(|item| item.kind() != Kind::Call || item.well_formed_call())
        .collect();
    let usage = &response["usage"];
    let usage = usage["input_tokens"].as_u64().map(|input| {
        let details = &usage["input_tokens_details"];
        let cached = details["cached_tokens"].as_u64().unwrap_or(0).min(input);
        Usage {
            input,
            output: usage["output_tokens"].as_u64().unwrap_or(0),
            cached,
            cache_write: details["cache_write_tokens"].as_u64().unwrap_or(0).min(input - cached),
            cost: usage["cost"].as_f64(),
        }
    });
    let incomplete = (response["status"] == "incomplete")
        .then(|| response["incomplete_details"]["reason"].as_str().unwrap_or("unknown").to_owned());
    Ok(Response { output, usage, incomplete })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::Cursor;

    use serde_json::json;

    use super::*;

    fn parse(stream: &str) -> (Result<Response, Failure>, Vec<String>) {
        let seen = RefCell::new(Vec::new());
        let on = |event: Event| {
            if let Event::Text(text) | Event::Thinking(text) = event {
                seen.borrow_mut().push(text);
            }
        };
        let result = Parser::new(&on).read(Cursor::new(stream), &AtomicBool::new(false));
        (result, seen.into_inner())
    }

    #[test]
    fn openai_streams_yield_output_usage_and_deltas() {
        let stream = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",",
            "\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\"}]}],",
            "\"usage\":{\"input_tokens\":120,\"input_tokens_details\":{\"cached_tokens\":100,\"cache_write_tokens\":15},\"output_tokens\":7,\"cost\":0.0012}}}\n\n",
        );
        let (result, deltas) = parse(stream);
        let response = result.expect("stream completes");
        assert_eq!(deltas, ["Hel", "lo"]);
        assert_eq!(
            response.usage,
            Some(Usage { input: 120, output: 7, cached: 100, cache_write: 15, cost: Some(0.0012) })
        );
        assert_eq!(response.output[0].text(), "Hello");
        assert_eq!(response.incomplete, None);
    }

    #[test]
    fn done_items_stand_in_for_missing_output() {
        let call =
            json!({ "type": "function_call", "call_id": "c1", "name": "bash", "arguments": "{}" });
        let done = json!({ "type": "response.output_item.done", "item": call });
        let omitted = json!({ "status": "completed" });
        let empty = json!({ "status": "completed", "output": [] });
        for response in [omitted, empty] {
            let completed = json!({ "type": "response.completed", "response": response });
            let stream =
                format!(": OPENROUTER PROCESSING\n\ndata: {done}\n\ndata: {completed}\n\n");
            let output = parse(&stream).0.expect("stream completes").output;
            assert_eq!(output, [Item::from_value(call.clone()).expect("item")], "{response}");
        }
    }

    #[test]
    fn hidden_reasoning_still_signals_that_it_started() {
        let stream = concat!(
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"reasoning\",\"summary\":[]}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        );
        let (result, seen) = parse(stream);
        assert!(result.is_ok());
        assert_eq!(seen, [""]);
    }

    #[test]
    fn output_items_that_cannot_be_sent_back_are_dropped() {
        let call = json!({"type":"function_call","call_id":"c1","name":"bash","arguments":""});
        let output = json!([
            null,
            {"type":"function_call","name":"bash","arguments":"{}"},
            {"type":"function_call","call_id":"c2","arguments":"{}"},
            {"type":"function_call","call_id":"c3","name":"bash","arguments":{}},
            call,
        ]);
        let response = json!({ "status": "completed", "output": output });
        let response = finish(response, Vec::new()).expect("response");
        assert_eq!(response.output, [Item::from_value(call).expect("item")]);
    }

    #[test]
    fn usage_never_counts_more_cached_tokens_than_input() {
        let usage = json!({
            "input_tokens": 10,
            "output_tokens": 1,
            "input_tokens_details": { "cached_tokens": 100, "cache_write_tokens": 5 },
        });
        let response = finish(json!({ "status": "completed", "usage": usage }), Vec::new());
        let usage = response.expect("response").usage;
        assert_eq!(
            usage,
            Some(Usage { input: 10, output: 1, cached: 10, cache_write: 0, cost: None })
        );
    }

    #[test]
    fn multiline_data_is_joined() {
        let stream = "data: {\"type\":\ndata: \"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n";
        let response = parse(stream).0.expect("stream completes");
        assert_eq!(response.incomplete.as_deref(), Some("max_output_tokens"));
    }

    #[test]
    fn streams_complete_only_at_a_terminal_event() {
        let (result, _) =
            parse("data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n");
        assert!(matches!(result, Err(Failure::Retry { .. })), "{result:?}");
        let (result, _) = parse("data: [DONE]\n\n");
        assert!(matches!(result, Err(Failure::Retry { .. })), "{result:?}");
        let (result, _) = parse(
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n",
        );
        assert!(result.is_ok(), "a last event without its blank line counts: {result:?}");
        let failed = format!(
            "data: {}\n\n",
            json!({ "type": "response.failed", "response": { "error": { "code": "rate_limit_exceeded", "message": "slow down" } } })
        );
        assert!(matches!(parse(&failed).0, Err(Failure::Retry { .. })));
    }
}
