//! The bash tool as the model sees it: its definition, the calls the model
//! makes, how their arguments are read, and the header each result starts
//! with.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::Exit;
use crate::item::Item;

const DEFAULT_WAIT: Duration = Duration::from_secs(30);
const MAX_WAIT: Duration = Duration::from_secs(3600);
/// Ends the output of a call the user cancelled.
pub(crate) const CANCELLED: &str = "[cancelled by the user]";

/// The arguments the tool accepts.
const ARGUMENTS: [&str; 5] = ["command", "id", "input", "wait", "kill"];
const EXAMPLE: &str = r#"{"command": "ls"}"#;

const DESCRIPTION: &str = "\
Run commands in a real terminal. Each command starts a process with its own id and pseudo-terminal, in the working directory. Call the tool in one of five forms:

- {\"command\": \"cargo test\", \"wait\": 300}: start a command. It returns when the command exits or after wait seconds (default 30, at most 3600). A command still running then keeps running in the background, and you are told when it exits, with its new output.
- {\"id\": 3}: return process 3's new output, waiting up to wait seconds for some.
- {\"id\": 3, \"input\": \"yes\"}: type into process 3, such as a prompt, REPL, ssh session or full-screen program, and return its output once it settles. Text is followed by Enter; input containing control characters, such as \\u0003 (Ctrl-C) or \\u001b[A (Up), is sent exactly.
- {\"id\": 3, \"kill\": true}: stop process 3 and its children.
- {\"wait\": 600}: wait until a background process exits or a message arrives, up to wait seconds, and return what happened.

Each command runs in a fresh shell, so cd and exported variables do not carry over; start bash and type into it when you need a shell that keeps its state. Do not background commands with &, nohup or setsid: a command that outlives its wait is already in the background.

A result starts with a header such as [id 3 · exit 0 · 1.2s] or [id 3 · running · 30.0s · you will be told when it exits], then the output that is new since you last saw it, as a terminal shows it. Output over 12 KB keeps its first 4 KB and last 8 KB. Every process's complete output is a plain text file, $AGT_SESSION_DIR/procs/<id>.log, which you can grep and read in ranges. A full-screen program returns its current screen.";

/// How the description tells a model that sees images to look at them.
const IMAGES: &str = "\
To see an image yourself, such as a screenshot of an interface you are building, a rendered page, a chart or a photo, run agt view <file>... in a command, on its own or after the commands that make the image. Each image is attached to the result after a header that names it, such as [image /work/shot.png · 2880x1800 · shown at 1980x1238, scale 1.45]. PNG, JPEG, WebP and GIF files are supported; convert others first, such as PDF pages with pdftoppm. An image larger than 2000 px or about 2.5 megapixels is scaled down; to read fine detail, view part of it at full resolution with agt view --region left,top,right,bottom <file>, in pixels of the original. A point in a scaled view maps to the original as the region's left and top plus the point times the scale.";

/// The tool definition requests carry, which tells a model that sees images
/// how to look at them.
pub(crate) fn definition(images: bool) -> Value {
    let description =
        if images { format!("{DESCRIPTION}\n\n{IMAGES}") } else { DESCRIPTION.to_owned() };
    json!({
        "type": "function",
        "name": "bash",
        "description": description,
        // Otherwise OpenAI's models treat every argument as required and fill
        // unused ones, such as a placeholder command in a call that reads a
        // process, which makes the call ambiguous.
        "strict": false,
        "parameters": {
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "A shell command to start as a new process in the working directory."
                },
                "id": {
                    "type": "integer",
                    "description": "The id of a process, from a result header, to read, type into or stop."
                },
                "input": {
                    "type": "string",
                    "description": "Text to type into process id, followed by Enter. Text with control characters is sent exactly."
                },
                "wait": {
                    "type": "number",
                    "description": "Seconds to wait at most (default 30, at most 3600). Given alone, waits for a background process to exit or a message to arrive."
                },
                "kill": {
                    "type": "boolean",
                    "description": "true stops process id and its children."
                }
            }
        }
    })
}

/// A function call from the model, with its arguments read.
#[derive(Debug)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    /// The arguments as the model wrote them.
    pub(crate) arguments: String,
    pub(crate) tool: Tool,
}

/// What a call asks for.
#[derive(Debug)]
pub(crate) enum Tool {
    Bash(Action),
    /// A tool agt does not have, by the name the model gave.
    Unknown(String),
    /// Arguments bash rejects, with a reason that names the call to make
    /// instead.
    Invalid(String),
}

impl ToolCall {
    pub(crate) fn new(id: String, name: &str, arguments: String) -> Self {
        let tool = match name {
            "bash" => Action::parse(&arguments).map_or_else(Tool::Invalid, Tool::Bash),
            _ => Tool::Unknown(name.to_owned()),
        };
        Self { id, arguments, tool }
    }

    /// The call a `function_call` item makes.
    pub(crate) fn from_item(item: &Item) -> Self {
        let field = |key: &str| item.str(key).unwrap_or_default();
        Self::new(field("call_id").to_owned(), field("name"), field("arguments").to_owned())
    }

    /// A one-line label for the call.
    pub(crate) fn title(&self) -> String {
        match &self.tool {
            Tool::Bash(Action::Start { command, .. }) => command.clone(),
            Tool::Bash(Action::Input { id, input, .. }) => {
                format!("input to {id}: {}", input.escape_debug())
            }
            Tool::Bash(Action::Read { id, .. }) => format!("read {id}"),
            Tool::Bash(Action::Kill { id }) => format!("kill {id}"),
            Tool::Bash(Action::Wait { wait }) => format!("wait up to {}", elapsed(*wait)),
            Tool::Unknown(_) | Tool::Invalid(_) => self.arguments.clone(),
        }
    }

    /// The call on one line, as transcripts show it: the title, where a
    /// command shows only its first line, without the `cd` into the working
    /// directory `cwd` that models often put before every command.
    pub(crate) fn headline(&self, cwd: &Path) -> String {
        let Tool::Bash(Action::Start { command, .. }) = &self.tool else {
            return self.title();
        };
        let command = command.trim();
        let command = command
            .strip_prefix("cd ")
            .and_then(|rest| rest.split_once("&&").or_else(|| rest.split_once(';')))
            .filter(|(dir, rest)| {
                Path::new(dir.trim().trim_matches(['"', '\''])) == cwd && !rest.trim().is_empty()
            })
            .map_or(command, |(_, rest)| rest.trim_start());
        match command.split_once('\n') {
            Some((first, _)) => format!("{} …", first.trim_end()),
            None => command.to_owned(),
        }
    }
}

/// A bash call, validated.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Action {
    /// Start a command, waiting up to `wait` for it to exit.
    Start { command: String, wait: Duration },
    /// Type into a process, waiting up to `wait` for its output to settle.
    Input { id: u32, input: String, wait: Duration },
    /// Read a process's new output, waiting up to `wait` for some.
    Read { id: u32, wait: Duration },
    /// Stop a process and its children.
    Kill { id: u32 },
    /// Wait up to `wait` for a background process to exit or a message to
    /// arrive.
    Wait { wait: Duration },
}

impl Action {
    /// Reads a call's arguments. A zero id counts as absent, since models fill
    /// unused fields, and conflicting forms are errors that name the call to
    /// make instead.
    pub(crate) fn parse(arguments: &str) -> Result<Self, String> {
        let args = Args::parse(arguments)?;
        let given_wait = match args.number("wait")? {
            None => None,
            Some(seconds) if seconds.is_finite() && seconds >= 0.0 => {
                Some(Duration::from_secs_f64(seconds.min(MAX_WAIT.as_secs_f64())))
            }
            Some(seconds) => {
                return Err(format!(
                    "wait must be a non-negative number of seconds, not {seconds}"
                ));
            }
        };
        let wait = given_wait.unwrap_or(DEFAULT_WAIT);
        let id = match args.number("id")? {
            None | Some(0.0) => None,
            Some(id) if id.fract() == 0.0 && (1.0..=f64::from(u32::MAX)).contains(&id) => {
                Some(id as u32)
            }
            Some(id) => {
                return Err(format!("id must be a process id from a result header, not {id}"));
            }
        };
        let command =
            args.string("command")?.filter(|command| !command.trim().is_empty()).map(str::to_owned);
        let input = args.string("input")?.filter(|input| !input.is_empty()).map(str::to_owned);
        let kill = match args.get("kill") {
            Value::Null => false,
            Value::Bool(kill) => *kill,
            Value::String(text) if text == "true" || text == "false" => text == "true",
            value => return Err(format!("kill must be true or false, not {value}")),
        };
        match (command, id, input, kill) {
            (Some(command), None, None, false) => Ok(Self::Start { command, wait }),
            (None, Some(id), None, false) => Ok(Self::Read { id, wait }),
            (None, Some(id), Some(input), false) => Ok(Self::Input { id, input, wait }),
            (None, Some(id), None, true) => Ok(Self::Kill { id }),
            (None, None, None, false) => match given_wait {
                Some(wait) => Ok(Self::Wait { wait }),
                None => Err(format!(
                    "nothing to run; pass {EXAMPLE} to start a command, {{\"id\": 3}} to read process 3, or {{\"wait\": 600}} to wait for a background process"
                )),
            },
            (Some(_), Some(id), ..) => Err(format!(
                "command starts a new process and cannot be combined with id; to type into process {id}, pass {{\"id\": {id}, \"input\": \"...\"}}"
            )),
            (Some(_), None, ..) => Err(
                "command cannot be combined with input or kill, which act on a running process given by id".into(),
            ),
            (None, None, ..) => Err("input and kill need the id of a running process".into()),
            (None, Some(_), Some(_), true) => Err("pass input or kill, not both".into()),
        }
    }
}

/// How a call ended.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Outcome {
    Ok,
    Failed,
    Cancelled,
}

