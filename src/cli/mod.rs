//! The command line: agt's commands, their options and help, and what they
//! print for people and agents.
//!
//! Every command reads its arguments with one parser and fails the same way:
//! a command line it cannot take exits with 2 and the command's help, and a
//! command that cannot do what it was asked exits with 1 and says why. What
//! commands print is plain text that reads the same in a terminal and in the
//! output of an agent's command.

mod login;
mod mcp;
mod models;
mod print;
mod render;
mod send;
mod sessions;
mod view;

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;

use lexopt::{Arg, Parser, ValueExt};

use crate::config::{self, Config, Overrides};
use crate::models::Effort;
use crate::{acp, store, tui};
pub(crate) use mcp::parse_server;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const ABOUT: &str = "agt: a coding agent whose one tool is a real terminal";

const USAGE: &str = "\
Usage:
  agt [options] [<prompt>]      open the terminal UI, sending <prompt> first if given
  agt -p [options] [<prompt>]   run <prompt> until the agent is done, and print what it did
  agt <command> [<args>]        run a command; agt <command> --help explains it";

const OPTIONS: &str = "\
Options:
  -p, --print             run without the terminal UI; <prompt> is read from stdin if not given
  -m, --model <id>        the model to use
      --provider <id>     the model's provider: openai, codex (a ChatGPT sign-in),
                          grok (a Grok sign-in), openrouter or vercel
  -e, --effort <level>    reasoning effort: none, minimal, low, medium, high, xhigh or max
  -c, --continue          continue the latest session in this directory
  -r, --resume <id>       continue session <id>
  -h, --help              show this help
  -V, --version           show the version

Print mode:
  The agent's reply goes to stdout and the rest to stderr: first the session id,
  then each command the agent runs with how it ended and the log file that holds
  its output, with the last lines of a failed one, and notices and errors. A last
  line says how the turn ended, what it cost and the session id again. agt exits
  with 0 when the agent is done, and with 1 otherwise. While it runs, agt send
  <id> <message> steers it; later, agt -p -r <id> <prompt> continues it and
  agt sessions show <id> prints it again.

Settings:
  agt uses the model and effort chosen in the terminal UI or with agt models use,
  saved in ~/.agt/config.json, and the sign-ins and API keys agt login saves in
  ~/.agt/auth.json. These variables override them, and the options override the
  variables:

  AGT_PROVIDER          provider, as --provider takes it
  AGT_MODEL             model, as --model takes it
  AGT_REASONING         reasoning effort, as --effort takes it
  AGT_API_KEY           API key; otherwise the saved key or the provider's own
                        OPENAI_API_KEY, OPENROUTER_API_KEY or AI_GATEWAY_API_KEY
  AGT_BASE_URL          replaces the provider's endpoint; only AGT_API_KEY is sent to it
  AGT_CONTEXT_WINDOW    context window in tokens, for every model
  AGT_HOME              settings, sessions and the global AGENTS.md (default ~/.agt)";

const ACP: &str = "\
Usage: agt acp [-m <model>] [--provider <id>] [-e <effort>]

Serves the Agent Client Protocol over stdin and stdout, for editors such as
Zed and libraries such as TanStack AI. Sessions use the saved settings, which
the variables agt --help lists override, and the options override those.";

/// Why a command did not succeed.
#[derive(Debug, PartialEq)]
pub(crate) enum Error {
    /// The command line is not one the command takes; its help follows.
    Usage(String),
    /// The command could not do what it was asked.
    Failed(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (Self::Usage(message) | Self::Failed(message)) = self;
        f.write_str(message)
    }
}

