//! Items of the conversation as the Responses API carries them.
//!
//! An item stays the JSON object the provider sent or agt wrote, so fields
//! agt never reads, such as encrypted reasoning, round-trip unchanged.
//! [`Kind`] says what an item is, and [`Content`] is what agt shows the model
//! in a tool result or a message of its own: text with saved images at points
//! in it.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::image;

/// Stands in for a field an item does not have.
static NULL: Value = Value::Null;

/// An item of the conversation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Item(Map<String, Value>);

/// What an item is, from its `type` and `role`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A message from the user, or one agt sends in the user's place.
    User,
    /// A message the model wrote.
    Assistant,
    Reasoning,
    /// A function call the model made.
    Call,
    /// The output of a function call.
    Output,
    /// A provider's compaction item, which stands in for earlier conversation.
    Compaction,
    /// An item agt has no use for, sent back as it came.
    Other,
}

impl Item {
    /// The item `value` holds, when it is an object.
    pub(crate) fn from_value(value: Value) -> Option<Self> {
        match value {
            Value::Object(fields) => Some(Self(fields)),
            _ => None,
        }
    }

    /// A message from the user made of `content` parts.
    pub(crate) fn user(content: Vec<Value>) -> Self {
        Self(object([
            ("type", "message".into()),
            ("role", "user".into()),
            ("content", content.into()),
        ]))
    }

    /// The output of call `call_id`: text, or parts that show images.
    pub(crate) fn output(call_id: &str, output: Value) -> Self {
        Self(object([
            ("type", "function_call_output".into()),
            ("call_id", call_id.into()),
            ("output", output),
        ]))
    }

    pub(crate) fn kind(&self) -> Kind {
        // The API reads an item without a type as a message.
        match self.str("type").unwrap_or("message") {
            "message" if self.str("role") == Some("user") => Kind::User,
            "message" => Kind::Assistant,
            "reasoning" => Kind::Reasoning,
            "function_call" => Kind::Call,
            "function_call_output" => Kind::Output,
            "compaction" => Kind::Compaction,
            _ => Kind::Other,
        }
    }

    /// The field `key`, or null when the item has none.
    pub(crate) fn get(&self, key: &str) -> &Value {
        self.0.get(key).unwrap_or(&NULL)
    }

    /// The field `key` when it is a string.
    pub(crate) fn str(&self, key: &str) -> Option<&str> {
        self.get(key).as_str()
    }

    /// The content of a message, or the output of a call.
    pub(crate) fn content(&self) -> &Value {
        self.0.get("output").unwrap_or_else(|| self.get("content"))
    }

    /// The content of a message, or the output of a call, to change in place.
    pub(crate) fn content_mut(&mut self) -> Option<&mut Value> {
        let key = if self.0.contains_key("output") { "output" } else { "content" };
        self.0.get_mut(key)
    }

    /// The text of a message or an output.
    pub(crate) fn text(&self) -> String {
        content_text(self.content())
    }

    /// Whether a message refuses instead of answering.
    pub(crate) fn refuses(&self) -> bool {
        self.get("content")
            .as_array()
            .is_some_and(|parts| parts.iter().any(|part| part["type"] == "refusal"))
    }

    /// Whether reasoning can be sent back: it carries encrypted content or its
    /// text. Reasoning with neither refers to provider state that stateless
    /// requests do not have.
    pub(crate) fn replayable(&self) -> bool {
        !self.get("encrypted_content").is_null()
            || self.get("content").as_array().is_some_and(|parts| !parts.is_empty())
    }

    /// Whether a function call can be sent back: it has a call id, a name and
    /// arguments as a string.
    pub(crate) fn well_formed_call(&self) -> bool {
        self.get("arguments").is_string()
            && ["call_id", "name"].iter().all(|key| self.str(key).is_some_and(|v| !v.is_empty()))
    }

    /// The saved images the item shows.
    pub(crate) fn images(&self) -> impl Iterator<Item = &str> {
        image::references(self.content())
    }

    /// The item without its `id`, which OpenAI pairs with the reasoning that
    /// produced it and rejects once that reasoning is left out.
    pub(crate) fn without_id(&self) -> Self {
        let mut item = self.clone();
        item.0.remove("id");
        item
    }
}

/// The text of content given as a string or as parts.
pub(crate) fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str().or(part["refusal"].as_str()))
            .collect(),
        _ => String::new(),
    }
}

/// A part of a message that holds `text`.
pub(crate) fn input_text(text: impl Into<String>) -> Value {
    Value::Object(object([("type", "input_text".into()), ("text", text.into().into())]))
}

/// Text with saved images at points in it: what a tool result or a message of
/// agt's own shows the model.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Content {
    text: String,
    /// Each image's saved file, after the byte of `text` it follows, in order.
    images: Vec<(usize, String)>,
}

