//! A session as text: what `agt -p` prints while the agent works, and what
//! `agt sessions show` prints of a saved or running session.
//!
//! Each command the agent runs shows on a line of its own, then how it ended
//! and the log file that holds all of its output, so a reader follows the
//! work at a glance and reads output only where it matters. A failed command
//! adds its last lines, where the error usually is. The model's reasoning is
//! left out.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agent::{Origin, Update};
use crate::bash::{self, Action, CANCELLED, Header, Outcome, Tool, ToolCall, elapsed};
use crate::image;
use crate::item::content_text;

/// Last output lines a failed command shows.
const FAILURE_LINES: usize = 5;
/// The most characters of a command its line shows.
const HEADLINE_CHARS: usize = 200;

/// What a run of lines shows. Blank lines set runs apart, except runs of
/// work, which follow one another.
#[derive(Clone, Copy, PartialEq)]
enum Block {
    /// A message the model received.
    User,
    /// What the model wrote.
    Text,
    /// Commands, and lines from agt.
    Work,
}

/// A call whose line is waiting to show, or shows last.
struct Call {
    id: String,
    line: String,
}

pub(super) struct Transcript<W> {
    out: W,
    /// The session's directory, which holds the logs of its commands.
    dir: PathBuf,
    /// The session's working directory, which command lines leave out.
    cwd: PathBuf,
    /// The text of the response being written.
    text: String,
    /// The text of the last response, held until what follows shows whether
    /// it ended the turn.
    reply: Option<String>,
    /// The call whose line is the last one shown, so its ending follows it.
    open: Option<Call>,
    /// Calls whose lines are not the last one shown, which show again with
    /// their ending: another call's line came first, or came after.
    waiting: Vec<Call>,
    last: Option<Block>,
    commands: usize,
    failed: usize,
}

impl<W: Write> Transcript<W> {
    /// The transcript of the session kept in `dir` that runs in `cwd`,
    /// written to `out`.
    pub(super) fn new(out: W, dir: &Path, cwd: &Path) -> Self {
        Self {
            out,
            dir: dir.to_path_buf(),
            cwd: cwd.to_path_buf(),
            text: String::new(),
            reply: None,
            open: None,
            waiting: Vec::new(),
            last: None,
            commands: 0,
            failed: 0,
        }
    }

    pub(super) fn update(&mut self, update: Update) -> io::Result<()> {
        match update {
            Update::Text(text) => self.text.push_str(&text),
            // The attempt that wrote the text failed.
            Update::Reset => self.text.clear(),
            Update::ResponseEnd => {
                self.show_reply()?;
                let text = std::mem::take(&mut self.text);
                self.reply = Some(text).filter(|text| !text.trim().is_empty());
            }
            Update::Thinking(_)
            | Update::ToolProgress { .. }
            | Update::Usage(_)
            | Update::TurnEnd(_) => {}
            Update::User { text, images, origin } => {
                self.show_reply()?;
                self.user(&text, &images, origin)?;
            }
            Update::ToolStart(call) => {
                self.show_reply()?;
                self.start(&call)?;
            }
            Update::ToolEnd { call_id, output, outcome } => {
                self.show_reply()?;
                self.end(&call_id, &output, outcome)?;
            }
            update @ (Update::Notice(_) | Update::Compacting | Update::Compacted { .. }) => {
                self.show_reply()?;
                let text = update.notice().expect("notices and compactions are told in words");
                self.line(&format!("agt: {}", first_line(&text)))?;
            }
            Update::Error(text) => {
                self.show_reply()?;
                self.line(&format!("agt: error: {text}"))?;
            }
        }
        Ok(())
    }

    /// Shows a line of agt's own among the work.
    pub(super) fn line(&mut self, text: &str) -> io::Result<()> {
        self.write(Block::Work, text)
    }

    /// Writes the reply that ended the turn to `to`, apart from the rest.
    pub(super) fn reply_to(&mut self, mut to: impl Write) -> io::Result<()> {
        let Some(reply) = self.reply.take() else {
            return Ok(());
        };
        self.separate(Block::Text)?;
        writeln!(to, "{}", reply.trim_end())?;
        to.flush()
    }

    /// Shows the last response's text, and flushes what was written.
    pub(super) fn finish(&mut self) -> io::Result<()> {
        self.show_reply()?;
        self.out.flush()
    }

