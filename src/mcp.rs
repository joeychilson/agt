//! MCP servers, whose tools the model calls through bash with `agt mcp`.
//!
//! Servers are listed in `~/.agt/mcp.json` and a project's `.mcp.json`, in the
//! `mcpServers` shape other agents read, and an ACP client can add more for a
//! session. None starts with the session. The model reads a server's tools
//! with `agt mcp tools` and calls one with `agt mcp call`. In a session those
//! commands reach its control socket, whose pool starts each server the first
//! time it is used and keeps it running, with whatever state it holds, until
//! the session ends. The system prompt names each server and the tools it had
//! when last used, from what agt remembered then, so no session waits on a
//! server.

mod catalog;
mod connection;
mod http;
mod pool;
mod schema;
mod stdio;

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::{Map, Value, json};

pub(crate) use catalog::{Listing, listings};
pub(crate) use pool::{Pool, Status, Tools};

use crate::config;

/// The file of servers in agt's home, and the one a project keeps.
pub(crate) const GLOBAL: &str = "mcp.json";
const PROJECT: &str = ".mcp.json";

/// The cancellation of a request that nothing cancels.
pub(crate) static NEVER: AtomicBool = AtomicBool::new(false);

/// A server a session can use.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Server {
    pub(crate) name: String,
    pub(crate) transport: Transport,
}

/// How agt reaches a server.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Transport {
    /// A program agt starts and speaks to over its standard streams.
    Stdio { command: String, args: Vec<String>, env: Vec<(String, String)>, cwd: Option<PathBuf> },
    /// An endpoint agt POSTs each message to.
    Http { url: String, headers: Vec<(String, String)> },
}

impl fmt::Display for Transport {
    /// The program or address, without the environment or headers, which
    /// may hold secrets.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio { command, args, .. } => {
                f.write_str(command)?;
                args.iter().try_for_each(|arg| write!(f, " {arg}"))
            }
            Self::Http { url, .. } => f.write_str(url),
        }
    }
}

/// Whether `name` can name a server: it appears in commands and the prompt.
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The servers a session in `cwd` uses, and the problems with their
/// settings: agt's own file, then the project's `.mcp.json`, then the ones an
/// editor gave, each replacing a server of the same name.
pub(crate) fn servers(home: &Path, cwd: &Path, given: &[Server]) -> (Vec<Server>, Vec<String>) {
    let mut servers = BTreeMap::new();
    let mut problems = Vec::new();
    let env = |name: &str| std::env::var(name).ok();
    for path in std::iter::once(home.join(GLOBAL)).chain(project_file(cwd)) {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                problems.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        let base = path.parent().unwrap_or(cwd);
        match parse(&text, base, &env) {
            Ok(found) => {
                for server in found {
                    match server {
                        Ok(server) => {
                            servers.insert(server.name.clone(), server);
                        }
                        Err(problem) => problems.push(format!("{}: {problem}", path.display())),
                    }
                }
            }
            Err(problem) => problems.push(format!("{}: {problem}", path.display())),
        }
    }
    for server in given {
        servers.insert(server.name.clone(), server.clone());
    }
    (servers.into_values().collect(), problems)
}

/// The `.mcp.json` of the project `cwd` is in: the nearest one from `cwd` up
/// to the repository's root, or in `cwd` itself outside a repository.
fn project_file(cwd: &Path) -> Option<PathBuf> {
    let root = cwd.ancestors().find(|dir| dir.join(".git").exists()).unwrap_or(cwd);
    for dir in cwd.ancestors() {
        let file = dir.join(PROJECT);
        if file.is_file() {
            return Some(file);
        }
        if dir == root {
            break;
        }
    }
    None
}

