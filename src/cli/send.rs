//! `agt send`: a message for a running session's agent, from any process.

use std::io::{self, IsTerminal, Read};
use std::process::ExitCode;

use lexopt::{Arg, Parser, ValueExt};

use super::{Error, failed, show_help};
use crate::control::{self, Request};

pub(super) const HELP: &str = "\
Usage: agt send [--later] <session> <message>

Sends a message to a running agt session, whether it runs in the terminal UI,
an editor or agt -p. The agent receives it at its next step, as a message the
user sends while it works, or with --later once it is done. A message of - is
read from stdin, and /compact [focus] compacts the session's context.

A session that is not running is continued with agt -p -r <session> <prompt>.";

pub(super) fn run(mut args: Parser) -> Result<ExitCode, Error> {
    let (mut later, mut words) = (false, Vec::new());
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('h') | Arg::Long("help") => return show_help(HELP),
            Arg::Long("later") => later = true,
            Arg::Value(word) => words.push(word.string()?),
            arg => return Err(arg.unexpected().into()),
        }
    }
    let mut words = words.into_iter();
    let (Some(id), Some(first)) = (words.next(), words.next()) else {
        return Err(Error::Usage("send takes a session's id and a message".into()));
    };
    let socket = control::path(&id).map_err(|error| match error.kind() {
        io::ErrorKind::InvalidInput => Error::Usage(error.to_string()),
        _ => failed(error),
    })?;
    let text = if first == "-" && words.len() == 0 {
        if io::stdin().is_terminal() {
            return Err(Error::Usage("a message of - is read from stdin; pipe it in".into()));
        }
        let mut text = String::new();
        io::stdin()
            .read_to_string(&mut text)
            .map_err(|error| failed(format!("cannot read the message: {error}")))?;
        text
    } else {
        std::iter::once(first).chain(words).collect::<Vec<_>>().join(" ")
    };
    if text.trim().is_empty() {
        return Err(Error::Usage("the message is empty".into()));
    }
    if !control::live(&id) {
        return Err(failed(format!(
            "session {id} is not running; continue it with agt -p -r {id} '<prompt>'"
        )));
    }
    control::ask(&socket, &Request::Send { text, later }).map_err(failed)?;
    let when = if later { "once its agent is done" } else { "for its next step" };
    println!("sent to session {id} {when}");
    Ok(ExitCode::SUCCESS)
}