impl From<lexopt::Error> for Error {
    fn from(error: lexopt::Error) -> Self {
        Self::Usage(error.to_string())
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// A command failed for `reason`.
fn failed(reason: impl fmt::Display) -> Error {
    Error::Failed(reason.to_string())
}

/// Prints a command's help, as `--help` asks.
fn show_help(help: &str) -> Result<ExitCode, Error> {
    println!("{help}");
    Ok(ExitCode::SUCCESS)
}

/// A command agt runs by name.
struct Command {
    name: &'static str,
    /// What it does, as agt's help lists it.
    about: &'static str,
    /// Its help, which `--help` and a command line it cannot take show.
    help: &'static str,
    run: fn(Parser) -> Result<ExitCode, Error>,
}

/// The commands, in the order agt's help lists them.
const COMMANDS: [Command; 8] = [
    Command {
        name: "sessions",
        about: "list sessions, print their transcripts and follow running ones",
        help: sessions::HELP,
        run: sessions::run,
    },
    Command {
        name: "send",
        about: "send a message to a running session",
        help: send::HELP,
        run: send::run,
    },
    Command {
        name: "models",
        about: "list the models you can use, and choose one",
        help: models::HELP,
        run: models::run,
    },
    Command {
        name: "mcp",
        about: "set up MCP servers and call their tools",
        help: mcp::HELP,
        run: mcp::run,
    },
    Command {
        name: "view",
        about: "show images to the agent, from its commands",
        help: view::HELP,
        run: view::run,
    },
    Command { name: "login", about: "sign in to a provider", help: login::HELP, run: login::login },
    Command {
        name: "logout",
        about: "forget a provider's sign-in or key",
        help: login::HELP,
        run: login::logout,
    },
    Command {
        name: "acp",
        about: "serve the Agent Client Protocol to an editor",
        help: ACP,
        run: serve,
    },
];

/// agt's help, with its commands.
fn help() -> String {
    let commands: String = COMMANDS
        .iter()
        .map(|command| format!("\n  {:<10}{}", command.name, command.about))
        .collect();
    format!(
        "{ABOUT}\n\n{USAGE}\n\nCommands:{commands}\n  {:<10}show the help of agt or of a command\n\n{OPTIONS}",
        "help"
    )
}

/// Runs agt with its arguments, after the program's name. Only the first
/// argument names a command, so a prompt may use the same words.
pub(crate) fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    // agt's own help is long, so a command line it cannot take points to it.
    let usage = format!("{USAGE}\n\nagt --help shows every command and option.");
    let mut args = args.into_iter().peekable();
    let first = args.peek().and_then(|arg| arg.to_str()).map(str::to_owned);
    if first.as_deref() == Some("help") {
        let result = match args.nth(1) {
            None => show_help(&help()),
            Some(topic) => match named(&topic) {
                Some(command) => show_help(command.help),
                None => Err(Error::Usage(format!("there is no command {topic:?}"))),
            },
        };
        return report("agt help", &usage, result);
    }
    match first.as_deref().and_then(named) {
        Some(command) => {
            let args = Parser::from_args(args.skip(1));
            report(&format!("agt {}", command.name), command.help, (command.run)(args))
        }
        None => report("agt", &usage, start(Parser::from_args(args))),
    }
}

/// The command named `name`.
fn named(name: impl AsRef<OsStr>) -> Option<&'static Command> {
    let name = name.as_ref();
    COMMANDS.iter().find(|command| name == command.name)
}

/// The exit code of a command `name` that ended with `result`, after saying
/// why it did not succeed, with its `help` when the command line was wrong.
fn report(name: &str, help: &str, result: Result<ExitCode, Error>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(Error::Usage(problem)) => {
            eprintln!("{name}: {problem}\n\n{help}");
            ExitCode::from(2)
        }
        Err(Error::Failed(problem)) => {
            eprintln!("{name}: {problem}");
            ExitCode::FAILURE
        }
    }
}

/// Where a session starts.
#[derive(Debug, Default, PartialEq)]
enum Resume {
    #[default]
    New,
    Latest,
    Id(String),
}

/// What agt is asked without a command, and `agt acp` with one.
#[derive(Debug, Default, PartialEq)]
struct Options {
    print: bool,
    overrides: Overrides,
    resume: Resume,
    prompt: Option<String>,
}

impl Options {
    /// Reads the options and the words of the prompt. Returns `None` once
    /// `help` or the version is printed.
    fn parse(mut args: Parser, help: &str) -> Result<Option<Self>, Error> {
        let mut options = Self::default();
        let mut words = Vec::new();
        while let Some(arg) = args.next()? {
            match arg {
                Arg::Short('h') | Arg::Long("help") => {
                    println!("{help}");
                    return Ok(None);
                }
                Arg::Short('V') | Arg::Long("version") => {
                    println!("agt {VERSION}");
                    return Ok(None);
                }
                Arg::Short('p') | Arg::Long("print") => options.print = true,
                Arg::Short('c') | Arg::Long("continue") => options.resume = Resume::Latest,
                Arg::Short('r') | Arg::Long("resume") => {
                    options.resume = Resume::Id(args.value()?.string()?);
                }
                Arg::Short('m') | Arg::Long("model") => {
                    options.overrides.model = Some(args.value()?.string()?);
                }
                Arg::Long("provider") => {
                    let id = args.value()?.string()?;
                    let provider = config::provider("--provider", &id).map_err(Error::Usage)?;
                    options.overrides.provider = Some(provider);
                }
                Arg::Short('e') | Arg::Long("effort") => {
                    let name = args.value()?.string()?;
                    let effort = config::effort("--effort", &name).map_err(Error::Usage)?;
                    options.overrides.effort = Some(effort);
                }
                Arg::Value(word) => words.push(word.string()?),
                arg => return Err(arg.unexpected().into()),
            }
        }
        options.prompt = (!words.is_empty()).then(|| words.join(" "));
        Ok(Some(options))
    }
}