impl Outcome {
    /// How the call that returned `output` ended, as its output says.
    pub(crate) fn of(output: &str) -> Self {
        if output.ends_with(CANCELLED) {
            Self::Cancelled
        } else if Header::parse(output).is_some_and(|(header, _)| header.succeeded()) {
            Self::Ok
        } else {
            Self::Failed
        }
    }
}

/// The line a result starts with, which says what the result is of. Results
/// are written and read back through this type alone.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Header {
    /// A process: `[id 3 · exit 0 · 1.2s]`, or while it runs
    /// `[id 3 · running · 30.0s · you will be told when it exits]`.
    Process {
        id: u32,
        /// How it ended, or `None` while it runs.
        exit: Option<Exit>,
        /// How long it has run.
        ran: Duration,
        /// A process that already runs the same command, which models
        /// sometimes lose track of and start again.
        duplicate: Option<u32>,
        /// Whether its exit will be reported, for a command left running.
        reported: bool,
    },
    /// A wait for anything that happened in the background: `[waited 12.3s]`.
    Waited(Duration),
}

/// The notes a process header can end with.
const DUPLICATE: (&str, &str) = ("process ", " already runs this command");
const REPORTED: &str = "you will be told when it exits";

impl Header {
    /// The header `result` starts with and the output after it, or `None`
    /// for a result without one, such as an error agt reports itself.
    pub(crate) fn parse(result: &str) -> Option<(Self, &str)> {
        let (header, output) = result.strip_prefix('[')?.split_once(']')?;
        if let Some(waited) = header.strip_prefix("waited ") {
            return Some((Self::Waited(parse_elapsed(waited)?), output));
        }
        let mut fields = header.strip_prefix("id ")?.split(" · ");
        let id = fields.next()?.parse().ok()?;
        let exit = match fields.next()? {
            "running" => None,
            status => Some(Exit::parse(status)?),
        };
        let ran = parse_elapsed(fields.next()?)?;
        let (mut duplicate, mut reported) = (None, false);
        for note in fields {
            if note == REPORTED {
                reported = true;
            } else if let Some(other) =
                note.strip_prefix(DUPLICATE.0).and_then(|rest| rest.strip_suffix(DUPLICATE.1))
            {
                duplicate = other.parse().ok();
            }
        }
        Some((Self::Process { id, exit, ran, duplicate, reported }, output))
    }

