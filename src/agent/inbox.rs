//! What waits for the model: messages sent while the agent worked, notices of
//! what happened in the background, and when each is due.

use std::path::PathBuf;
use std::time::Duration;

use super::{Delivery, Moment, Queued, Stop};
use crate::bash::{Exit, elapsed};
use crate::item::{Content, Item, Kind};
use crate::store;

/// How a block of notices opens and closes.
const NOTICES: [&str; 2] = ["<background>", "</background>"];

/// Something that happened that the model should know.
#[derive(Debug, PartialEq)]
pub(crate) enum Notice {
    /// A process left running exited, with the output the model had not read.
    Exited {
        id: u32,
        command: String,
        exit: Exit,
        ran: Duration,
        log: PathBuf,
        output: Option<Content>,
    },
    /// Processes that were running when the previous agt of a resumed session
    /// stopped, and stopped with it.
    Lost(Vec<(u32, String)>),
    /// The user stopped a process.
    Stopped {
        id: u32,
        command: String,
    },
    /// The user added an MCP server, which `target` serves.
    ServerAdded {
        name: String,
        target: String,
    },
    ServerRemoved {
        name: String,
    },
}

impl Notice {
    /// Whether the notice starts a turn when the agent is idle.
    pub(super) fn wakes(&self) -> bool {
        matches!(self, Self::Exited { .. })
    }

    /// The notice in a line, as frontends show it.
    pub(crate) fn summary(&self) -> String {
        match self {
            Self::Exited { id, command, exit, .. } => {
                format!("process {id} ({command}) finished with {exit}")
            }
            Self::Lost(processes) => {
                let named: Vec<String> =
                    processes.iter().map(|(id, command)| format!("{id} ({command})")).collect();
                format!("these processes stopped when agt last did: {}", named.join(", "))
            }
            Self::Stopped { id, command } => format!("the user stopped process {id} ({command})"),
            Self::ServerAdded { name, target } => format!(
                "the user added MCP server {name} ({target}); agt mcp tools {name} lists its tools"
            ),
            Self::ServerRemoved { name } => format!("the user removed MCP server {name}"),
        }
    }

    /// What the model reads.
    fn content(self) -> Content {
        match self {
            Self::Exited { id, command, exit, ran, log, output } => {
                let mut content = Content::from(format!(
                    "process {id} ({command}) finished with {exit} after {}; its log is {}",
                    elapsed(ran),
                    log.display()
                ));
                if let Some(output) = output {
                    content.push_str("\n");
                    content.append(output);
                }
                content
            }
            notice => notice.summary().into(),
        }
    }
}

#[derive(Default)]
pub(super) struct Inbox {
    /// Oldest first.
    messages: Vec<Queued>,
    /// Oldest first, each with the time it happened.
    notices: Vec<(u64, Notice)>,
    /// Whether a notice asked for a turn once the current one ends.
    wake: bool,
}

impl Inbox {
    pub(super) fn messages(&self) -> &[Queued] {
        &self.messages
    }

    pub(super) fn push(&mut self, message: Queued) {
        self.messages.push(message);
    }

    /// Takes back the newest message.
    pub(super) fn pop(&mut self) -> Option<Queued> {
        self.messages.pop()
    }

    /// Takes every message, which is then never delivered.
    pub(super) fn take_messages(&mut self) -> Vec<Queued> {
        std::mem::take(&mut self.messages)
    }

    /// Keeps `notice`, which happened `at`, for the next request; with `wake`,
    /// a turn that ends normally is followed by one that tells it.
    pub(super) fn notify(&mut self, at: u64, notice: Notice, wake: bool) {
        self.notices.push((at, notice));
        self.wake |= wake;
    }

    pub(super) fn has_notices(&self) -> bool {
        !self.notices.is_empty()
    }

    /// Whether a message waits for the agent's next step.
    pub(super) fn next_waits(&self) -> bool {
        self.messages.iter().any(|message| message.delivery == Delivery::Next)
    }

    /// Whether a turn that ended with `stop` is followed by another: messages
    /// are still owed an answer, and a notice may have asked for one.
    pub(super) fn continues(&self, stop: Stop) -> bool {
        !self.messages.is_empty() || (self.wake && stop == Stop::EndTurn)
    }

    /// The notices not yet told, each starting a line with the time it happened.
    pub(super) fn take_notices(&mut self) -> Option<Content> {
        if self.notices.is_empty() {
            return None;
        }
        self.wake = false;
        let mut content = Content::default();
        for (index, (at, notice)) in std::mem::take(&mut self.notices).into_iter().enumerate() {
            if index > 0 {
                content.push_str("\n");
            }
            content.append(stamped(at, notice.content()));
        }
        Some(content)
    }