/// Runs agt without a command: the terminal UI, or print mode.
fn start(args: Parser) -> Result<ExitCode, Error> {
    let Some(options) = Options::parse(args, &help())? else {
        return Ok(ExitCode::SUCCESS);
    };
    let config = load(options.overrides)?;
    let cwd = working_directory()?;
    let resume = match options.resume {
        Resume::New => None,
        Resume::Id(id) => Some(id),
        Resume::Latest => Some(
            store::latest(&config.home, &cwd)
                .map_err(|error| failed(format!("cannot list sessions: {error}")))?
                .ok_or_else(|| failed("there is no session to continue in this directory"))?,
        ),
    };
    if options.print {
        return print::run(&config, &cwd, resume.as_deref(), options.prompt);
    }
    tui::run(config, &cwd, resume.as_deref(), options.prompt, false).map_err(failed)
}

/// Serves the Agent Client Protocol.
fn serve(args: Parser) -> Result<ExitCode, Error> {
    let Some(options) = Options::parse(args, ACP)? else {
        return Ok(ExitCode::SUCCESS);
    };
    if options.print || options.resume != Resume::New || options.prompt.is_some() {
        return Err(Error::Usage("acp takes only --model, --provider and --effort".into()));
    }
    acp::run(&load(options.overrides)?).map_err(failed)?;
    Ok(ExitCode::SUCCESS)
}

/// The settings, with the command line's `overrides`. An effort the command
/// line names must be one the model takes.
fn load(overrides: Overrides) -> Result<Config, Error> {
    let effort = overrides.effort;
    let config = Config::load(config::home()?, overrides)?;
    if let (Some(effort), Some(model)) = (effort, &config.model) {
        check_effort(&model.id, crate::models::efforts(Some(model)), effort)?;
    }
    Ok(config)
}

/// Refuses an effort model `id` does not take, as a usage error naming those
/// it does.
fn check_effort(id: &str, efforts: &[Effort], effort: Effort) -> Result<(), Error> {
    if efforts.contains(&effort) {
        return Ok(());
    }
    let names: Vec<&str> = efforts.iter().map(|effort| effort.as_str()).collect();
    Err(Error::Usage(format!(
        "{id} takes no {} effort; its efforts are {}",
        effort.as_str(),
        names.join(", ")
    )))
}

fn working_directory() -> Result<PathBuf, Error> {
    std::env::current_dir().map_err(|error| failed(format!("no working directory: {error}")))
}

/// Reads the rest of the arguments as words, or `None` once `help` is printed.
fn words(mut args: Parser, help: &str) -> Result<Option<Vec<String>>, Error> {
    let mut words = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('h') | Arg::Long("help") => {
                println!("{help}");
                return Ok(None);
            }
            Arg::Value(word) => words.push(word.string()?),
            arg => return Err(arg.unexpected().into()),
        }
    }
    Ok(Some(words))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;

    fn parse(args: &[&str]) -> Result<Option<Options>, Error> {
        Options::parse(Parser::from_args(args), "help")
    }

    #[test]
    fn options_choose_the_mode_settings_session_and_prompt() {
        let options = parse(&["-m", "gpt", "-c", "fix", "the", "build"]).expect("valid");
        let overrides = Overrides { model: Some("gpt".into()), ..Overrides::default() };
        let expected = Options {
            print: false,
            overrides,
            resume: Resume::Latest,
            prompt: Some("fix the build".into()),
        };
        assert_eq!(options, Some(expected));

        let args =
            ["-p", "--model=luna", "--provider", "codex", "-ehigh", "-r", "17-ab", "--", "-v"];
        let options = parse(&args).expect("valid").expect("options");
        assert!(options.print);
        let overrides = Overrides {
            provider: Some(Provider::Codex),
            model: Some("luna".into()),
            effort: Some(Effort::High),
        };
        assert_eq!(options.overrides, overrides);
        assert_eq!(options.resume, Resume::Id("17-ab".into()));
        assert_eq!(options.prompt.as_deref(), Some("-v"), "words after -- are the prompt");
        assert!(named("sessions").is_some() && named("fix").is_none());
    }

    #[test]
    fn command_lines_agt_cannot_take_say_what_is_wrong() {
        for (args, error) in [
            (&["--bogus"][..], "invalid option '--bogus'"),
            (&["-m"][..], "missing argument for option '-m'"),
            (
                &["--provider", "custom"][..],
                r#"--provider must be one of openai, codex, grok, openrouter, vercel, not "custom""#,
            ),
            (
                &["-e", "extreme"][..],
                r#"--effort must be one of none, minimal, low, medium, high, xhigh, max, not "extreme""#,
            ),
        ] {
            assert_eq!(parse(args), Err(Error::Usage(error.into())), "{args:?}");
        }
        assert_eq!(
            check_effort("grok-4.6", &[Effort::Low, Effort::High], Effort::Max),
            Err(Error::Usage("grok-4.6 takes no max effort; its efforts are low, high".into()))
        );
    }
}
