//! The servers of a session and its connections to them, each opened the
//! first time its server is used.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::catalog;
use super::connection::{Connection, Error, Info, Tool, Wait};
use super::{Server, Transport, lock, schema};

/// The servers a session has, as `agt mcp list` shows them.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Status {
    pub(crate) servers: Vec<ServerStatus>,
    /// Problems with the settings, such as a server left out.
    pub(crate) problems: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ServerStatus {
    pub(crate) name: String,
    /// `stdio` or `http`.
    pub(crate) transport: String,
    /// The program or address.
    pub(crate) target: String,
    pub(crate) running: bool,
}

/// A server's tools, as `agt mcp tools` shows them.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Tools {
    pub(super) info: Info,
    pub(super) tools: Vec<Tool>,
    /// Tools left out, each with the reason.
    pub(super) left_out: Vec<String>,
}

/// Characters of each tool's description that the list of tools shows.
const DESCRIPTION: usize = 400;

impl Tools {
    /// The tools as `agt mcp tools` lists them: what server `server` says of
    /// itself, then each tool's signature and the start of its description.
    pub(crate) fn describe(&self, server: &str) -> String {
        let mut out = server.to_owned();
        if let Some(title) = &self.info.title {
            out.push_str(&format!(" · {title}"));
        }
        out.push('\n');
        if let Some(instructions) = &self.info.instructions {
            out.push_str(&format!("{}\n", instructions.trim()));
        }
        for tool in &self.tools {
            out.push_str(&format!("\n{}\n", schema::signature(&tool.name, &tool.input_schema)));
            if let Some(description) = &tool.description {
                let paragraph = description.trim().split("\n\n").next().unwrap_or_default();
                let mut shown: String = paragraph.chars().take(DESCRIPTION).collect();
                if shown.len() < description.trim().len() {
                    shown.push('…');
                }
                for line in shown.lines() {
                    out.push_str(&format!("  {line}\n"));
                }
            }
        }
        if self.tools.is_empty() {
            out.push_str("\nno tools\n");
        }
        for left_out in &self.left_out {
            out.push_str(&format!("\nleft out {left_out}\n"));
        }
        out
    }

    /// Tool `name` of server `server` in full: its signature, description and
    /// arguments, or why the server has no such tool.
    pub(crate) fn describe_tool(&self, server: &str, name: &str) -> Result<String, String> {
        let Some(tool) = self.tools.iter().find(|tool| tool.name == name) else {
            return Err(unknown_tool(server, name, &self.tools));
        };
        let mut out = schema::signature(&tool.name, &tool.input_schema);
        if let Some(description) = &tool.description {
            out.push_str(&format!("\n\n{}", description.trim()));
        }
        let arguments =
            serde_json::to_string_pretty(&tool.input_schema).expect("JSON values always serialize");
        Ok(format!("{out}\n\nArguments, as JSON Schema:\n{arguments}"))
    }
}

pub(crate) struct Pool {
    home: PathBuf,
    cwd: PathBuf,
    /// Servers an editor gave the session.
    given: Vec<Server>,
    /// Where servers log their standard error, or `None` for agt's own.
    logs: Option<PathBuf>,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
}

/// A server and its connection, which the first caller opens while others
/// wait for it.
struct Slot {
    server: Server,
    connection: Mutex<Option<Arc<Connection>>>,
}

impl Pool {
    pub(crate) fn new(
        home: PathBuf,
        cwd: PathBuf,
        given: Vec<Server>,
        logs: Option<PathBuf>,
    ) -> Self {
        Self { home, cwd, given, logs, slots: Mutex::default() }
    }

    /// The servers, read from their settings again so servers added since
    /// the session began are there.
    fn servers(&self) -> (Vec<Server>, Vec<String>) {
        super::servers(&self.home, &self.cwd, &self.given)
    }

    pub(crate) fn status(&self) -> Status {
        let (servers, problems) = self.servers();
        let slots = lock(&self.slots).clone();
        let servers = servers
            .into_iter()
            .map(|server| {
                // A server still starting holds its slot, and is not running yet.
                let running = slots
                    .get(&server.name)
                    .filter(|slot| slot.server == server)
                    .is_some_and(|slot| {
                        slot.connection.try_lock().is_ok_and(|open| {
                            open.as_ref().is_some_and(|connection| connection.alive())
                        })
                    });
                let transport = match server.transport {
                    Transport::Stdio { .. } => "stdio",
                    Transport::Http { .. } => "http",
                };
                ServerStatus {
                    target: server.transport.to_string(),
                    transport: transport.into(),
                    name: server.name,
                    running,
                }
            })
            .collect();
        Status { servers, problems }
    }