/// The servers a file of settings lists, each or the problem with it.
/// Relative working directories are taken from `base`, and `${NAME}` from
/// `env`.
fn parse(
    text: &str,
    base: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<Result<Server, String>>, String> {
    let root: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let Some(root) = root.as_object() else {
        return Err("expected a JSON object".into());
    };
    let Some(entries) = root.get("mcpServers") else {
        return Ok(Vec::new());
    };
    let entries = entries.as_object().ok_or("mcpServers must be an object of servers")?;
    Ok(entries
        .iter()
        .map(|(name, spec)| {
            if !valid_name(name) {
                return Err(format!(
                    "server {name:?}: a name is up to 64 letters, digits, `-`, `_` and `.`"
                ));
            }
            let transport = transport(spec, base, env)
                .map_err(|problem| format!("server {name}: {problem}"))?;
            Ok(Server { name: name.clone(), transport })
        })
        .collect())
}

fn transport(
    spec: &Value,
    base: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Transport, String> {
    let text = |key: &str| match spec.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => expand(text, env).map(Some),
        Some(_) => Err(format!("{key} must be a string")),
    };
    let strings = |key: &str| match spec.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().ok_or(format!("{key} must be strings")))
            .map(|item| item.and_then(|item| expand(item, env)))
            .collect(),
        Some(_) => Err(format!("{key} must be an array of strings")),
    };
    let pairs = |key: &str| match spec.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(name, value)| {
                let value = value.as_str().ok_or(format!("{key} values must be strings"))?;
                Ok((name.clone(), expand(value, env)?))
            })
            .collect(),
        Some(_) => Err(format!("{key} must be an object of strings")),
    };
    match (spec.get("type").and_then(Value::as_str), spec.get("url")) {
        (Some("sse"), _) => {
            Err("the deprecated SSE transport is not supported; use the server's streamable HTTP address".into())
        }
        (Some("http" | "streamable-http"), _) | (None, Some(_)) => Ok(Transport::Http {
            url: text("url")?.ok_or("url is missing")?,
            headers: pairs("headers")?,
        }),
        (Some("stdio") | None, _) => Ok(Transport::Stdio {
            command: text("command")?.filter(|command| !command.is_empty()).ok_or("command is missing")?,
            args: strings("args")?,
            env: pairs("env")?,
            cwd: text("cwd")?.map(|dir| base.join(dir)),
        }),
        (Some(other), _) => Err(format!("unknown type {other:?}; the types are stdio and http")),
    }
}

