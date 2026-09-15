//! Running the calls of a response: commands and waits, their results, and
//! the exits of processes left running.

use std::path::Path;
use std::time::{Duration, Instant};

use super::{Agent, Moment, Notice, Origin, PROGRESS_INTERVAL, Phase, Update, ended, inbox, label};
use crate::bash::{Action, CANCELLED, Exit, Header, Outcome, Procs, Tool, ToolCall, Wait};
use crate::item::{Content, Item};
use crate::prompt;
use crate::store::{self, Record};

/// The longest calls keep a message for the agent's next step waiting.
const NEXT_WAIT: Duration = Duration::from_secs(3);
/// Output lines a running call shows live.
const PROGRESS_LINES: usize = 3;

/// A call of the response being run.
pub(super) struct Call {
    id: String,
    kind: CallKind,
    /// The catalog skill whose SKILL.md this command reads.
    skill: Option<usize>,
    stage: Stage,
    progress_at: Instant,
    /// New output arrived that live updates have not shown yet.
    progress_due: bool,
}

/// What a call does, where that changes how it ends: a command it started is
/// interrupted when the turn is cancelled, a stop succeeds however the process
/// exits, and nothing hurries a stop.
#[derive(Clone, Copy, PartialEq)]
enum CallKind {
    Start,
    Kill,
    Other,
}

/// How far a call has got.
enum Stage {
    /// Waiting on its process.
    Waiting(Wait),
    /// Waiting for anything to happen in the background, since `since`.
    Idle { since: Instant, until: Instant },
    /// Ended, with its output for the history.
    Ended(Content),
}

impl Call {
    /// Whether the call waits on process `id`.
    pub(super) fn waits_on(&self, id: u32) -> bool {
        matches!(&self.stage, Stage::Waiting(wait) if wait.id == id)
    }

    /// When the call needs attention next: its wait ends or output is due to
    /// show.
    pub(super) fn deadline(&self, procs: &Procs) -> Option<Instant> {
        match &self.stage {
            Stage::Waiting(wait) => {
                let deadline = procs.deadline(wait);
                let progress = self.progress_due.then_some(self.progress_at + PROGRESS_INTERVAL);
                Some(progress.map_or(deadline, |progress| progress.min(deadline)))
            }
            Stage::Idle { until, .. } => Some(*until),
            Stage::Ended(_) => None,
        }
    }
}

impl Agent {
    pub(super) fn run_tools(&mut self, requested: Vec<ToolCall>) {
        if let Some(error) = self.log_error.take() {
            for request in &requested {
                let output = format!("error: not run: {error}").into();
                self.push(Item::output(&request.id, output), Origin::User);
            }
            // If these answers cannot be recorded either, a resumed session
            // answers the calls itself.
            self.log_error = None;
            return self.fail_turn(error);
        }
        let now = Instant::now();
        let mut calls = Vec::with_capacity(requested.len());
        for request in requested {
            let (kind, skill) = match &request.tool {
                Tool::Bash(Action::Start { command, .. }) => {
                    let skills = &self.instructions.skills;
                    (
                        CallKind::Start,
                        skills.iter().position(|skill| reads_file(command, &skill.path)),
                    )
                }
                Tool::Bash(Action::Kill { .. }) => (CallKind::Kill, None),
                _ => (CallKind::Other, None),
            };
            let started = match &request.tool {
                Tool::Bash(action) => self.start(action, now),
                // Models trained on other agents may call a tool agt lacks.
                Tool::Unknown(name) if self.vision => Err(format!(
                    "unknown tool {name:?}; the only tool is bash, where `agt view <file>` shows you an image"
                )),
                Tool::Unknown(name) => Err(format!("unknown tool {name:?}; the only tool is bash")),
                Tool::Invalid(error) => Err(error.clone()),
            };
            let id = request.id.clone();
            self.updates.push(Update::ToolStart(request));
            let stage = started.unwrap_or_else(|error| {
                Stage::Ended(ended(
                    &mut self.updates,
                    &id,
                    format!("error: {error}").into(),
                    Outcome::Failed,
                ))
            });
            calls.push(Call { id, kind, skill, stage, progress_at: now, progress_due: false });
        }
        self.phase = Phase::Tools(calls);
        self.hurry(now);
        self.poll_calls(now);
    }

