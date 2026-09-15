//! `agt mcp`: how the model, and the user, reach MCP servers from a shell.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use lexopt::{Arg, Parser, ValueExt};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{Error, failed, show_help, words};
use crate::bash::SESSION_DIR;
use crate::control::{self, Request};
use crate::mcp::{self, Pool, Server, Status, Tools, Transport};
use crate::{config, image};

pub(super) const HELP: &str = "\
Usage:
  agt mcp list                                 list the MCP servers
  agt mcp tools <server> [<tool>]              show a server's tools, or one tool in full
  agt mcp call <server> <tool> [<json> | -]    call a tool with a JSON object of arguments,
                                               or read the arguments from stdin with -
  agt mcp add <name> <url> [-H 'Name: value']...
  agt mcp add <name> [-e NAME=value]... [--] <command> [<arg>...]
  agt mcp remove <name>

Servers come from ~/.agt/mcp.json and the project's .mcp.json, in the
mcpServers format: {\"mcpServers\": {\"name\": {\"command\": \"npx\", \"args\": [...]}}}
or {\"type\": \"http\", \"url\": \"...\", \"headers\": {...}}. add and remove change
~/.agt/mcp.json. ${NAME} in a value reads that environment variable, so quote it
when adding a server. In an agt session these commands use the session's
servers, which keep running until it ends.";

/// Runs `agt mcp` with the arguments after `mcp`.
pub(super) fn run(mut args: Parser) -> Result<ExitCode, Error> {
    let name = match args.next()? {
        None | Some(Arg::Short('h') | Arg::Long("help")) => return show_help(HELP),
        Some(Arg::Value(name)) => name.string()?,
        Some(arg) => return Err(arg.unexpected().into()),
    };
    if name == "add" {
        let Some(server) = parse_server(&mut args)? else {
            return show_help(HELP);
        };
        return add(&server);
    }
    let Some(words) = words(args, HELP)? else {
        return Ok(ExitCode::SUCCESS);
    };
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    match (name.as_str(), &words[..]) {
        ("help", _) => show_help(HELP),
        ("list", []) => list(),
        ("tools", [server]) => tools(server, None),
        ("tools", [server, tool]) => tools(server, Some(tool)),
        ("call", [server, tool]) => call(server, tool, None),
        ("call", [server, tool, arguments]) => call(server, tool, Some(arguments)),
        ("remove", [name]) => remove(name),
        ("list", _) => Err(Error::Usage("list takes no arguments".into())),
        ("tools", _) => Err(Error::Usage("tools takes a server, and optionally one of its tools".into())),
        ("call", _) => Err(Error::Usage(
            "call takes a server, a tool and its arguments as one quoted JSON object, such as agt mcp call github get_me '{}'".into(),
        )),
        ("remove", _) => Err(Error::Usage("remove takes the name of a server".into())),
        (name, _) => Err(Error::Usage(format!("there is no command {name:?}"))),
    }
}

/// A server as `agt mcp add` takes it: a name, then the command that starts
/// it and its arguments, with `--env` for its environment, or its address,
/// with `--header` for its headers. Returns `None` for `--help`.
pub(crate) fn parse_server(args: &mut Parser) -> Result<Option<Server>, Error> {
    let (mut name, mut env, mut headers) = (None, Vec::new(), Vec::new());
    let target = loop {
        match args.next()? {
            None => {
                return Err(Error::Usage(match name {
                    None => "add takes a name, then the command that starts the server or its http:// or https:// address".into(),
                    Some(_) => "name the command that starts the server, or its http:// or https:// address".into(),
                }));
            }
            Some(Arg::Short('h') | Arg::Long("help")) => return Ok(None),
            Some(Arg::Short('e') | Arg::Long("env")) => {
                let assignment = args.value()?.string()?;
                let (variable, value) = assignment
                    .split_once('=')
                    .ok_or_else(|| Error::Usage(format!("{assignment:?} is not NAME=value")))?;
                env.push((variable.to_owned(), value.to_owned()));
            }
            Some(Arg::Short('H') | Arg::Long("header")) => headers.push(header(args)?),
            Some(Arg::Value(word)) if name.is_none() => name = Some(word.string()?),
            Some(Arg::Value(word)) => break word.string()?,
            Some(arg) => return Err(arg.unexpected().into()),
        }
    };
    let name = name.unwrap_or_default();
    if !mcp::valid_name(&name) {
        return Err(Error::Usage(format!(
            "{name:?} cannot name a server: use up to 64 letters, digits, `-`, `_` and `.`"
        )));
    }
    let transport = if target.starts_with("http://") || target.starts_with("https://") {
        while let Some(arg) = args.next()? {
            match arg {
                Arg::Short('H') | Arg::Long("header") => headers.push(header(args)?),
                arg => return Err(arg.unexpected().into()),
            }
        }
        if !env.is_empty() {
            return Err(Error::Usage(
                "--env sets the environment of a command; give an address --header".into(),
            ));
        }
        Transport::Http { url: target, headers }
    } else {
        if !headers.is_empty() {
            return Err(Error::Usage(
                "--header is sent to an address; give a command --env".into(),
            ));
        }
        // The rest are the command's own arguments, options included.
        let args = args.raw_args()?.map(|arg| arg.string()).collect::<Result<_, _>>()?;
        Transport::Stdio { command: target, args, env, cwd: None }
    };
    Ok(Some(Server { name, transport }))
}

/// The value of `--header`, as `Name: value`.
fn header(args: &mut Parser) -> Result<(String, String), Error> {
    let header = args.value()?.string()?;
    let (name, value) = header
        .split_once(':')
        .ok_or_else(|| Error::Usage(format!("{header:?} is not a header such as 'Name: value'")))?;
    Ok((name.trim().to_owned(), value.trim().to_owned()))
}

/// Where requests go: to the session's socket inside a session, or to
/// servers started for this command alone outside one.
enum Backend {
    Session(PathBuf),
    /// Servers started for this command, whose requests only ending it stops.
    Local(Pool),
}

impl Backend {
    fn new() -> Result<Self, String> {
        if let Some(socket) = std::env::var_os(control::SOCKET) {
            return Ok(Self::Session(socket.into()));
        }
        let home = config::home()?;
        let cwd =
            std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?;
        Ok(Self::Local(Pool::new(home, cwd, Vec::new(), None)))
    }

    fn status(&self) -> Result<Status, String> {
        match self {
            Self::Session(socket) => decode(control::ask(socket, &Request::Servers)?),
            Self::Local(pool) => Ok(pool.status()),
        }
    }

    fn tools(&self, server: &str) -> Result<Tools, String> {
        match self {
            Self::Session(socket) => {
                decode(control::ask(socket, &Request::Tools { server: server.into() })?)
            }
            Self::Local(pool) => pool.tools(server, &mcp::NEVER),
        }
    }

    fn call(&self, server: &str, tool: &str, arguments: Value) -> Result<Value, String> {
        match self {
            Self::Session(socket) => {
                let request = Request::Call { server: server.into(), tool: tool.into(), arguments };
                control::ask(socket, &request)
            }
            Self::Local(pool) => pool.call(server, tool, &arguments, &mcp::NEVER),
        }
    }
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value)
        .map_err(|error| format!("the session's answer is unreadable: {error}"))
}

fn list() -> Result<ExitCode, Error> {
    let status = Backend::new()?.status()?;
    for problem in &status.problems {
        eprintln!("agt mcp: {problem}");
    }
    if status.servers.is_empty() {
        println!("no MCP servers are set up; add one with `agt mcp add`");
    }
    let width = status.servers.iter().map(|server| server.name.len()).max().unwrap_or(0);
    for server in &status.servers {
        let running = if server.running { "  running" } else { "" };
        println!("{:width$}  {:5}  {}{running}", server.name, server.transport, server.target);
    }
    Ok(ExitCode::SUCCESS)
}

fn tools(server: &str, tool: Option<&str>) -> Result<ExitCode, Error> {
    let listed = Backend::new()?.tools(server)?;
    match tool {
        None => print!("{}", listed.describe(server)),
        Some(tool) => println!("{}", listed.describe_tool(server, tool)?),
    }
    Ok(ExitCode::SUCCESS)
}

fn call(server: &str, tool: &str, arguments: Option<&str>) -> Result<ExitCode, Error> {
    let text = match arguments {
        None => "{}".to_owned(),
        Some("-") => {
            let mut text = String::new();
            io::stdin()
                .read_to_string(&mut text)
                .map_err(|error| failed(format!("cannot read the arguments: {error}")))?;
            text
        }
        Some(text) => text.to_owned(),
    };
    let example = r#"such as '{"url": "https://example.com"}'"#;
    let arguments: Value = serde_json::from_str(&text).map_err(|error| {
        failed(format!("the arguments are not JSON ({error}); pass an object {example}"))
    })?;
    if !arguments.is_object() {
        return Err(failed(format!("the arguments must be a JSON object, {example}")));
    }
    let result = Backend::new()?.call(server, tool, arguments)?;
    let failed_call =
        show(&result).map_err(|error| failed(format!("cannot print the result: {error}")))?;
    Ok(if failed_call { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

/// Prints a tool's result, showing its images to the session as `agt view`
/// does. Returns whether the tool reports that it failed.
fn show(result: &Value) -> io::Result<bool> {
    let mut out = io::stdout().lock();
    let mut images = Images::default();
    let content = result["content"].as_array().map(Vec::as_slice).unwrap_or_default();
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    for item in content {
        match item["type"].as_str().unwrap_or_default() {
            "text" => writeln!(out, "{}", text(&item["text"]).trim_end_matches('\n'))?,
            "image" => images.show(&item["data"], &text(&item["mimeType"]), &mut out)?,
            "audio" => {
                writeln!(
                    out,
                    "[audio {} omitted: agt cannot hear audio]",
                    text(&item["mimeType"])
                )?;
            }
            "resource_link" => {
                writeln!(out, "[resource {}: {}]", text(&item["name"]), text(&item["uri"]))?;
                if let Some(description) = item["description"].as_str() {
                    writeln!(out, "{description}")?;
                }
            }
            "resource" => {
                let resource = &item["resource"];
                let (uri, mime) = (text(&resource["uri"]), text(&resource["mimeType"]));
                match resource["text"].as_str() {
                    Some(body) => writeln!(out, "<resource uri=\"{uri}\">\n{body}\n</resource>")?,
                    None if mime.starts_with("image/") => {
                        images.show(&resource["blob"], &mime, &mut out)?;
                    }
                    None => writeln!(out, "[binary resource {uri} ({mime}) omitted]")?,
                }
            }
            _ => writeln!(out, "{item}")?,
        }
    }
    // Structured content repeats the text a tool also returns, when it does.
    if content.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        let pretty =
            serde_json::to_string_pretty(structured).expect("JSON values always serialize");
        writeln!(out, "{pretty}")?;
    }
    out.flush()?;
    Ok(result["isError"].as_bool().unwrap_or(false))
}

/// Shows images to the session reading the command's terminal.
#[derive(Default)]
struct Images {
    terminal: Option<image::Terminal>,
}

impl Images {
    /// Shows the image in base64 `data`, or says in `out` why it cannot.
    fn show(&mut self, data: &Value, mime: &str, out: &mut impl Write) -> io::Result<()> {
        // Output printed so far goes before the image.
        out.flush()?;
        if let Err(problem) = self.try_show(data) {
            writeln!(out, "[image {mime}: {problem}]")?;
        }
        Ok(())
    }

    fn try_show(&mut self, data: &Value) -> Result<(), String> {
        let dir = std::env::var_os(SESSION_DIR).ok_or("shown only in an agt session")?;
        let bytes = STANDARD
            .decode(data.as_str().unwrap_or_default())
            .map_err(|_| "its data is not base64")?;
        let shown = image::save_image(&bytes, Path::new(&dir))?;
        let terminal = match &mut self.terminal {
            Some(terminal) => terminal,
            terminal => terminal.insert(image::Terminal::open()?),
        };
        terminal.show(&shown).map_err(|error| format!("cannot show it: {error}"))
    }
}

fn add(server: &Server) -> Result<ExitCode, Error> {
    let home = config::home()?;
    mcp::add(&home, server).map_err(|error| {
        failed(format!("cannot save {}: {error}", home.join(mcp::GLOBAL).display()))
    })?;
    println!("added {}: {}", server.name, server.transport);
    Ok(ExitCode::SUCCESS)
}

fn remove(name: &str) -> Result<ExitCode, Error> {
    let home = config::home()?;
    let path = home.join(mcp::GLOBAL);
    match mcp::remove(&home, name) {
        Ok(true) => {
            println!("removed {name}");
            Ok(ExitCode::SUCCESS)
        }
        Ok(false) => Err(failed(format!("{} has no server {name:?}", path.display()))),
        Err(error) => Err(failed(format!("cannot save {}: {error}", path.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str]) -> Result<Option<Server>, Error> {
        parse_server(&mut Parser::from_args(words))
    }

    #[test]
    fn servers_are_added_by_address_or_command() {
        assert_eq!(
            parse(&[
                "github",
                "https://api.example.com/mcp",
                "--header",
                "Authorization: Bearer ${TOKEN}"
            ]),
            Ok(Some(Server {
                name: "github".into(),
                transport: Transport::Http {
                    url: "https://api.example.com/mcp".into(),
                    headers: vec![("Authorization".into(), "Bearer ${TOKEN}".into())],
                },
            }))
        );
        let playwright = Server {
            name: "playwright".into(),
            transport: Transport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@playwright/mcp".into(), "--headless".into()],
                env: vec![("DEBUG".into(), "1".into())],
                cwd: None,
            },
        };
        let words =
            ["playwright", "--env", "DEBUG=1", "--", "npx", "-y", "@playwright/mcp", "--headless"];
        assert_eq!(parse(&words), Ok(Some(playwright.clone())));
        let words = ["playwright", "-e", "DEBUG=1", "npx", "-y", "@playwright/mcp", "--headless"];
        assert_eq!(parse(&words), Ok(Some(playwright)), "a command's options need no --");
        for (words, problem) in [
            (
                &["x", "https://x.dev", "--header", "no colon"][..],
                "\"no colon\" is not a header such as 'Name: value'",
            ),
            (&["x", "https://x.dev", "--verbose"][..], "invalid option '--verbose'"),
            (&["x", "--env", "NOVALUE", "cmd"][..], "\"NOVALUE\" is not NAME=value"),
            (
                &["x", "--env", "A=1"][..],
                "name the command that starts the server, or its http:// or https:// address",
            ),
            (
                &["bad name", "cmd"][..],
                "\"bad name\" cannot name a server: use up to 64 letters, digits, `-`, `_` and `.`",
            ),
            (
                &["x", "-H", "A: b", "cmd"][..],
                "--header is sent to an address; give a command --env",
            ),
        ] {
            assert_eq!(parse(words), Err(Error::Usage(problem.into())), "{words:?}");
        }
    }
}