    /// How many commands ran and how many failed, such as `3 commands, 1
    /// failed`, once any ran.
    pub(super) fn counts(&self) -> Option<String> {
        let commands = match self.commands {
            0 => return None,
            1 => "1 command".to_owned(),
            count => format!("{count} commands"),
        };
        Some(match self.failed {
            0 => commands,
            failed => format!("{commands}, {failed} failed"),
        })
    }

    fn show_reply(&mut self) -> io::Result<()> {
        match self.reply.take() {
            Some(reply) => self.write(Block::Text, reply.trim_end()),
            None => Ok(()),
        }
    }

    fn user(&mut self, text: &str, images: &[String], origin: Origin) -> io::Result<()> {
        let mut lines = text.trim_end().lines();
        let from = match origin {
            Origin::User => "",
            Origin::Send => "[agt send] ",
        };
        let mut shown = format!("❯ {from}{}", lines.next().unwrap_or_default());
        for line in lines {
            shown.push('\n');
            if !line.is_empty() {
                shown.push_str(&format!("  {line}"));
            }
        }
        for reference in images {
            shown.push_str(&format!("\n  [image {}]", self.dir.join(reference).display()));
        }
        self.write(Block::User, &shown)
    }

    fn start(&mut self, call: &ToolCall) -> io::Result<()> {
        let marker = if matches!(call.tool, Tool::Bash(Action::Start { .. })) { '$' } else { '>' };
        let headline = call.headline(&self.cwd);
        let mut chars = headline.chars();
        let mut line = format!("{marker} ");
        line.extend(chars.by_ref().take(HEADLINE_CHARS));
        if chars.next().is_some() {
            line.push('…');
        }
        let call = Call { id: call.id.clone(), line };
        // Calls of one response start together; the first shows now, and
        // the others with their endings.
        if self.open.is_some() {
            self.waiting.push(call);
            return Ok(());
        }
        self.write(Block::Work, &call.line)?;
        self.open = Some(call);
        Ok(())
    }

    fn end(&mut self, id: &str, output: &Value, outcome: Outcome) -> io::Result<()> {
        self.commands += 1;
        self.failed += usize::from(outcome == Outcome::Failed);
        match self.open.take() {
            Some(open) if open.id == id => {}
            open => {
                self.waiting.extend(open);
                // A call started before the session was compacted has no line.
                if let Some(index) = self.waiting.iter().position(|call| call.id == id) {
                    let call = self.waiting.remove(index);
                    self.write(Block::Work, &call.line)?;
                }
            }
        }
        for line in ending(&content_text(output), outcome, &self.dir) {
            self.write(Block::Work, &line)?;
        }
        Ok(())
    }

    fn write(&mut self, block: Block, text: &str) -> io::Result<()> {
        self.separate(block)?;
        writeln!(self.out, "{text}")
    }

    /// Begins a line of `block`, after a blank line where one sets it apart.
    fn separate(&mut self, block: Block) -> io::Result<()> {
        // A line under the open call's line means its ending shows it again.
        if let Some(open) = self.open.take() {
            self.waiting.push(open);
        }
        if self.last.is_some_and(|last| last != block || block != Block::Work) {
            writeln!(self.out)?;
        }
        self.last = Some(block);
        Ok(())
    }
}

/// The lines under a call that say how it ended: its status, how long it ran
/// and its log file, then the images it showed, or a failed call's last
/// lines. A wait shows what happened while it waited.
fn ending(result: &str, outcome: Outcome, dir: &Path) -> Vec<String> {
    let Some((header, output)) = Header::parse(result) else {
        // An error agt reported itself, such as arguments the tool rejects.
        return vec![format!("  {}", first_line(result))];
    };
    let lines =
        output.lines().map(str::trim_end).filter(|line| !line.is_empty() && *line != CANCELLED);
    let (status, shown): (String, Vec<&str>) = match header {
        Header::Process { id, exit, ran, .. } => {
            let state = match (outcome, exit) {
                (Outcome::Cancelled, _) => "cancelled".to_owned(),
                (_, None) => "running".to_owned(),
                (_, Some(exit)) => exit.to_string(),
            };
            let log = bash::log_path(dir, id);
            let status = format!("  {state} · {} · {}", elapsed(ran), log.display());
            let shown = match outcome {
                Outcome::Failed => {
                    let lines: Vec<&str> = lines.collect();
                    lines[lines.len().saturating_sub(FAILURE_LINES)..].to_vec()
                }
                Outcome::Ok | Outcome::Cancelled => {
                    lines.filter(|line| line.starts_with(image::HEADER)).collect()
                }
            };
            (status, shown)
        }
        Header::Waited(waited) => (format!("  waited {}", elapsed(waited)), lines.collect()),
    };
    std::iter::once(status).chain(shown.into_iter().map(|line| format!("    {line}"))).collect()
}

