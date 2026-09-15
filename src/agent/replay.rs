//! Showing a logged session again: the updates each record produced when it
//! happened, so frontends render a replay through the code that renders a
//! live session.

use super::context::is_checkpoint;
use super::{Origin, Update, is_notices};
use crate::bash::{Outcome, ToolCall};
use crate::image;
use crate::item::{Item, Kind, content_text};
use crate::prompt;
use crate::store::Record;

/// The updates that show `record` again.
pub(crate) fn replay(record: &Record<'_>) -> Vec<Update> {
    match record {
        // Each notice has a record of its own, logged when it was told.
        Record::Item { item, .. } if is_notices(item) => Vec::new(),
        Record::Item { item, origin, .. } => replay_item(item, *origin),
        Record::Notice { text, .. } => vec![Update::Notice(text.to_string())],
        Record::Error { text, .. } => vec![Update::Error(text.to_string())],
        Record::Turn { stop, .. } => vec![Update::TurnEnd(*stop)],
        // A compaction is told in its notice records.
        Record::Compaction(_)
        | Record::Session { .. }
        | Record::Model { .. }
        | Record::Proc { .. }
        | Record::Exit { .. }
        | Record::Cost { .. }
        | Record::Mask { .. }
        | Record::Other => Vec::new(),
    }
}

/// The updates that show an item of the conversation again, which came from
/// `origin` when it is a message.
pub(crate) fn replay_item(item: &Item, origin: Origin) -> Vec<Update> {
    match item.kind() {
        Kind::User if is_notices(item) => notices(item),
        Kind::User if is_checkpoint(item) => Vec::new(),
        Kind::Compaction | Kind::Other => Vec::new(),
        Kind::User => {
            let images = item.images().map(str::to_owned).collect();
            vec![Update::User { text: user_text(item), images, origin }]
        }
        Kind::Assistant => vec![Update::Text(item.text()), Update::ResponseEnd],
        Kind::Reasoning => {
            let text = item.text();
            if text.is_empty() { Vec::new() } else { vec![Update::Thinking(text)] }
        }
        Kind::Call => vec![Update::ToolStart(ToolCall::from_item(item))],
        Kind::Output => vec![Update::ToolEnd {
            call_id: item.str("call_id").unwrap_or_default().to_owned(),
            output: item.content().clone(),
            outcome: Outcome::of(&item.text()),
        }],
    }
}

/// What the user wrote in a message: its text without the skills agt inlined
/// or the headers of attached images, which are for the model.
fn user_text(item: &Item) -> String {
    let text = match item.content() {
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .filter(|text| !text.starts_with(image::HEADER))
            .collect(),
        content => content_text(content),
    };
    prompt::user_words(&text).to_owned()
}

/// A message of notices as its notices showed: the first line of each,
/// without the time it was told.
fn notices(item: &Item) -> Vec<Update> {
    item.text()
        .lines()
        .filter_map(|line| line.strip_prefix('[')?.split_once(" UTC] "))
        .map(|(_, notice)| Update::Notice(notice.to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use serde_json::json;

    use super::*;
    use crate::agent::Stop;
    use crate::item::input_text;

    #[test]
    fn records_replay_as_the_updates_they_showed() {
        let message = Item::user(vec![
            input_text("[image /w/shot.png · 4x2]\n"),
            json!({ "type": "input_image", "image_url": "images/ab-4x2.png", "detail": "high" }),
            input_text("fix the header\n\n<skill_content name=\"deploy\">steps</skill_content>"),
        ]);
        let record = Record::Item { at: 1, origin: Origin::Send, item: Cow::Borrowed(&message) };
        let updates = replay(&record);
        let [Update::User { text, images, origin }] = &updates[..] else {
            panic!("a message replays as the user's: {updates:?}");
        };
        assert_eq!(
            (text.as_str(), &images[..], *origin),
            ("fix the header", &["images/ab-4x2.png".to_owned()][..], Origin::Send)
        );

        let output = Item::output("c1", "[id 2 · exit 101 · 4.2s]\nerror".into());
        let updates = replay_item(&output, Origin::User);
        assert!(
            matches!(&updates[..], [Update::ToolEnd { outcome: Outcome::Failed, .. }]),
            "{updates:?}"
        );
        let notices = Item::user(vec![input_text(
            "<background>\n[2026-09-15 14:02 UTC] process 1 exited after 2.0s\noutput\n</background>",
        )]);
        let shown = replay_item(&notices, Origin::User);
        assert!(
            matches!(&shown[..], [Update::Notice(text)] if text == "process 1 exited after 2.0s"),
            "{shown:?}"
        );
        let record = Record::Item { at: 1, origin: Origin::User, item: Cow::Borrowed(&notices) };
        assert!(replay(&record).is_empty(), "a log replays notices from their own records");
        let turn = Record::Turn { at: 2, stop: Stop::Cancelled };
        assert!(matches!(replay(&turn)[..], [Update::TurnEnd(Stop::Cancelled)]));
        let notice = Record::Notice { at: 3, text: "retrying".into() };
        assert!(matches!(&replay(&notice)[..], [Update::Notice(text)] if text == "retrying"));
    }
}