/// `text` with each `${NAME}` or `${NAME:-default}` replaced from `env`, so
/// files of settings need not hold secrets.
fn expand(text: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| format!("{text:?} has an unclosed ${{"))?;
        let (name, default) = match after[..end].split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (&after[..end], None),
        };
        let value = env(name).or_else(|| default.map(str::to_owned));
        out.push_str(&value.ok_or_else(|| format!("${{{name}}} is not set"))?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Saves `server` in agt's own file, replacing one of the same name.
pub(crate) fn add(home: &Path, server: &Server) -> io::Result<()> {
    let spec = match &server.transport {
        Transport::Stdio { command, args, env, .. } => {
            let mut spec = json!({ "command": command });
            if !args.is_empty() {
                spec["args"] = json!(args);
            }
            if !env.is_empty() {
                spec["env"] = Value::Object(object(env));
            }
            spec
        }
        Transport::Http { url, headers } => {
            let mut spec = json!({ "type": "http", "url": url });
            if !headers.is_empty() {
                spec["headers"] = Value::Object(object(headers));
            }
            spec
        }
    };
    // Headers and environments often hold tokens, so the file is private.
    config::update(home, GLOBAL, true, |root| {
        let servers = root.entry("mcpServers").or_insert_with(|| Value::Object(Map::new()));
        if !servers.is_object() {
            *servers = Value::Object(Map::new());
        }
        servers[server.name.as_str()] = spec;
    })
}

/// Removes server `name` from agt's own file. Returns false when it lists
/// no such server.
pub(crate) fn remove(home: &Path, name: &str) -> io::Result<bool> {
    let mut removed = false;
    config::update(home, GLOBAL, true, |root| {
        removed = root
            .get_mut("mcpServers")
            .and_then(Value::as_object_mut)
            .is_some_and(|servers| servers.remove(name).is_some());
    })?;
    Ok(removed)
}

fn object(pairs: &[(String, String)]) -> Map<String, Value> {
    pairs.iter().map(|(name, value)| (name.clone(), value.clone().into())).collect()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "TOKEN" => Some("secret".into()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn settings_read_the_shapes_other_agents_write() {
        let text = r#"{"mcpServers": {
            "playwright": {"command": "npx", "args": ["-y", "@playwright/mcp", "--headless"], "env": {"DEBUG": "1"}, "cwd": "tools"},
            "github": {"type": "http", "url": "https://api.githubcopilot.com/mcp/", "headers": {"Authorization": "Bearer ${TOKEN}"}},
            "docs": {"url": "https://mcp.example.com/mcp"},
            "old": {"type": "sse", "url": "https://mcp.example.com/sse"},
            "bad name": {"command": "x"},
            "empty": {"command": ""},
            "unset": {"command": "run", "args": ["${MISSING}"]},
            "fallback": {"command": "run", "args": ["${MISSING:-default}", "${EMPTY}x"]}
        }}"#;
        let found = parse(text, Path::new("/project"), &env).expect("valid file");
        assert_eq!(
            found[0],
            Err("server \"bad name\": a name is up to 64 letters, digits, `-`, `_` and `.`".into())
        );
        let named = |name: &str| {
            found.iter().find(|server| match server {
                Ok(server) => server.name == name,
                Err(problem) => problem.starts_with(&format!("server {name}:")),
            })
        };
        assert_eq!(
            named("playwright"),
            Some(&Ok(Server {
                name: "playwright".into(),
                transport: Transport::Stdio {
                    command: "npx".into(),
                    args: vec!["-y".into(), "@playwright/mcp".into(), "--headless".into()],
                    env: vec![("DEBUG".into(), "1".into())],
                    cwd: Some(PathBuf::from("/project/tools")),
                },
            }))
        );
        assert_eq!(
            named("github"),
            Some(&Ok(Server {
                name: "github".into(),
                transport: Transport::Http {
                    url: "https://api.githubcopilot.com/mcp/".into(),
                    headers: vec![("Authorization".into(), "Bearer secret".into())],
                },
            }))
        );
        assert!(matches!(
            named("docs"),
            Some(Ok(Server { transport: Transport::Http { .. }, .. }))
        ));
        for (name, problem) in [
            ("old", "the deprecated SSE transport is not supported"),
            ("empty", "command is missing"),
            ("unset", "${MISSING} is not set"),
        ] {
            let found = named(name).and_then(|server| server.as_ref().err());
            assert!(found.is_some_and(|found| found.contains(problem)), "{name}: {found:?}");
        }
        let Some(Ok(Server { transport: Transport::Stdio { args, .. }, .. })) = named("fallback")
        else {
            panic!("fallback server: {:?}", named("fallback"));
        };
        assert_eq!(args, &["default", "x"], "a default fills an unset variable; a set one wins");
        assert_eq!(
            parse("[]", Path::new("/"), &env).err().as_deref(),
            Some("expected a JSON object")
        );
        assert_eq!(parse("{}", Path::new("/"), &env), Ok(Vec::new()));
    }

    #[test]
    fn projects_and_editors_replace_servers_of_the_same_name() {
        let root = tempfile::tempdir().expect("temp dir");
        let (home, project) = (root.path().join("home"), root.path().join("project"));
        let cwd = project.join("src/deep");
        fs::create_dir_all(&cwd).expect("dirs");
        fs::create_dir_all(project.join(".git")).expect("git");
        fs::create_dir_all(&home).expect("home");
        let stdio = |command: &str| Transport::Stdio {
            command: command.into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let server = |name: &str, command| Server { name: name.into(), transport: stdio(command) };
        add(&home, &server("shared", "global")).expect("add");
        add(&home, &server("mine", "mine")).expect("add");
        fs::write(project.join(PROJECT), r#"{"mcpServers": {"shared": {"command": "project"}}}"#)
            .expect("project file");
        let given = Server { name: "mine".into(), transport: stdio("editor") };
        let (servers, problems) = servers(&home, &cwd, &[given]);
        assert!(problems.is_empty(), "{problems:?}");
        let commands: Vec<(String, String)> = servers
            .iter()
            .map(|server| (server.name.clone(), server.transport.to_string()))
            .collect();
        assert_eq!(
            commands,
            [("mine".into(), "editor".into()), ("shared".into(), "project".into())]
        );
        assert!(remove(&home, "shared").expect("remove"));
        assert!(!remove(&home, "shared").expect("remove again"));
    }
}