    /// Does the immediate part of `action` and returns what its call waits for.
    fn start(&mut self, action: &Action, now: Instant) -> Result<Stage, String> {
        let wait = match action {
            Action::Start { command, wait } => {
                let started = self.procs.run(command, *wait, now)?;
                let command = command.as_str().into();
                self.record(&Record::Proc { at: store::now(), id: started.id, command });
                started
            }
            Action::Input { id, input, wait } => self.procs.type_into(*id, input, *wait, now)?,
            Action::Read { id, wait } => self.procs.read(*id, *wait, now)?,
            Action::Kill { id } => self.procs.stop(*id, now)?,
            Action::Wait { wait } => return Ok(Stage::Idle { since: now, until: now + *wait }),
        };
        Ok(Stage::Waiting(wait))
    }

    /// Keeps a message for the next step from waiting on calls for longer than
    /// `NEXT_WAIT`. A command still running then keeps running, and its exit
    /// is reported as usual; a wait for anything ends at once.
    pub(super) fn hurry(&mut self, now: Instant) {
        if !self.inbox.next_waits() {
            return;
        }
        let Phase::Tools(calls) = &mut self.phase else {
            return;
        };
        // A stop waits only for the process to go, which takes as long as it takes.
        for call in calls.iter_mut().filter(|call| call.kind != CallKind::Kill) {
            match &mut call.stage {
                Stage::Waiting(wait) => wait.hurry(now + NEXT_WAIT),
                Stage::Idle { until, .. } => *until = now,
                Stage::Ended(_) => {}
            }
        }
    }

    pub(super) fn on_output(&mut self, id: u32) {
        let now = Instant::now();
        self.procs.on_output(id, now);
        if let Phase::Tools(calls) = &mut self.phase {
            for call in calls.iter_mut().filter(|call| call.waits_on(id)) {
                call.progress_due = true;
            }
        }
        self.poll_calls(now);
    }

    /// Collects the results of calls whose waits are over, and shows the
    /// output of the others as it arrives.
    pub(super) fn poll_calls(&mut self, now: Instant) {
        self.procs.tick(now);
        let Phase::Tools(calls) = &mut self.phase else {
            return;
        };
        for call in calls.iter_mut() {
            let output = match &call.stage {
                Stage::Ended(_) => continue,
                Stage::Idle { since, until } => {
                    let happened = self.inbox.has_notices() || self.inbox.next_waits();
                    if !happened && now < *until {
                        continue;
                    }
                    let mut output =
                        Content::from(Header::Waited(now.duration_since(*since)).to_string());
                    if let Some(notices) = self.inbox.take_notices() {
                        output.push_str("\n");
                        output.append(notices);
                    }
                    ended(&mut self.updates, &call.id, output, Outcome::Ok)
                }
                Stage::Waiting(wait) => {
                    // Output within the interval shows once it passes, even if
                    // the process then goes quiet.
                    if call.progress_due && now >= call.progress_at + PROGRESS_INTERVAL {
                        call.progress_due = false;
                        call.progress_at = now;
                        let lines = self.procs.tail(wait.id, PROGRESS_LINES);
                        self.updates.push(Update::ToolProgress { call_id: call.id.clone(), lines });
                    }
                    if !self.procs.ready(wait, now) {
                        continue;
                    }
                    let report = self.procs.finish(wait, now);
                    let mut output = report.output;
                    // A skill read with bash gets its instructions in full, in a
                    // form that masking keeps and compaction carries forward.
                    if let Some(index) = call.skill
                        && report.exit.is_some_and(Exit::success)
                    {
                        let skill = &self.instructions.skills[index];
                        let header = output.text().lines().next().unwrap_or_default().to_owned();
                        if self.context.skills.contains(&skill.name) {
                            output = format!(
                                "{header}\nThe {} skill is already loaded in this conversation.",
                                skill.name
                            )
                            .into();
                        } else if let Ok(block) = prompt::skill_block(skill) {
                            output = format!("{header}\n{block}").into();
                            self.context.skills.push(skill.name.clone());
                        }
                    }
                    let ok = call.kind == CallKind::Kill || report.exit.is_none_or(Exit::success);
                    let outcome = if ok { Outcome::Ok } else { Outcome::Failed };
                    ended(&mut self.updates, &call.id, output, outcome)
                }
            };
            call.stage = Stage::Ended(output);
        }
        if calls.iter().all(|call| matches!(call.stage, Stage::Ended(_))) {
            self.tools_done();
        }
    }

