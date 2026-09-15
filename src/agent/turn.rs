//! A turn: sending what is due to the model, reading its response, recovering
//! from what went wrong, and ending.

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use super::compaction::{After, Way};
use super::{Agent, Event, Message, Moment, Origin, Phase, Queued, Stop, Update, inbox};
use crate::bash::{self, ToolCall};
use crate::image;
use crate::item::{Item, Kind, input_text};
use crate::llm;
use crate::models::{MIN_WINDOW, bytes, tokens};
use crate::prompt;
use crate::provider::Compaction;
use crate::store::{self, Record};

/// The least serialized conversation a fallback summary is asked to read; a
/// request that still overflows below it cannot be summarized.
const MIN_SUMMARY_INPUT: usize = 16 * 1024;

impl Agent {
    /// Sends pending messages to the model, compacting first when needed.
    /// Compaction starts earlier at the start of a turn, so it tends to fall
    /// between tasks rather than in the middle of one.
    pub(super) fn advance(&mut self, moment: Moment) {
        self.flush_pending(moment);
        let used = self.context_tokens();
        let threshold = match moment {
            Moment::TurnStart => self.limits.compact_between_turns_at(),
            Moment::MidTurn => self.limits.compact_at(),
        };
        let native = self.settings.compaction();
        // A provider that compacts inline does it while responding; agt only
        // elides images, which every request carries in full.
        if native != Compaction::Inline && used > threshold {
            // Rewriting history invalidates the prompt cache, so it happens only
            // here. Without native compaction, eliding old tool output comes
            // first when it frees enough; otherwise the conversation is
            // summarized while the prefix the summary request reuses is cached.
            let keep = bytes(self.limits.keep());
            let masked = used.saturating_sub(tokens(self.context.maskable(keep)));
            let compact = native == Compaction::Endpoint || masked > self.limits.summarize_above();
            if compact
                && !self.compaction_backoff(used)
                && self.start_compaction(self.best_way(None), None, After::Respond)
            {
                return;
            }
            self.mask_old_output();
        } else if image::bytes(&self.context.items, &self.session.dir) > image::CONTEXT_BYTES {
            self.mask_old_output();
        }
        self.respond(moment);
    }

    /// Adds background notices and the messages due at `moment` to the
    /// history, showing frontends each message as the model receives it.
    pub(super) fn flush_pending(&mut self, moment: Moment) {
        if let Some(notices) = self.inbox.take_notices() {
            self.push(Item::user(inbox::block(notices).into_parts()), Origin::User);
        }
        for Queued { message, .. } in self.inbox.take_due(moment) {
            let Message { mut content, typed, origin } = message;
            self.expand_skills(&typed, &mut content);
            let item = Item::user(content);
            let images = item.images().map(str::to_owned).collect();
            self.updates.push(Update::User { text: typed, images, origin });
            self.push(item, origin);
        }
    }

    /// Inlines skills the user invoked with `/name` or `$name`.
    fn expand_skills(&mut self, typed: &str, content: &mut Vec<Value>) {
        let invoked: Vec<prompt::Skill> = self
            .instructions
            .skills
            .iter()
            .filter(|skill| mentions_skill(typed, &skill.name))
            .filter(|skill| !self.context.skills.contains(&skill.name))
            .cloned()
            .collect();
        for skill in invoked {
            match prompt::skill_block(&skill) {
                Ok(block) => {
                    content.push(input_text(format!("\n\n{block}")));
                    self.context.skills.push(skill.name);
                }
                Err(error) => self.error(format!("cannot read skill {}: {error}", skill.name)),
            }
        }
    }

    pub(super) fn mask_old_output(&mut self) {
        let keep = bytes(self.limits.keep());
        let freed = self.context.mask(keep);
        if freed > 0 {
            self.record(&Record::Mask { keep });
            self.notice(format!("elided old tool output, freeing about {} tokens", tokens(freed)));
        }
    }

    /// Whether a recent compaction failed and the context of `used` tokens has
    /// not grown much since.
    pub(super) fn compaction_backoff(&self, used: u64) -> bool {
        self.compaction_failed_at.is_some_and(|at| used < at.saturating_add(self.limits.keep()))
    }

