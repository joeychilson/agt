//! The conversation a session carries, measured against the limits it is kept
//! within, and the ways it gets smaller: eliding old tool output, cutting it
//! at a turn boundary for a summary, and continuing from what a provider's
//! compaction returned.
//!
//! Any change to earlier history invalidates the provider's prompt cache from
//! that point on, and cached input costs a fraction of fresh input, so history
//! is rewritten rarely and all at once, when the context nears its budget.

use std::fmt::Write as _;
use std::io;

use serde_json::Value;

use super::inbox::is_notices;
use crate::image;
use crate::item::{Item, Kind, content_text};
use crate::models::{bytes, tokens};
use crate::prompt::{self, SKILL_CONTENT};
use crate::store::{Restored, Snapshot};

/// Tool outputs shorter than this are never elided.
const MASK_MIN_OUTPUT: usize = 256;
const MASKED: &str =
    "[output elided to save context; procs/<id>.log in the session directory has all of it]";
/// Ends the note that replaces an elided image, after the file it names.
const IMAGE_KEPT: &str = " in the session directory keeps it]";
/// Most bytes of verbatim user messages a checkpoint carries.
const USERS_BUDGET: usize = 24 * 1024;
/// Most bytes of skill instructions a checkpoint carries (about 25k tokens).
const SKILLS_BUDGET: usize = 100 * 1024;
/// Most words a summary may use, reached with windows of 60k tokens or more.
const SUMMARY_WORDS: u64 = 1500;
/// How the message that starts a summarized context begins.
pub(super) const CHECKPOINT: &str = "<checkpoint>";

/// Token thresholds derived from the context window and the model's pricing.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) window: u64,
    /// Input tokens above which a whole request is billed at higher rates.
    pub(crate) tier: Option<u64>,
}

impl Limits {
    /// The context agt works within: the window, or the pricing tier when it
    /// is smaller, since crossing the tier raises the price of every token.
    pub(crate) fn budget(self) -> u64 {
        self.tier.map_or(self.window, |tier| tier.min(self.window))
    }

    /// Context above which a request compacts first.
    pub(crate) fn compact_at(self) -> u64 {
        // The window must also hold the response. A tier counts input only,
        // and the margin below it covers estimated tokens.
        let reserve = (self.window / 100 * 15).max(16_000).min(self.window / 2);
        let under_tier = self.tier.map_or(u64::MAX, |tier| tier - tier / 20);
        (self.window - reserve).min(under_tier)
    }

    /// Context above which a new user turn compacts first. Compacting between
    /// tasks loses less than compacting in the middle of one.
    pub(crate) fn compact_between_turns_at(self) -> u64 {
        self.compact_at() / 4 * 3
    }

    /// Context still above this once old tool output is elided needs a summary.
    pub(crate) fn summarize_above(self) -> u64 {
        self.budget() / 2
    }

    /// Recent context kept verbatim, both as unelided tool output and as the
    /// tail that survives summarization.
    pub(crate) fn keep(self) -> u64 {
        (self.budget() / 8).min(24_000)
    }

    /// Serialized conversation the fallback summarizer may read at once.
    pub(crate) fn summary_input(self) -> u64 {
        self.budget() / 2
    }

    /// Bytes of verbatim user messages a checkpoint carries.
    pub(crate) fn users_budget(self) -> usize {
        bytes(self.budget() / 16).min(USERS_BUDGET)
    }

    /// Bytes of active skill instructions a checkpoint carries.
    pub(crate) fn skills_budget(self) -> usize {
        bytes(self.budget() / 10).min(SKILLS_BUDGET)
    }

    /// Words a summary may use. Scaling with the budget keeps checkpoints
    /// small enough that compaction frees real room even in small windows.
    pub(crate) fn summary_words(self) -> u64 {
        (self.budget() / 40).clamp(100, SUMMARY_WORDS)
    }
}

/// The conversation every request carries.
#[derive(Default)]
pub(super) struct Context {
    pub(super) items: Vec<Item>,
    /// Reasoning and compaction items before this index are not sent back:
    /// another model made them, or the provider rejected them.
    pub(super) reasoning_from: usize,
    /// Tokens the provider counted, and how many items that count covers.
    counted: Option<(u64, usize)>,
    /// The latest summary, which the next one updates.
    pub(super) summary: String,
    /// User messages compaction took out of the context, which checkpoints
    /// carry verbatim.
    pub(super) users: Vec<String>,
    /// Skills whose instructions are in the context, which are not added again.
    pub(super) skills: Vec<String>,
}

