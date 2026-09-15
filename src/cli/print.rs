//! Print mode: running a prompt until the agent is done, without the terminal
//! UI. The reply goes to stdout and what the agent did to stderr, so a script
//! can take the reply alone, while an agent reading both sees the reply after
//! the work that led to it.

use std::io::{self, IsTerminal, Read};
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, mpsc};
use std::time::Instant;

use super::render::Transcript;
use super::{Error, failed};
use crate::agent::{Agent, Delivery, Message, Notify, Open, Origin, Stop, Update};
use crate::bash;
use crate::config::Config;

/// Runs `prompt`, or the prompt on stdin, in a new session or session `resume`.
pub(super) fn run(
    config: &Config,
    cwd: &Path,
    resume: Option<&str>,
    prompt: Option<String>,
) -> Result<ExitCode, Error> {
    let prompt = match prompt {
        Some(prompt) => prompt,
        None if !io::stdin().is_terminal() => {
            let mut prompt = String::new();
            io::stdin()
                .read_to_string(&mut prompt)
                .map_err(|error| failed(format!("cannot read the prompt: {error}")))?;
            prompt
        }
        None => String::new(),
    };
    if prompt.trim().is_empty() {
        return Err(Error::Usage("give a prompt, or pipe one on stdin".into()));
    }
    let (sender, events) = mpsc::channel();
    let notify: Notify = Arc::new(move |event| {
        let _ = sender.send(event);
    });
    let open = Open { cwd, resume, replay: None, autowake: false, servers: &[] };
    let mut agent = Agent::open(config, open, notify).map_err(failed)?;
    let started = Instant::now();
    let mut transcript = Transcript::new(io::stderr(), agent.dir(), cwd);
    let settings = agent.settings();
    let effort = settings.reasoning.map(|effort| format!(" {}", effort.as_str()));
    transcript
        .line(&format!(
            "agt: session {} · {}{} · {}",
            agent.session_id(),
            settings.model.id,
            effort.unwrap_or_default(),
            settings.endpoint.provider.spec().name
        ))
        .map_err(failed)?;
    agent.submit(Message::text(&prompt), Delivery::Next);
    loop {
        let deadline = agent.poll();
        let updates: Vec<Update> = agent.drain().collect();
        for update in updates {
            match update {
                // The prompt is the caller's own; messages sent to the session show.
                Update::User { origin: Origin::User, .. } => {}
                Update::TurnEnd(stop) => {
                    return finish(&agent, &mut transcript, stop, started).map_err(failed);
                }
                update => transcript.update(update).map_err(failed)?,
            }
        }
        match crate::recv(&events, deadline) {
            Ok(Some(event)) => agent.handle(event),
            Ok(None) => {}
            Err(_) => return Err(failed("the agent's event channel closed")),
        }
    }
}

/// Prints the reply that ended the turn, then a line saying how the turn
/// ended, and returns the exit code that says it.
fn finish(
    agent: &Agent,
    transcript: &mut Transcript<io::Stderr>,
    stop: Stop,
    started: Instant,
) -> io::Result<ExitCode> {
    transcript.reply_to(io::stdout().lock())?;
    let (ended, code) = match stop {
        Stop::EndTurn => ("done", ExitCode::SUCCESS),
        Stop::Cancelled => ("cancelled", ExitCode::FAILURE),
        Stop::MaxTokens => ("stopped at the output token limit", ExitCode::FAILURE),
        Stop::Refusal => ("refused", ExitCode::FAILURE),
        Stop::Error => ("failed", ExitCode::FAILURE),
    };
    let mut summary = vec![format!("{ended} in {}", bash::elapsed(started.elapsed()))];
    summary.extend(transcript.counts());
    summary.extend(agent.usage().spent());
    summary.push(format!("session {}", agent.session_id()));
    transcript.line(&format!("agt: {}", summary.join(" · ")))?;
    Ok(code)
}