    /// Whether the call went as it should: its process exited cleanly or is
    /// still running, or it waited.
    pub(crate) fn succeeded(&self) -> bool {
        match self {
            Self::Process { exit, .. } => exit.is_none_or(Exit::success),
            Self::Waited(_) => true,
        }
    }
}

impl fmt::Display for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Process { id, exit, ran, duplicate, reported } => {
                write!(f, "[id {id} · ")?;
                match exit {
                    Some(exit) => write!(f, "{exit}")?,
                    None => f.write_str("running")?,
                }
                write!(f, " · {}", elapsed(*ran))?;
                if let Some(other) = duplicate {
                    write!(f, " · {}{other}{}", DUPLICATE.0, DUPLICATE.1)?;
                }
                if *reported {
                    write!(f, " · {REPORTED}")?;
                }
                f.write_str("]")
            }
            Self::Waited(waited) => write!(f, "[waited {}]", elapsed(*waited)),
        }
    }
}

/// Formats a duration compactly: `4.2s`, `3m07s`, `2h05m`.
pub(crate) fn elapsed(duration: Duration) -> String {
    let secs = duration.as_secs();
    match secs {
        0..60 => format!("{:.1}s", duration.as_secs_f64()),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs / 60 % 60),
    }
}

/// Reads a duration `elapsed` wrote.
fn parse_elapsed(text: &str) -> Option<Duration> {
    let number = |text: &str| text.parse::<u64>().ok();
    if let Some((hours, minutes)) = text.strip_suffix('m').and_then(|rest| rest.split_once('h')) {
        return Some(Duration::from_secs(number(hours)? * 3600 + number(minutes)? * 60));
    }
    let seconds = text.strip_suffix('s')?;
    if let Some((minutes, seconds)) = seconds.split_once('m') {
        return Some(Duration::from_secs(number(minutes)? * 60 + number(seconds)?));
    }
    let seconds =
        seconds.parse::<f64>().ok().filter(|seconds| seconds.is_finite() && *seconds >= 0.0)?;
    Some(Duration::from_secs_f64(seconds))
}