    /// Reports model events to the frontend's loop, tagged with the current
    /// request's epoch.
    pub(super) fn forward(&self) -> impl Fn(llm::Event) + Send + Sync + 'static {
        let notify = Arc::clone(&self.notify);
        let epoch = self.epoch;
        move |event| notify(Event::Llm(epoch, event))
    }

    /// The session's own request: its instructions, tools and history.
    pub(super) fn request(&self) -> llm::Request<'_> {
        llm::Request {
            model: &self.settings.model.id,
            reasoning: self.settings.reasoning,
            instructions: &self.instructions.text,
            input: &self.context.items,
            reasoning_from: self.context.reasoning_from,
            tools: &self.tools,
            tool_choice: None,
            cache_key: &self.session.id,
            max_output_tokens: None,
            compact_at: None,
            images: self.vision.then_some(self.session.dir.as_path()),
        }
    }

    /// Sends the next request, the first of a turn or one within it.
    pub(super) fn respond(&mut self, moment: Moment) {
        if let Some(error) = self.log_error.take() {
            return self.fail_turn(error);
        }
        self.adopt_refresh();
        self.epoch += 1;
        let compact_at = (self.settings.compaction() == Compaction::Inline).then(|| match moment {
            Moment::TurnStart => self.limits.compact_between_turns_at(),
            Moment::MidTurn => self.limits.compact_at(),
        });
        let request = llm::Request { compact_at, ..self.request() };
        let stream = self.client.stream(&request, self.forward());
        self.stream = Some(stream);
        self.phase = Phase::Responding;
        self.text_started = false;
        self.last_event = Instant::now();
    }

    /// Forwards streamed text, dropping the blank lines some models emit
    /// before their first words.
    fn text(&mut self, text: String) {
        let text =
            if self.text_started { text } else { text.trim_start_matches(['\n', '\r']).to_owned() };
        if !text.is_empty() {
            self.text_started = true;
            self.updates.push(Update::Text(text));
        }
    }

    pub(super) fn on_llm(&mut self, event: llm::Event) {
        self.last_event = Instant::now();
        let compacting = matches!(self.phase, Phase::Compacting(_));
        match event {
            llm::Event::Text(text) if !compacting => self.text(text),
            llm::Event::Thinking(text) if !compacting => self.updates.push(Update::Thinking(text)),
            llm::Event::Reset if !compacting => {
                self.text_started = false;
                self.updates.push(Update::Reset);
            }
            llm::Event::Alive
            | llm::Event::Text(_)
            | llm::Event::Thinking(_)
            | llm::Event::Reset => {}
            llm::Event::Retry { attempt, delay, reason } => {
                // Waiting out a retry delay is not a stall.
                self.last_event += delay;
                self.notice(format!(
                    "{reason}; retrying in {} (attempt {attempt})",
                    bash::elapsed(delay)
                ));
            }
            llm::Event::Done(response) => {
                self.stream = None;
                if compacting {
                    self.compaction_done(response);
                } else {
                    self.response_done(response);
                }
            }
            llm::Event::Failed(error) => {
                self.stream = None;
                self.failed(error);
            }
        }
    }

    fn response_done(&mut self, response: llm::Response) {
        // Gateways may ignore streaming, or send only completed output items.
        if !self.text_started {
            for item in response.output.iter().filter(|item| item.kind() == Kind::Assistant) {
                self.text(item.text());
            }
        }
        let mut calls = Vec::new();
        let mut refusal = false;
        let mut compacted = false;
        let mut output = response.output;
        // Reasoning must be followed by the item it led to; sending back a
        // response cut off after its reasoning would fail every later request.
        while output.last().is_some_and(|item| item.kind() == Kind::Reasoning) {
            output.pop();
        }
        for item in output {
            match item.kind() {
                Kind::Reasoning if !item.replayable() => continue,
                Kind::Call => calls.push(ToolCall::from_item(&item)),
                Kind::Assistant => refusal |= item.refuses(),
                Kind::Compaction => compacted = true,
                _ => {}
            }
            self.push(item, Origin::User);
        }
        if let Some(usage) = response.usage {
            self.add_cost(&usage);
            self.context.count(usage.input.saturating_add(usage.output));
        }
        if compacted {
            self.inline_compacted();
        }
        // After the compaction's notices, so a reply still reads as the end of
        // the turn.
        self.updates.push(Update::ResponseEnd);
        self.report_usage();
        if !calls.is_empty() {
            return self.run_tools(calls);
        }
        if self.inbox.next_waits() {
            return self.advance(Moment::TurnStart);
        }
        let stop = match (response.incomplete.as_deref(), refusal) {
            (Some("content_filter"), _) | (None, true) => Stop::Refusal,
            (Some(_), _) => Stop::MaxTokens,
            (None, false) => Stop::EndTurn,
        };
        self.end_turn(stop);
    }

    fn failed(&mut self, error: llm::Error) {
        let way = match &self.phase {
            Phase::Compacting(compacting) => Some(compacting.way),
            _ => None,
        };
        match (error, way) {
            (llm::Error::Reasoning, _) => {
                // The reasoning sent back so far is left out from now on, such
                // as Gemini's from another of Google's backends, and reasoning
                // produced after this is sent back as usual. The history itself
                // is untouched so it stays aligned with the session log.
                let replayed = way != Some(Way::Serialized)
                    && self
                        .context
                        .items
                        .get(self.context.reasoning_from..)
                        .unwrap_or_default()
                        .iter()
                        .any(|item| item.kind() == Kind::Reasoning);
                if !replayed {
                    return self.fail_turn("the provider rejected reasoning items".into());
                }
                self.context.reasoning_from = self.context.items.len();
                self.notice("the provider rejected replayed reasoning; continuing without it");
                self.retry_after_recovery(way);
            }
            (llm::Error::Overflow, Some(Way::Native | Way::Cached)) => {
                self.fall_back(Way::Serialized);
            }
            (llm::Error::Fatal(message), Some(Way::Native)) => {
                self.notice(format!(
                    "the provider's compaction failed ({message}); writing a summary instead"
                ));
                self.fall_back(Way::Cached);
            }
            (llm::Error::Fatal(_), Some(Way::Cached)) => self.fall_back(Way::Serialized),
            (llm::Error::Overflow, Some(Way::Serialized)) => {
                if let Phase::Compacting(compacting) = &mut self.phase {
                    compacting.budget /= 2;
                    if compacting.budget < MIN_SUMMARY_INPUT {
                        return self.fail_turn("the conversation is too large to summarize".into());
                    }
                }
                self.request_summary();
            }
            (llm::Error::Overflow, None) => {
                let used = self.context_tokens();
                if self.compaction_backoff(used) {
                    return self
                        .fail_turn("the context window is full and compaction failed".into());
                }
                // The provider's real limit is lower than configured; learn it.
                self.limits.window = self.limits.window.min(used / 10 * 9).max(MIN_WINDOW);
                self.notice(format!(
                    "context window exceeded; limiting it to {} tokens",
                    self.limits.window
                ));
                if !self.start_compaction(Way::Serialized, None, After::Respond) {
                    self.fail_turn(
                        "the context window is full and nothing can be compacted".into(),
                    );
                }
            }
            (llm::Error::Image(message), _) if self.vision && self.context.has_images() => {
                // The images passed agt's own checks, so this provider takes
                // none, or not this one; the conversation goes on without them.
                self.vision = false;
                self.notice(format!(
                    "the provider rejected an image, so images are no longer sent: {message}"
                ));
                self.retry_after_recovery(way);
            }
            (llm::Error::Image(message) | llm::Error::Fatal(message), _) => {
                self.fail_turn(message);
            }
        }
    }

    /// Sends the request that failed again, once what it failed on is left out.
    fn retry_after_recovery(&mut self, compacting: Option<Way>) {
        match compacting {
            Some(_) => self.request_summary(),
            None => self.respond(Moment::MidTurn),
        }
    }

    /// Ends the turn with an error. A failed summary only skips compaction, so
    /// the turn goes on.
    pub(super) fn fail_turn(&mut self, message: String) {
        if matches!(self.phase, Phase::Compacting(_)) {
            self.notice(format!("compaction failed: {message}"));
            return self.compaction_skipped();
        }
        self.error(message);
        self.end_turn(Stop::Error);
    }

    pub(super) fn end_turn(&mut self, stop: Stop) {
        if let Some(error) = self.log_error.take() {
            self.updates.push(Update::Error(error));
        }
        self.phase = Phase::Idle;
        self.stream = None;
        self.record(&Record::Turn { at: store::now(), stop });
        self.updates.push(Update::TurnEnd(stop));
        if self.inbox.continues(stop) {
            self.advance(Moment::TurnStart);
        }
    }
}

/// Whether `text` invokes a skill as `$name` anywhere or `/name` at the start.
fn mentions_skill(text: &str, name: &str) -> bool {
    fn bare(word: &str) -> &str {
        word.trim_start_matches(|c: char| c.is_ascii_punctuation() && c != '$' && c != '/')
            .trim_end_matches(|c: char| c.is_ascii_punctuation() && c != '-')
    }
    let leading = text
        .split_whitespace()
        .next()
        .is_some_and(|word| bare(word).strip_prefix('/') == Some(name));
    leading || text.split_whitespace().any(|word| bare(word).strip_prefix('$') == Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_are_invoked_by_slash_or_dollar() {
        assert!(mentions_skill("/deploy now", "deploy"));
        assert!(mentions_skill("please use $deploy, then test", "deploy"));
        assert!(mentions_skill("(see $deploy)", "deploy"));
        assert!(mentions_skill("/deploy", "deploy"));
        assert!(!mentions_skill("run deploy", "deploy"));
        assert!(!mentions_skill("fix /deploy docs", "deploy"));
        assert!(!mentions_skill("$deployer", "deploy"));
    }
}
