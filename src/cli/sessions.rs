//! `agt sessions`: the sessions of this directory or of every one, the
//! transcript of a session, and following a session while it runs.

use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, SystemTime};

use lexopt::{Arg, Parser, ValueExt};

use super::render::Transcript;
use super::{Error, failed, show_help, working_directory};
use crate::item::Kind;
use crate::store::{self, Record};
use crate::{agent, config, control};

/// The most sessions listed, unless `--limit` says otherwise.
const LIMIT: usize = 20;
/// How often a followed session's log is read for what it added.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(200);

pub(super) const HELP: &str = "\
Usage:
  agt sessions [--all] [-n <count>]   list the sessions of this directory, newest first
  agt sessions show <id> [-f]         print a session's transcript

Options:
  -a, --all            list the sessions of every directory, with their directories
  -n, --limit <count>  list at most <count> sessions (default 20)
  -f, --follow         keep printing a running session until its agent is done

A running session says running where others say when they were last used. A
transcript shows each message sent, each command the agent ran with how it
ended and the log file that holds its output, and the agent's replies; its
full record is the log.jsonl the first line names. Continue a session with
agt -r <id>, or agt -p -r <id> <prompt>.";

pub(super) fn run(mut args: Parser) -> Result<ExitCode, Error> {
    if args.raw_args()?.next_if(|arg| arg == "show").is_some() {
        return show(args);
    }
    let (mut all, mut limit) = (false, LIMIT);
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('h') | Arg::Long("help") => return show_help(HELP),
            Arg::Short('a') | Arg::Long("all") => all = true,
            Arg::Short('n') | Arg::Long("limit") => limit = args.value()?.parse()?,
            arg => return Err(arg.unexpected().into()),
        }
    }
    let (home, cwd) = (config::home()?, working_directory()?);
    let listed =
        store::list(&home).map_err(|error| failed(format!("cannot list sessions: {error}")))?;
    // A session nobody wrote in has nothing to continue or read.
    let sessions: Vec<store::Listing> = listed
        .into_iter()
        .filter(|session| !session.title.is_empty() && (all || session.cwd == cwd))
        .collect();
    list(&mut io::stdout().lock(), &sessions, &cwd, all, limit).map_err(failed)?;
    Ok(ExitCode::SUCCESS)
}

/// Writes the first `limit` of `sessions` newest first, from every directory
/// with `all` and otherwise from `cwd`, then how to list the rest.
fn list(
    out: &mut impl Write,
    sessions: &[store::Listing],
    cwd: &Path,
    all: bool,
    limit: usize,
) -> io::Result<()> {
    match (sessions.is_empty(), all) {
        (true, true) => return writeln!(out, "no sessions yet"),
        (true, false) => {
            let cwd = cwd.display();
            return writeln!(out, "no sessions in {cwd}; agt sessions --all lists every one");
        }
        (false, _) => {}
    }
    let now = SystemTime::now();
    for session in sessions.iter().take(limit) {
        let when = if control::live(&session.id) { "running".to_owned() } else { session.ago(now) };
        if all {
            let place = session.cwd.display();
            writeln!(out, "{}  {when:>8}  {place}  {}", session.id, session.title)?;
        } else {
            writeln!(out, "{}  {when:>8}  {}", session.id, session.title)?;
        }
    }
    if sessions.len() > limit {
        let flag = if all { " --all" } else { "" };
        let (more, total) = (sessions.len() - limit, sessions.len());
        writeln!(out, "+{more} more; agt sessions{flag} -n {total} lists them")?;
    }
    Ok(())
}

/// Prints a session's transcript, following it while it runs with `-f`.
fn show(mut args: Parser) -> Result<ExitCode, Error> {
    let (mut id, mut follow) = (None, false);
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('h') | Arg::Long("help") => return show_help(HELP),
            Arg::Short('f') | Arg::Long("follow") => follow = true,
            Arg::Value(value) if id.is_none() => id = Some(value.string()?),
            arg => return Err(arg.unexpected().into()),
        }
    }
    let Some(id) = id else {
        return Err(Error::Usage("show takes a session's id, as agt sessions lists them".into()));
    };
    let saved = store::Saved::open(&config::home()?, &id).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => failed(format!("there is no session {id}")),
        io::ErrorKind::InvalidInput => Error::Usage(error.to_string()),
        _ => failed(format!("cannot read session {id}: {error}")),
    })?;
    let mut printer = Printer {
        transcript: Transcript::new(io::stdout().lock(), &saved.dir, &saved.cwd),
        written: Ok(()),
        working: false,
    };
    let heading = format!(
        "session {id} · {} · {}",
        saved.cwd.display(),
        saved.dir.join(store::LOG).display()
    );
    printer.transcript.line(&heading).map_err(failed)?;
    let unreadable = |error: io::Error| failed(format!("cannot read session {id}: {error}"));
    let mut offset = saved.read(0, &mut |record| printer.print(record)).map_err(unreadable)?;
    if follow {
        loop {
            // Checked before reading, so what a session logs as it ends is read.
            let live = control::live(&id);
            offset = saved.read(offset, &mut |record| printer.print(record)).map_err(unreadable)?;
            if !printer.working || !live || printer.written.is_err() {
                break;
            }
            // Printed text reaches a reader of this command as it arrives.
            let _ = io::stdout().flush();
            thread::sleep(FOLLOW_INTERVAL);
        }
    }
    printer.written.and_then(|()| printer.transcript.finish()).map_err(failed)?;
    Ok(ExitCode::SUCCESS)
}

/// Prints a session's records as they are read.
struct Printer<W> {
    transcript: Transcript<W>,
    /// Whether everything was written; nothing is after a failure.
    written: io::Result<()>,
    /// Whether a turn has started that has not ended.
    working: bool,
}

impl<W: Write> Printer<W> {
    fn print(&mut self, record: Record<'static>) {
        match &record {
            Record::Item { item, .. } => self.working |= item.kind() != Kind::Other,
            Record::Turn { .. } => self.working = false,
            _ => {}
        }
        for update in agent::replay(&record) {
            if self.written.is_ok() {
                self.written = self.transcript.update(update);
            }
        }
    }
}