/// The arguments of a call, read leniently since models quote numbers and
/// fill unused fields with empty strings. Errors say which call to make
/// instead: a vague error leads models to retry the same call.
struct Args(Map<String, Value>);

impl Args {
    /// Reads `arguments`, a JSON object of the tool's arguments only.
    fn parse(arguments: &str) -> Result<Self, String> {
        match serde_json::from_str::<Value>(arguments) {
            Ok(Value::Object(args)) => {
                match args.keys().find(|key| !ARGUMENTS.contains(&key.as_str())) {
                    Some(unknown) => Err(format!(
                        "unknown argument {unknown:?}; the arguments are {}",
                        ARGUMENTS.join(", ")
                    )),
                    None => Ok(Self(args)),
                }
            }
            Ok(_) => Err(format!("arguments must be a JSON object, such as {EXAMPLE}")),
            Err(error) => Err(format!(
                "arguments are not valid JSON ({error}); pass an object such as {EXAMPLE}"
            )),
        }
    }

    /// The argument named `key`, or null when it is absent.
    fn get(&self, key: &str) -> &Value {
        self.0.get(key).unwrap_or(&Value::Null)
    }

    /// A number, which may be written as a string; an empty one is absent.
    fn number(&self, key: &str) -> Result<Option<f64>, String> {
        match self.get(key) {
            Value::Null => Ok(None),
            Value::String(text) if text.trim().is_empty() => Ok(None),
            Value::String(text) => text
                .trim()
                .parse::<f64>()
                .map(Some)
                .map_err(|_| format!("{key} must be a number, not {text:?}")),
            value => value
                .as_f64()
                .map(Some)
                .ok_or_else(|| format!("{key} must be a number, not {value}")),
        }
    }