impl Content {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty() && self.images.is_empty()
    }

    pub(crate) fn push_str(&mut self, text: &str) {
        self.text.push_str(text);
    }

    /// Shows saved image `reference` after the text so far.
    pub(crate) fn push_image(&mut self, reference: String) {
        self.images.push((self.text.len(), reference));
    }

    pub(crate) fn append(&mut self, other: Self) {
        let offset = self.text.len();
        self.text.push_str(&other.text);
        self.images.extend(other.images.into_iter().map(|(at, image)| (offset + at, image)));
    }

    /// Removes trailing whitespace; images after it stay at the end.
    pub(crate) fn trim_end(&mut self) {
        let len = self.text.trim_end().len();
        self.text.truncate(len);
        for (at, _) in &mut self.images {
            *at = (*at).min(len);
        }
    }

    /// The parts that show it in order: text, and images at high detail.
    pub(crate) fn into_parts(self) -> Vec<Value> {
        let mut parts = Vec::with_capacity(self.images.len() * 2 + 1);
        let mut from = 0;
        for (at, reference) in self.images {
            if at > from {
                parts.push(input_text(&self.text[from..at]));
            }
            parts.push(image::part(reference));
            from = at;
        }
        if from < self.text.len() || parts.is_empty() {
            parts.push(input_text(&self.text[from..]));
        }
        parts
    }

    /// A call's output: its text alone when it shows no image, and its parts
    /// otherwise.
    pub(crate) fn into_output(self) -> Value {
        if self.images.is_empty() { self.text.into() } else { self.into_parts().into() }
    }
}

impl From<String> for Content {
    fn from(text: String) -> Self {
        Self { text, images: Vec::new() }
    }
}

impl From<&str> for Content {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

/// An object that owns its values; `json!` would copy large pastes and outputs.
fn object<const N: usize>(fields: [(&str, Value); N]) -> Map<String, Value> {
    fields.into_iter().map(|(key, value)| (key.to_owned(), value)).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn image(reference: &str) -> Value {
        json!({ "type": "input_image", "image_url": reference, "detail": "high" })
    }

    #[test]
    fn items_are_known_by_their_type_and_role() {
        let kind = |value: Value| Item::from_value(value).expect("an object").kind();
        assert_eq!(kind(json!({ "role": "user", "content": [] })), Kind::User);
        assert_eq!(kind(json!({ "type": "message", "role": "assistant" })), Kind::Assistant);
        assert_eq!(kind(json!({ "type": "reasoning" })), Kind::Reasoning);
        assert_eq!(kind(json!({ "type": "function_call" })), Kind::Call);
        assert_eq!(kind(json!({ "type": "function_call_output" })), Kind::Output);
        assert_eq!(kind(json!({ "type": "compaction" })), Kind::Compaction);
        assert_eq!(kind(json!({ "type": "web_search_call" })), Kind::Other);
        assert_eq!(Item::from_value(json!([1])), None, "an item is an object");
    }

    #[test]
    fn items_round_trip_the_fields_agt_does_not_read() {
        // Keys are kept sorted, so an item serializes the same way every time.
        let raw = r#"{"encrypted_content":"sealed","id":"rs_1","summary":[],"type":"reasoning"}"#;
        let item: Item = serde_json::from_str(raw).expect("an item");
        assert!(item.replayable());
        assert_eq!(serde_json::to_string(&item).expect("JSON"), raw);
        let bare = Item::from_value(json!({ "type": "reasoning", "summary": [] })).expect("item");
        assert!(!bare.replayable(), "reasoning without content cannot be sent back");
        assert_eq!(item.without_id().get("id"), &Value::Null);
    }

    #[test]
    fn content_shows_its_images_where_they_were_put() {
        let mut content = Content::from("[id 1 · exit 0 · 0.2s]\n");
        content.push_image("images/a-4x2.png".into());
        let mut rest = Content::from("between\n");
        rest.push_image("images/b-4x2.png".into());
        rest.push_str("\n\n");
        content.append(rest);
        content.trim_end();
        assert_eq!(content.text(), "[id 1 · exit 0 · 0.2s]\nbetween");
        assert_eq!(
            content.into_output(),
            json!([
                input_text("[id 1 · exit 0 · 0.2s]\n"),
                image("images/a-4x2.png"),
                input_text("between"),
                image("images/b-4x2.png"),
            ])
        );
        assert_eq!(Content::from("plain").into_output(), "plain", "text alone stays a string");
        let mut only = Content::default();
        only.push_image("images/c-1x1.png".into());
        assert_eq!(only.into_parts(), [image("images/c-1x1.png")]);
        assert_eq!(Content::default().into_parts(), [input_text("")], "a message is never empty");
    }
}