impl Context {
    /// The context a resumed session continues with.
    pub(super) fn restored(restored: Restored, reasoning_from: usize) -> Self {
        Self {
            skills: skills_in(&restored.history),
            items: restored.history,
            reasoning_from,
            counted: None,
            summary: restored.summary,
            users: restored.users,
        }
    }

    pub(super) fn push(&mut self, item: Item) {
        self.items.push(item);
    }

    /// Tokens in use: what the provider last counted plus an estimate for the
    /// items since, or an estimate of everything with `fixed` tokens of
    /// instructions and tools.
    pub(super) fn tokens(&self, fixed: u64) -> u64 {
        let estimate = |items: &[Item]| tokens(items.iter().map(size).sum());
        match self.counted {
            Some((counted, len)) if len <= self.items.len() => {
                counted.saturating_add(estimate(&self.items[len..]))
            }
            _ => fixed + estimate(&self.items),
        }
    }

    /// Records that the provider counted `tokens` for the context so far.
    pub(super) fn count(&mut self, tokens: u64) {
        self.counted = Some((tokens, self.items.len()));
    }

    /// Elides tool outputs older than the newest `keep` bytes of output.
    /// Returns bytes freed.
    pub(super) fn mask(&mut self, keep: usize) -> usize {
        let freed = mask(&mut self.items, keep);
        if freed > 0 {
            self.counted = None;
        }
        freed
    }

    /// Bytes `mask` would free, without changing anything.
    pub(super) fn maskable(&self, keep: usize) -> usize {
        mask_targets(&self.items, keep).iter().map(|(_, _, bytes)| bytes).sum()
    }

    /// Where to split the history so about `keep` bytes stay verbatim. Cuts
    /// fall only on turn boundaries, so a call is never separated from its
    /// output or from the reasoning before it. `None` means there is nothing
    /// worth summarizing.
    pub(super) fn cut(&self, keep: usize) -> Option<usize> {
        let items = &self.items;
        let mut suffix = 0;
        let mut cut = None;
        for index in (1..items.len()).rev() {
            suffix += size(&items[index]);
            if is_boundary(items, index) {
                cut = Some(index);
                if suffix >= keep {
                    break;
                }
            }
        }
        cut.filter(|&cut| !(cut == 1 && is_checkpoint(&items[0])))
    }

    /// Replaces the items before `cut` with the checkpoint `checkpoint` makes
    /// from the new `summary` and the user messages carried within
    /// `users_budget` bytes.
    pub(super) fn summarized(
        &mut self,
        cut: usize,
        summary: String,
        users_budget: usize,
        checkpoint: impl FnOnce(&str, &[String]) -> Item,
    ) {
        let span: Vec<Item> = self.items.drain(..cut).collect();
        carry_users(&mut self.users, &span, users_budget);
        self.summary = summary;
        self.items.insert(0, checkpoint(&self.summary, &self.users));
        self.reasoning_from = (self.reasoning_from + 1).saturating_sub(cut);
        self.recount();
    }

    /// Continues from `window`, the context a provider's compaction endpoint
    /// returned, carrying the user messages it left out for a summary agt may
    /// write later.
    pub(super) fn replaced(&mut self, window: Vec<Item>, users_budget: usize) {
        let span = std::mem::replace(&mut self.items, window);
        let kept: Vec<String> =
            self.items.iter().filter(|item| item.kind() == Kind::User).map(Item::text).collect();
        let dropped: Vec<Item> = span
            .into_iter()
            .filter(|item| item.kind() == Kind::User && !kept.contains(&item.text()))
            .collect();
        carry_users(&mut self.users, &dropped, users_budget);
        // Reasoning a provider rejected is gone from the new context.
        self.reasoning_from = 0;
        self.recount();
    }