    /// Adds every call's output to the history, with the notices not yet told
    /// after the last one, and sends the next request.
    fn tools_done(&mut self) {
        let Phase::Tools(calls) = std::mem::replace(&mut self.phase, Phase::Idle) else {
            return;
        };
        let count = calls.len();
        for (index, call) in calls.into_iter().enumerate() {
            let mut output = match call.stage {
                Stage::Ended(output) => output,
                Stage::Waiting(_) | Stage::Idle { .. } => Content::default(),
            };
            if index + 1 == count
                && let Some(notices) = self.inbox.take_notices()
            {
                output.push_str("\n\n");
                output.append(inbox::block(notices));
            }
            self.push(Item::output(&call.id, output.into_output()), Origin::User);
        }
        self.advance(Moment::MidTurn);
    }

    /// Ends the calls of a cancelled turn, interrupting the commands they
    /// started.
    pub(super) fn cancel_calls(&mut self, calls: Vec<Call>) {
        let now = Instant::now();
        for call in calls {
            let output = match call.stage {
                Stage::Ended(output) => output,
                Stage::Idle { since, .. } => {
                    let mut output =
                        Content::from(Header::Waited(now.duration_since(since)).to_string());
                    output.push_str(&format!("\n{CANCELLED}"));
                    ended(&mut self.updates, &call.id, output, Outcome::Cancelled)
                }
                Stage::Waiting(wait) => {
                    if call.kind == CallKind::Start {
                        self.procs.interrupt(wait.id);
                    }
                    let mut output = self.procs.finish(&wait, now).output;
                    output.push_str(&format!("\n{CANCELLED}"));
                    ended(&mut self.updates, &call.id, output, Outcome::Cancelled)
                }
            };
            self.push(Item::output(&call.id, output.into_output()), Origin::User);
        }
    }

    pub(super) fn on_exit(&mut self, id: u32, exit: Exit) {
        self.record(&Record::Exit { at: store::now(), id, exit });
        let news = self.procs.on_exit(id, exit);
        let awaited = matches!(&self.phase, Phase::Tools(calls) if calls.iter().any(|call| call.waits_on(id)));
        let task = self
            .procs
            .task(id)
            .map(|task| (label(task.command), task.started, task.log.to_path_buf()));
        if news
            && !awaited
            && let Some((command, started, log)) = task
        {
            // The notice carries the new output and the images shown in it,
            // saving the model a request to read them.
            let output = self.procs.take_unread(id);
            self.tell(Notice::Exited { id, command, exit, ran: started.elapsed(), log, output });
        }
        self.poll_calls(Instant::now());
    }
}

/// Whether `command` only prints `path`, which is how the catalog tells the
/// model to load a skill.
fn reads_file(command: &str, path: &Path) -> bool {
    command
        .trim()
        .strip_prefix("cat ")
        .is_some_and(|rest| Path::new(rest.trim().trim_matches(['"', '\''])) == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_reads_load_a_skill() {
        let path = Path::new("/skills/deploy/SKILL.md");
        assert!(reads_file("cat /skills/deploy/SKILL.md", path));
        assert!(reads_file(" cat '/skills/deploy/SKILL.md' ", path));
        assert!(!reads_file("wc -l /skills/deploy/SKILL.md", path));
        assert!(!reads_file("cat /skills/deploy/SKILL.md | head", path));
        assert!(!reads_file("cat /skills/deploy/SKILL.md && git diff", path));
    }
}