    /// Takes the messages due at `moment`. Messages for the next step are due
    /// at any moment. Once none waits, the start of a turn takes the oldest
    /// message for later, so each gets a turn of its own.
    pub(super) fn take_due(&mut self, moment: Moment) -> Vec<Queued> {
        if self.next_waits() {
            let (next, later) = std::mem::take(&mut self.messages)
                .into_iter()
                .partition(|message| message.delivery == Delivery::Next);
            self.messages = later;
            next
        } else if moment == Moment::TurnStart && !self.messages.is_empty() {
            vec![self.messages.remove(0)]
        } else {
            Vec::new()
        }
    }
}

/// `content` starting with the time `at` it happened, as a notice is told.
pub(super) fn stamped(at: u64, content: Content) -> Content {
    let mut stamped = Content::from(format!("[{}] ", store::timestamp(at)));
    stamped.append(content);
    stamped
}

/// `notices` as the block that follows a tool result or makes a message of
/// its own.
pub(super) fn block(notices: Content) -> Content {
    let mut block = Content::from(format!("{}\n", NOTICES[0]));
    block.append(notices);
    block.push_str(&format!("\n{}", NOTICES[1]));
    block
}

/// Whether `item` is a message of notices, which holds no words of the user's.
pub(crate) fn is_notices(item: &Item) -> bool {
    item.kind() == Kind::User
        && item.content()[0]["text"].as_str().is_some_and(|text| text.starts_with(NOTICES[0]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Message;

    fn queued(typed: &str, delivery: Delivery) -> Queued {
        Queued { message: Message::text(typed), delivery }
    }

    fn typed(messages: Vec<Queued>) -> Vec<String> {
        messages.into_iter().map(|queued| queued.message.typed).collect()
    }

    #[test]
    fn next_step_messages_go_first_and_later_ones_get_a_turn_each() {
        let mut inbox = Inbox::default();
        for (text, delivery) in [
            ("later 1", Delivery::Later),
            ("next 1", Delivery::Next),
            ("later 2", Delivery::Later),
            ("next 2", Delivery::Next),
        ] {
            inbox.push(queued(text, delivery));
        }
        assert_eq!(typed(inbox.take_due(Moment::MidTurn)), ["next 1", "next 2"]);
        assert_eq!(typed(inbox.take_due(Moment::MidTurn)), Vec::<String>::new());
        assert!(inbox.continues(Stop::Error), "waiting messages are owed an answer");
        assert_eq!(typed(inbox.take_due(Moment::TurnStart)), ["later 1"]);
        assert_eq!(typed(inbox.take_due(Moment::TurnStart)), ["later 2"]);
        assert!(!inbox.continues(Stop::EndTurn));
    }

    #[test]
    fn notices_carry_their_time_wake_only_a_turn_that_ended_normally_and_are_told_once() {
        let mut inbox = Inbox::default();
        inbox.notify(0, Notice::Stopped { id: 1, command: "npm run dev".into() }, false);
        assert!(!inbox.continues(Stop::EndTurn), "this notice waits for the next request");
        let exited = Notice::Exited {
            id: 2,
            command: "cargo test".into(),
            exit: Exit::Code(101),
            ran: Duration::from_secs(192),
            log: PathBuf::from("/s/procs/2.log"),
            output: Some("test result: FAILED".into()),
        };
        assert!(exited.wakes());
        inbox.notify(60_000, exited, true);
        assert!(inbox.continues(Stop::EndTurn));
        assert!(!inbox.continues(Stop::Cancelled));
        let notices = inbox.take_notices().expect("notices");
        assert_eq!(
            block(notices).text(),
            "<background>\n\
             [1970-01-01 00:00 UTC] the user stopped process 1 (npm run dev)\n\
             [1970-01-01 00:01 UTC] process 2 (cargo test) finished with exit 101 after 3m12s; its log is /s/procs/2.log\n\
             test result: FAILED\n\
             </background>"
        );
        assert_eq!(inbox.take_notices(), None);
        assert!(!inbox.continues(Stop::EndTurn), "notices already told start no turn");
    }

    #[test]
    fn messages_of_notices_are_told_from_the_users() {
        let notices = block("[t] process 1 exited".into());
        assert!(is_notices(&Item::user(notices.into_parts())));
        assert!(!is_notices(&Item::user(vec![crate::item::input_text("hello")])));
        assert!(!is_notices(&Item::output("c1", "<background>".into())));
    }
}