    /// Continues from the last compaction item the provider's inline
    /// compaction left, dropping the items before it, as the provider advises
    /// for requests that carry the whole input.
    pub(super) fn inline_compacted(&mut self, users_budget: usize) {
        let Some(at) = self.items.iter().rposition(|item| item.kind() == Kind::Compaction) else {
            return;
        };
        let span: Vec<Item> = self.items.drain(..at).collect();
        carry_users(&mut self.users, &span, users_budget);
        self.reasoning_from = self.reasoning_from.saturating_sub(at);
        self.recount();
    }

    /// Adds `message` right after the last compaction item, or at the end
    /// without one, so it belongs to the context that compaction left.
    pub(super) fn carry(&mut self, message: Item) {
        let at = self
            .items
            .iter()
            .rposition(|item| item.kind() == Kind::Compaction)
            .map_or(self.items.len(), |at| at + 1);
        self.items.insert(at, message);
        if at < self.reasoning_from {
            self.reasoning_from += 1;
        }
        self.counted = None;
    }

    /// The compaction record of the context, made with `provider`'s `model`
    /// after a session cost `cost` dollars.
    pub(super) fn snapshot<'a>(
        &'a self,
        provider: &'a str,
        model: &'a str,
        cost: f64,
    ) -> Snapshot<'a> {
        Snapshot {
            history: self.items.as_slice().into(),
            summary: self.summary.as_str().into(),
            users: self.users.as_slice().into(),
            provider: provider.into(),
            model: model.into(),
            reasoning_from: self.reasoning_from,
            cost,
        }
    }

    /// Whether any item shows an image.
    pub(super) fn has_images(&self) -> bool {
        self.items.iter().any(|item| item.images().next().is_some())
    }

    /// Forgets counted tokens and finds the skills in use again, after the
    /// items changed.
    fn recount(&mut self) {
        self.counted = None;
        self.skills = skills_in(&self.items);
    }
}

/// Serialized size of `value` in bytes.
pub(super) fn value_size(value: &Value) -> usize {
    struct Counter(usize);
    impl io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).map_or(0, |()| counter.0)
}

/// Serialized size of an item in bytes, with each saved image it shows
/// counted at its visual tokens rather than by its short reference.
fn size(item: &Item) -> usize {
    let serialized = serde_json::to_vec(item).map_or(0, |bytes| bytes.len());
    let images: usize = item.images().map(|reference| bytes(image::tokens(reference))).sum();
    serialized + images
}

/// Replaces tool outputs older than the newest `keep` bytes of output with a
/// short note. Skill instructions are never elided, and an elided image
/// leaves the name of its saved file. Returns bytes freed.
pub(crate) fn mask(history: &mut [Item], keep: usize) -> usize {
    let mut freed = 0;
    for (index, marker, bytes) in mask_targets(history, keep) {
        if let Some(output) = history[index].content_mut() {
            *output = marker.into();
            freed += bytes;
        }
    }
    freed
}

/// The outputs `mask` replaces, with their notes and the bytes each frees.
fn mask_targets(history: &[Item], keep: usize) -> Vec<(usize, String, usize)> {
    let mut kept = 0;
    let mut targets = Vec::new();
    for (index, item) in history.iter().enumerate().rev() {
        if item.kind() != Kind::Output {
            continue;
        }
        let (size, marker) = match item.content() {
            Value::String(output) => {
                // A loaded skill's output is its status line, then its instructions.
                let skill =
                    output.lines().nth(1).is_some_and(|line| line.starts_with(SKILL_CONTENT));
                if output.len() < MASK_MIN_OUTPUT
                    || output.ends_with(MASKED)
                    || output.ends_with(IMAGE_KEPT)
                    || skill
                {
                    continue;
                }
                let header = output.lines().next().unwrap_or_default();
                (output.len(), format!("{header}\n{MASKED}"))
            }
            output => {
                let references: Vec<&str> = image::references(output).collect();
                if references.is_empty() {
                    continue;
                }
                let text = content_text(output);
                let mut lines = text.lines();
                let mut marker = lines.next().unwrap_or_default().to_owned();
                // Output besides the headers of the images is in the log.
                if lines.any(|line| !line.is_empty() && !line.starts_with(image::HEADER)) {
                    let _ = write!(marker, "\n{MASKED}");
                }
                for reference in references {
                    let _ =
                        write!(marker, "\n[image elided to save context; {reference}{IMAGE_KEPT}");
                }
                (self::size(item), marker)
            }
        };
        if kept < keep {
            kept += size;
            continue;
        }
        let freed = size.saturating_sub(marker.len());
        targets.push((index, marker, freed));
    }
    targets
}

