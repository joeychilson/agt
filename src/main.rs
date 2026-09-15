//! agt: a coding agent whose one tool is bash, a real terminal.
//!
//! It speaks the Responses API to five providers, runs every command in a
//! pseudo-terminal of its own, reads web pages, sees images and calls MCP
//! servers through commands of its own, compacts context for sessions that last
//! weeks, reads AGENTS.md and Agent Skills, and serves a terminal UI, print mode
//! and the Agent Client Protocol over one agent.

mod acp;
mod agent;
mod auth;
mod bash;
mod cli;
mod config;
mod control;
mod fetch;
mod image;
mod item;
mod llm;
mod mcp;
mod models;
mod prompt;
mod provider;
mod store;
mod tui;

use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Instant;

fn main() -> ExitCode {
    cli::run(std::env::args_os().skip(1))
}

/// Waits for the next event until `deadline`. `Ok(None)` means the deadline
/// passed; with no deadline it waits as long as it takes.
pub(crate) fn recv<T>(
    events: &mpsc::Receiver<T>,
    deadline: Option<Instant>,
) -> Result<Option<T>, mpsc::RecvError> {
    let Some(deadline) = deadline else {
        return events.recv().map(Some);
    };
    match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(event) => Ok(Some(event)),
        Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(mpsc::RecvError),
    }
}