    fn string(&self, key: &str) -> Result<Option<&str>, String> {
        match self.get(key) {
            Value::Null => Ok(None),
            Value::String(text) => Ok(Some(text)),
            value => Err(format!("{key} must be a string, not {value}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_are_read_leniently() {
        let wait = DEFAULT_WAIT;
        for (arguments, action) in [
            (r#"{"command":"ls","id":0,"input":""}"#, Action::Start { command: "ls".into(), wait }),
            (
                r#"{"id":"3","input":"y\n","wait":"2.5"}"#,
                Action::Input { id: 3, input: "y\n".into(), wait: Duration::from_millis(2500) },
            ),
            (r#"{"id":3}"#, Action::Read { id: 3, wait }),
            (r#"{"id":3,"kill":"true"}"#, Action::Kill { id: 3 }),
            (
                r#"{"command":"sleep 1","wait":99999}"#,
                Action::Start { command: "sleep 1".into(), wait: MAX_WAIT },
            ),
            (r#"{"wait":600}"#, Action::Wait { wait: Duration::from_secs(600) }),
            (r#"{"wait":0,"command":""}"#, Action::Wait { wait: Duration::ZERO }),
        ] {
            assert_eq!(Action::parse(arguments), Ok(action), "{arguments}");
        }
    }

    #[test]
    fn invalid_actions_name_the_call_to_make() {
        let nothing = r#"nothing to run; pass {"command": "ls"} to start a command, {"id": 3} to read process 3, or {"wait": 600} to wait for a background process"#;
        for (arguments, error) in [
            (
                r#"{"command":"ls","id":2}"#,
                r#"command starts a new process and cannot be combined with id; to type into process 2, pass {"id": 2, "input": "..."}"#,
            ),
            (
                r#"{"command":"ls","kill":true}"#,
                "command cannot be combined with input or kill, which act on a running process given by id",
            ),
            ("{}", nothing),
            (r#"{"kill":true}"#, "input and kill need the id of a running process"),
            (r#"{"id":1,"input":"y","kill":true}"#, "pass input or kill, not both"),
            (r#"{"id":1.5}"#, "id must be a process id from a result header, not 1.5"),
            (
                r#"{"id":4294967296}"#,
                "id must be a process id from a result header, not 4294967296",
            ),
            (
                r#"{"command":"ls","wait":-1}"#,
                "wait must be a non-negative number of seconds, not -1",
            ),
            (
                r#"{"command":"ls","wait":"NaN"}"#,
                "wait must be a non-negative number of seconds, not NaN",
            ),
            (r#"{"id":1,"kill":"yes"}"#, r#"kill must be true or false, not "yes""#),
            (
                r#"{"size":1}"#,
                r#"unknown argument "size"; the arguments are command, id, input, wait, kill"#,
            ),
            ("[1]", r#"arguments must be a JSON object, such as {"command": "ls"}"#),
            (r#"{"id":"many"}"#, r#"id must be a number, not "many""#),
            (r#"{"command":3}"#, "command must be a string, not 3"),
        ] {
            assert_eq!(Action::parse(arguments), Err(error.to_owned()), "{arguments}");
        }
        let unreadable = Action::parse("{").expect_err("not JSON");
        assert!(unreadable.starts_with("arguments are not valid JSON ("), "{unreadable}");
    }

    #[test]
    fn calls_are_titled_and_headlined() {
        let call = |name: &str, arguments: &str| ToolCall::new("c".into(), name, arguments.into());
        assert_eq!(call("bash", r#"{"id":2,"input":"y\n"}"#).title(), "input to 2: y\\n");
        assert_eq!(call("bash", r#"{"command":"cargo test"}"#).title(), "cargo test");
        assert_eq!(call("bash", r#"{"id":4,"kill":true}"#).title(), "kill 4");
        assert_eq!(call("bash", r#"{"wait":90}"#).title(), "wait up to 1m30s");
        assert_eq!(call("bash", "{oops").title(), "{oops");
        assert!(matches!(call("view", "{}").tool, Tool::Unknown(name) if name == "view"));
        assert!(matches!(call("bash", "{}").tool, Tool::Invalid(_)));

        let headline = |command: &str| {
            let arguments = json!({ "command": command }).to_string();
            call("bash", &arguments).headline(Path::new("/work"))
        };
        assert_eq!(headline("cd \"/work/\"; ls"), "ls");
        assert_eq!(headline("cd /else && ls"), "cd /else && ls");
        assert_eq!(headline("cat > a <<'EOF'\nbody\nEOF"), "cat > a <<'EOF' …");
    }

    #[test]
    fn headers_are_written_and_read_back() {
        let ran = Duration::from_millis(1200);
        let exited = Header::Process {
            id: 3,
            exit: Some(Exit::Code(0)),
            ran,
            duplicate: None,
            reported: false,
        };
        let running = Header::Process {
            id: 4,
            exit: None,
            ran: Duration::from_secs(187),
            duplicate: Some(1),
            reported: true,
        };
        let signalled = Header::Process {
            id: 5,
            exit: Some(Exit::Signal(9)),
            ran: Duration::from_secs(7500),
            duplicate: None,
            reported: false,
        };
        let waited = Header::Waited(Duration::from_millis(12_300));
        for (header, text) in [
            (&exited, "[id 3 · exit 0 · 1.2s]"),
            (
                &running,
                "[id 4 · running · 3m07s · process 1 already runs this command · you will be told when it exits]",
            ),
            (&signalled, "[id 5 · signal 9 · 2h05m]"),
            (&waited, "[waited 12.3s]"),
        ] {
            assert_eq!(header.to_string(), text);
            assert_eq!(
                Header::parse(&format!("{text}\noutput")),
                Some((header.clone(), "\noutput"))
            );
        }
        assert!(exited.succeeded() && running.succeeded() && waited.succeeded());
        assert!(!signalled.succeeded());
        for result in [
            "error: unknown argument",
            "[image a.png · 1x1]",
            "[id 1 · exit 0]",
            "[id x · exit 0 · 1.0s]",
            "",
        ] {
            assert_eq!(Header::parse(result), None, "{result}");
        }
    }

    #[test]
    fn outputs_tell_how_calls_ended() {
        assert_eq!(Outcome::of("[id 1 · exit 0 · 0.1s]\nok"), Outcome::Ok);
        assert_eq!(Outcome::of("[id 1 · running · 30.0s]"), Outcome::Ok);
        assert_eq!(Outcome::of("[waited 3.0s]\nprocess 2 exited"), Outcome::Ok);
        assert_eq!(Outcome::of("[id 1 · exit 2 · 0.1s]"), Outcome::Failed);
        assert_eq!(Outcome::of("error: unknown argument"), Outcome::Failed);
        assert_eq!(
            Outcome::of("[id 1 · running · 1.0s]\n[cancelled by the user]"),
            Outcome::Cancelled
        );
    }

    #[test]
    fn durations_format_compactly() {
        assert_eq!(elapsed(Duration::from_millis(4200)), "4.2s");
        assert_eq!(elapsed(Duration::from_secs(187)), "3m07s");
        assert_eq!(elapsed(Duration::from_secs(7500)), "2h05m");
    }
}