fn is_boundary(history: &[Item], index: usize) -> bool {
    let item = history[index].kind();
    let previous = history[index - 1].kind();
    item != Kind::Output
        && (item == Kind::User || previous == Kind::User || previous == Kind::Output)
}

/// Whether `item` is the checkpoint message a summarized context starts with.
pub(super) fn is_checkpoint(item: &Item) -> bool {
    item.kind() == Kind::User
        && item.content()[0]["text"].as_str().is_some_and(|text| text.starts_with(CHECKPOINT))
}

/// The skills whose instructions appear in `history`.
fn skills_in(history: &[Item]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for item in history {
        for name in prompt::skill_names(&item.text()) {
            if !names.iter().any(|known| known == name) {
                names.push(name.to_owned());
            }
        }
    }
    names
}

/// Adds user messages leaving the context to the verbatim record, keeping the
/// first (usually the task) and the most recent within `budget` bytes. Each
/// message is clipped first, so one long paste cannot crowd out the rest.
fn carry_users(users: &mut Vec<String>, span: &[Item], budget: usize) {
    // A model switch may have reduced the window since the previous checkpoint.
    for (index, message) in users.iter_mut().enumerate() {
        *message = clip_middle(message, budget / if index == 0 { 3 } else { 6 });
    }
    users.retain(|message| !message.is_empty());
    let messages = span
        .iter()
        .filter(|item| item.kind() == Kind::User && !is_checkpoint(item) && !is_notices(item));
    for item in messages {
        let limit = if users.is_empty() { budget / 3 } else { budget / 6 };
        // Skills travel separately; only the user's words are kept.
        let message = clip_middle(prompt::user_words(&item.text()), limit);
        if !message.is_empty() {
            users.push(message);
        }
    }
    if users.iter().map(String::len).sum::<usize>() <= budget {
        return;
    }
    let first = users[0].clone();
    let mut room = budget.saturating_sub(first.len());
    let mut recent: Vec<String> = users[1..]
        .iter()
        .rev()
        .map_while(|message| {
            room = room.checked_sub(message.len())?;
            Some(message.clone())
        })
        .collect();
    recent.reverse();
    *users = std::iter::once(first).chain(recent).collect();
}