/// The first line of `text`, marked when more follows.
fn first_line(text: &str) -> String {
    let text = text.trim();
    match text.split_once('\n') {
        Some((first, _)) => format!("{} …", first.trim_end()),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::Stop;

    fn transcript() -> Transcript<Vec<u8>> {
        Transcript::new(Vec::new(), Path::new("/agt/sessions/1"), Path::new("/work"))
    }

    fn start(id: &str, arguments: Value) -> Update {
        Update::ToolStart(ToolCall::new(id.into(), "bash", arguments.to_string()))
    }

    fn end(id: &str, output: &str, outcome: Outcome) -> Update {
        Update::ToolEnd { call_id: id.into(), output: output.into(), outcome }
    }

    fn text(transcript: Transcript<Vec<u8>>) -> String {
        String::from_utf8(transcript.out).expect("UTF-8")
    }

    #[test]
    fn calls_show_how_they_ended_with_their_logs_and_a_failures_last_lines() {
        let mut transcript = transcript();
        let failing: String = (1..=8).map(|n| format!("line {n}\n")).collect();
        for update in [
            Update::Text("Checking.".into()),
            Update::ResponseEnd,
            start("a", json!({ "command": "cd /work && cargo test" })),
            start("b", json!({ "command": "agt view shot.png" })),
            end("b", "[id 2 · exit 0 · 0.4s]\n[image /work/shot.png · 4x2]", Outcome::Ok),
            end("a", &format!("[id 1 · exit 101 · 12.3s]\n{failing}"), Outcome::Failed),
            start("c", json!({ "id": 1, "kill": true })),
            end("c", "error: no process with id 1", Outcome::Failed),
            start("d", json!({ "wait": 60 })),
            end(
                "d",
                "[waited 3.0s]\n[2026-09-15 14:02 UTC] process 3 (make) finished with exit 0",
                Outcome::Ok,
            ),
            Update::Notice("elided old tool output\nand more".into()),
            Update::Text("Fixed.".into()),
            Update::ResponseEnd,
            Update::TurnEnd(Stop::EndTurn),
        ] {
            transcript.update(update).expect("written");
        }
        let mut reply = Vec::new();
        transcript.reply_to(&mut reply).expect("written");
        assert_eq!(transcript.counts().as_deref(), Some("4 commands, 2 failed"));
        assert_eq!(String::from_utf8(reply).expect("UTF-8"), "Fixed.\n");
        assert_eq!(
            text(transcript),
            "Checking.\n\n\
             $ cargo test\n\
             $ agt view shot.png\n  exit 0 · 0.4s · /agt/sessions/1/procs/2.log\n    [image /work/shot.png · 4x2]\n\
             $ cargo test\n  exit 101 · 12.3s · /agt/sessions/1/procs/1.log\n    line 4\n    line 5\n    line 6\n    line 7\n    line 8\n\
             > kill 1\n  error: no process with id 1\n\
             > wait up to 1m00s\n  waited 3.0s\n    [2026-09-15 14:02 UTC] process 3 (make) finished with exit 0\n\
             agt: elided old tool output …\n\n"
        );
    }

    #[test]
    fn replayed_messages_and_replies_read_in_order() {
        let mut transcript = transcript();
        for update in [
            Update::User {
                text: "fix it\n\nplease".into(),
                images: vec!["images/a-4x2.png".into()],
                origin: Origin::User,
            },
            start("a", json!({ "command": "make" })),
            end("a", "[id 1 · running · 30.0s · you will be told when it exits]", Outcome::Ok),
            Update::Text("Started.".into()),
            Update::ResponseEnd,
            Update::User {
                text: "stop when it passes".into(),
                images: Vec::new(),
                origin: Origin::Send,
            },
        ] {
            transcript.update(update).expect("written");
        }
        transcript.finish().expect("written");
        assert_eq!(
            text(transcript),
            "❯ fix it\n\n  please\n  [image /agt/sessions/1/images/a-4x2.png]\n\n\
             $ make\n  running · 30.0s · /agt/sessions/1/procs/1.log\n\n\
             Started.\n\n\
             ❯ [agt send] stop when it passes\n"
        );
    }
}