    /// Server `name` and its connection, opened if it is not open, or open
    /// with settings that have changed since.
    fn connection(
        &self,
        name: &str,
        cancel: &AtomicBool,
    ) -> Result<(Server, Arc<Connection>), String> {
        let (servers, _) = self.servers();
        let Some(server) = servers.iter().find(|server| server.name == name).cloned() else {
            let names: Vec<&str> = servers.iter().map(|server| server.name.as_str()).collect();
            return Err(if names.is_empty() {
                "no MCP servers are set up; add one with `agt mcp add`".into()
            } else {
                format!("there is no MCP server {name:?}; the servers are {}", names.join(", "))
            });
        };
        let slot = {
            let mut slots = lock(&self.slots);
            let slot = slots.entry(name.to_owned()).or_insert_with(|| Arc::new(Slot::new(&server)));
            if slot.server != server {
                *slot = Arc::new(Slot::new(&server));
            }
            Arc::clone(slot)
        };
        let mut open = lock(&slot.connection);
        if let Some(connection) = open.as_ref().filter(|connection| connection.alive()) {
            return Ok((server, Arc::clone(connection)));
        }
        let remembered = catalog::recall(&self.home, &server).era;
        let log = self.logs.as_ref().map(|dir| dir.join(format!("{name}.log")));
        let connection = Connection::open(&server, &self.cwd, remembered, log.as_deref(), cancel)
            .map_err(|error| format!("cannot start MCP server {name}: {error}"))?;
        let connection = Arc::new(connection);
        catalog::remember(&self.home, &server, |memory| {
            memory.era = Some(connection.era);
            memory.about = connection.info.about().or(memory.about.take());
        });
        *open = Some(Arc::clone(&connection));
        Ok((server, connection))
    }

    pub(crate) fn tools(&self, name: &str, cancel: &AtomicBool) -> Result<Tools, String> {
        let (server, connection) = self.connection(name, cancel)?;
        let wait = Wait { until: None, cancel };
        let (tools, left_out) =
            connection.list_tools(wait).map_err(|error| format!("{name}: {error}"))?;
        self.remember_tools(&server, &tools);
        Ok(Tools { info: connection.info.clone(), tools, left_out })
    }

    /// Calls `tool` of server `name` and returns its result. A tool the
    /// server lacks, or a call missing required arguments, is refused with
    /// what to call instead.
    pub(crate) fn call(
        &self,
        name: &str,
        tool: &str,
        arguments: &Value,
        cancel: &AtomicBool,
    ) -> Result<Value, String> {
        let (server, connection) = self.connection(name, cancel)?;
        let wait = Wait { until: None, cancel };
        let failed = |error: Error| format!("{name} {tool}: {error}");
        let mut tools = connection.tools(wait).map_err(failed)?;
        if !tools.iter().any(|listed| listed.name == tool) {
            // The server may have gained the tool since it was listed.
            tools = connection.list_tools(wait).map_err(failed)?.0;
            self.remember_tools(&server, &tools);
        }
        let Some(listed) = tools.iter().find(|listed| listed.name == tool) else {
            return Err(unknown_tool(name, tool, &tools));
        };
        let missing = schema::missing(&listed.input_schema, arguments);
        if !missing.is_empty() {
            return Err(format!(
                "{tool} needs {}: {}",
                missing.join(", "),
                schema::signature(tool, &listed.input_schema)
            ));
        }
        connection.call_tool(tool, arguments, wait).map_err(failed)
    }

    fn remember_tools(&self, server: &Server, tools: &[Tool]) {
        catalog::remember(&self.home, server, |memory| {
            memory.tools = tools.iter().map(|tool| tool.name.clone()).collect();
        });
    }
}

impl Slot {
    fn new(server: &Server) -> Self {
        Self { server: server.clone(), connection: Mutex::new(None) }
    }
}

/// Why server `server` cannot call `tool`: the tools with a name close to
/// it, or all of them.
pub(super) fn unknown_tool(server: &str, tool: &str, tools: &[Tool]) -> String {
    let names: Vec<&str> = tools.iter().map(|listed| listed.name.as_str()).collect();
    let close: Vec<&str> =
        names.iter().copied().filter(|name| name.contains(tool) || tool.contains(name)).collect();
    if close.is_empty() {
        format!("{server} has no tool {tool:?}; its tools are {}", names.join(", "))
    } else {
        format!("{server} has no tool {tool:?}; did you mean {}?", close.join(" or "))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn unknown_tools_name_the_close_ones() {
        let tool =
            |name: &str| Tool { name: name.into(), description: None, input_schema: json!({}) };
        let tools = [tool("list_issues"), tool("get_issue"), tool("search_code")];
        assert_eq!(
            unknown_tool("github", "list_issue", &tools),
            "github has no tool \"list_issue\"; did you mean list_issues?"
        );
        assert_eq!(
            unknown_tool("github", "merge", &tools),
            "github has no tool \"merge\"; its tools are list_issues, get_issue, search_code"
        );
    }
}