/// Keeps both ends of `text` within `max` bytes, including the omission marker.
pub(super) fn clip_middle(text: &str, max: usize) -> String {
    const MARKER: &str = "\n[…]\n";
    if text.len() <= max {
        return text.to_owned();
    }
    if max < MARKER.len() {
        return text[..text.floor_char_boundary(max)].to_owned();
    }
    let room = max - MARKER.len();
    let head = text.floor_char_boundary(room / 2);
    let tail = text.ceil_char_boundary(text.len() - (room - head));
    format!("{}{MARKER}{}", &text[..head], &text[tail..])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::item::{self, input_text};

    fn user(text: &str) -> Item {
        Item::user(vec![input_text(text)])
    }

    fn item(value: Value) -> Item {
        Item::from_value(value).expect("an object")
    }

    fn call(id: &str) -> Item {
        item(
            json!({"type": "function_call", "call_id": id, "name": "bash", "arguments": "{\"command\":\"ls\"}"}),
        )
    }

    fn output(id: &str, text: &str) -> Item {
        Item::output(id, text.into())
    }

    fn reply(text: &str) -> Item {
        item(
            json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}),
        )
    }

    fn reasoning() -> Item {
        item(json!({"type": "reasoning", "encrypted_content": "x", "summary": []}))
    }

    fn context(items: Vec<Item>) -> Context {
        Context { items, ..Context::default() }
    }

    #[test]
    fn cuts_never_split_calls_from_outputs() {
        let history = vec![
            user("task"),
            reasoning(),
            call("a"),
            call("b"),
            output("a", "1"),
            output("b", "2"),
            reasoning(),
            call("c"),
            output("c", "3"),
            reply("done"),
            user("next"),
            reply("ok"),
        ];
        let boundaries: Vec<usize> =
            (1..history.len()).filter(|&index| is_boundary(&history, index)).collect();
        assert_eq!(boundaries, [1, 6, 9, 10, 11]);
        let context = context(history);
        assert_eq!(context.cut(0), Some(11));
        assert_eq!(context.cut(1_000_000), Some(1));
        assert_eq!(self::context(vec![user("only")]).cut(0), None);
        let compacted = self::context(vec![user("<checkpoint>old</checkpoint>"), user("new")]);
        assert_eq!(compacted.cut(0), None);
    }

    #[test]
    fn masking_keeps_recent_output_and_skills() {
        let long = |n: &str| format!("[id {n} · exit 0 · 1.0s]\n{}", "x".repeat(1000));
        let skill =
            format!("[id 0 · exit 0 · 0.0s]\n<skill_content name=\"s\">{}", "y".repeat(1000));
        let grep = format!(
            "[id 9 · exit 0 · 0.1s]\nlog.jsonl: <skill_content name=\"s\"> {}",
            "z".repeat(1000)
        );
        let mut context = context(vec![
            call("g"),
            output("g", &grep),
            call("s"),
            output("s", &skill),
            call("a"),
            output("a", &long("1")),
            call("b"),
            output("b", &long("2")),
            call("c"),
            output("c", &long("3")),
        ]);
        let expected = context.maskable(1500);
        let freed = context.mask(1500);
        assert!(freed > 1800, "{freed}");
        assert_eq!(freed, expected);
        let outputs: Vec<&Value> = context.items.iter().map(Item::content).collect();
        assert_eq!(outputs[1], &json!(format!("[id 9 · exit 0 · 0.1s]\n{MASKED}")));
        assert_eq!(outputs[3], &json!(skill));
        assert_eq!(outputs[5], &json!(format!("[id 1 · exit 0 · 1.0s]\n{MASKED}")));
        assert_eq!(outputs[7], &json!(long("2")));
        assert_eq!(context.mask(1500), 0, "masking is idempotent");
    }

    #[test]
    fn old_images_are_elided_to_the_name_of_their_file() {
        let shown = |id: &str, printed: &str, names: &[&str]| {
            let mut output = item::Content::from(format!("[id {id} · exit 0 · 1.0s]\n{printed}"));
            for name in names {
                output.push_str(&format!(
                    "[image /w/{name}.png · 4000x2000 · shown at 2000x1000, scale 2.00]\n"
                ));
                output.push_image(format!("images/{name}-2000x1000.png"));
            }
            Item::output(id, output.into_output())
        };
        let history = vec![
            shown("1", "", &["a"]),
            shown("2", "rendered 2 pages\n", &["b", "c"]),
            shown("3", "", &["new"]),
        ];
        // An image counts at its visual tokens: 72 by 36 patches of 28 px.
        let one = size(&history[2]);
        assert!(one > bytes(72 * 36), "{one}");
        let mut context = context(history.clone());
        let expected = context.maskable(one);
        assert_eq!(context.mask(one), expected);
        assert!(expected > bytes(3 * 72 * 36));
        let elided = |name: &str| {
            format!(
                "[image elided to save context; images/{name}-2000x1000.png in the session directory keeps it]"
            )
        };
        assert_eq!(
            context.items[0].content(),
            &json!(format!("[id 1 · exit 0 · 1.0s]\n{}", elided("a")))
        );
        assert_eq!(
            context.items[1].content(),
            &json!(format!("[id 2 · exit 0 · 1.0s]\n{MASKED}\n{}\n{}", elided("b"), elided("c"))),
            "other output points to the log"
        );
        assert_eq!(context.items[2], history[2]);
    }

    #[test]
    fn users_keep_the_task_and_recent_messages() {
        let mut users = Vec::new();
        let span = [
            user("the task"),
            user("<background>\nprocess 3 exited\n</background>"),
            user("tweak\n\n<skill_content name=\"x\">body</skill_content>"),
        ];
        carry_users(&mut users, &span, USERS_BUDGET);
        assert_eq!(users, ["the task", "tweak"]);

        let big = "m".repeat(10 * 1024);
        let span: Vec<Item> = (0..12).map(|_| user(&big)).collect();
        carry_users(&mut users, &span, USERS_BUDGET);
        assert_eq!(users[0], "the task");
        assert!(users.iter().map(String::len).sum::<usize>() <= USERS_BUDGET);
        assert_eq!(users.len(), 6, "long messages are clipped, not dropped");

        for budget in [0, 1, 6, 30, 1200] {
            let mut users = vec!["界".repeat(3000), "🙂".repeat(3000)];
            carry_users(&mut users, &[user(""), user(&"é".repeat(3000))], budget);
            let sizes: Vec<usize> = users.iter().map(String::len).collect();
            assert!(sizes.iter().sum::<usize>() <= budget, "budget {budget}, sizes {sizes:?}");
            assert!(!sizes.contains(&0), "budget {budget} kept an empty message: {sizes:?}");
        }
        assert_eq!(clip_middle("αβγδεζηθ", 11), "α\n[…]\nθ", "clips fall on character boundaries");
    }

    #[test]
    fn compactions_leave_the_context_they_describe() {
        let mut summarized =
            context(vec![user("task"), reply("working"), user("next"), reply("done")]);
        summarized.reasoning_from = 3;
        summarized.summarized(2, "## Goal".into(), USERS_BUDGET, |summary, users| {
            user(&format!("<checkpoint>{summary} {}</checkpoint>", users.join("|")))
        });
        let texts: Vec<String> = summarized.items.iter().map(Item::text).collect();
        assert_eq!(texts, ["<checkpoint>## Goal task</checkpoint>", "next", "done"]);
        assert_eq!(summarized.reasoning_from, 2);

        let mut inline = context(vec![
            user("task"),
            reply("working"),
            item(json!({ "type": "compaction", "encrypted_content": "sealed" })),
            call("c1"),
        ]);
        inline.reasoning_from = 3;
        inline.inline_compacted(USERS_BUDGET);
        assert_eq!(inline.items[0].kind(), Kind::Compaction);
        assert_eq!((inline.items.len(), inline.reasoning_from), (2, 1));
        assert_eq!(inline.users, ["task"], "users it dropped are kept for a later summary");

        let mut replaced = context(vec![user("task"), user("more")]);
        replaced.replaced(vec![user("task"), item(json!({ "type": "compaction" }))], USERS_BUDGET);
        assert_eq!(replaced.users, ["more"]);
    }

    #[test]
    fn tokens_are_counted_then_estimated_for_what_follows() {
        let mut context = context(vec![user("hello")]);
        let fixed = context.tokens(1000);
        assert!(fixed > 1000);
        context.count(5000);
        assert_eq!(context.tokens(1000), 5000);
        context.push(user(&"x".repeat(4000)));
        assert!(context.tokens(1000) > 5900);
    }

    #[test]
    fn limits_scale_with_the_window_and_the_pricing_tier() {
        let limits = Limits { window: 200_000, tier: None };
        assert_eq!(limits.budget(), 200_000);
        assert_eq!(limits.compact_at(), 170_000);
        assert_eq!(limits.compact_between_turns_at(), 127_500);
        assert_eq!(limits.summarize_above(), 100_000);
        assert_eq!(limits.keep(), 24_000);
        assert_eq!(limits.users_budget(), USERS_BUDGET);
        assert_eq!(limits.skills_budget(), 80_000);
        assert_eq!(limits.summary_words(), 1500);
        let small = Limits { window: 16_000, tier: None };
        assert_eq!(small.compact_at(), 8_000);
        assert_eq!(small.keep(), 2_000);
        assert_eq!(small.users_budget(), 4_000);
        assert_eq!(small.summary_words(), 400);
        // GPT-5.6 and later bill requests over 272K input tokens at higher rates.
        let tiered = Limits { window: 1_050_000, tier: Some(272_000) };
        assert_eq!(tiered.budget(), 272_000);
        assert_eq!(tiered.compact_at(), 258_400);
        assert_eq!(tiered.summarize_above(), 136_000);
        let chatgpt = Limits { window: 272_000, tier: Some(272_000) };
        assert_eq!(chatgpt.compact_at(), 231_200);
    }
}
