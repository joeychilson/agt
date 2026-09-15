//! What agt remembers of each server between sessions: the era it speaks,
//! what it is for and its tools' names, which the system prompt lists.

use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::thread;

use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::connection::Era;
use super::{Server, Transport};

/// A server as the system prompt lists it.
#[derive(Debug, PartialEq)]
pub(crate) struct Listing {
    pub(crate) name: String,
    /// The program or address that serves it.
    pub(crate) target: String,
    /// What the server is for, as it said when last used.
    pub(crate) about: Option<String>,
    /// Its tools when last listed.
    pub(crate) tools: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct Remembered {
    #[serde(default)]
    pub(super) era: Option<Era>,
    #[serde(default)]
    pub(super) about: Option<String>,
    #[serde(default)]
    pub(super) tools: Vec<String>,
}

/// `servers` as the system prompt lists them.
pub(crate) fn listings(home: &Path, servers: &[Server]) -> Vec<Listing> {
    servers
        .iter()
        .map(|server| {
            let remembered = recall(home, server);
            Listing {
                name: server.name.clone(),
                target: server.transport.to_string(),
                about: remembered.about,
                tools: remembered.tools,
            }
        })
        .collect()
}

pub(super) fn recall(home: &Path, server: &Server) -> Remembered {
    let bytes = fs::read(path(home, server)).ok();
    bytes.and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

/// Changes what agt remembers of `server`. A failure to save is ignored:
/// the prompt then lists less.
pub(super) fn remember(home: &Path, server: &Server, change: impl FnOnce(&mut Remembered)) {
    let path = path(home, server);
    let mut remembered = recall(home, server);
    change(&mut remembered);
    let bytes = serde_json::to_vec_pretty(&remembered).expect("JSON values always serialize");
    // Written aside and renamed, so a reader never sees part of it.
    let temp = path.with_extension(format!("{}.{:?}.tmp", process::id(), thread::current().id()));
    let saved = path
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::write(&temp, bytes))
        .and_then(|()| fs::rename(&temp, &path));
    if saved.is_err() {
        let _ = fs::remove_file(&temp);
    }
}

/// The file agt remembers `server` in, named for the server and a hash of
/// where it is, so servers of one name in different projects stay apart.
/// Environments and headers are left out: they hold tokens, which change
/// while the server stays the same.
fn path(home: &Path, server: &Server) -> PathBuf {
    let place = match &server.transport {
        Transport::Stdio { command, args, cwd, .. } => json!([command, args, cwd]),
        Transport::Http { url, .. } => json!([url]),
    };
    let bytes = serde_json::to_vec(&place).expect("JSON values always serialize");
    let hash: String =
        digest(&SHA256, &bytes).as_ref()[..8].iter().map(|byte| format!("{byte:02x}")).collect();
    home.join("mcp").join(format!("{}-{hash}.json", server.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_are_remembered_apart_by_how_they_are_reached() {
        let home = tempfile::tempdir().expect("temp dir");
        let server = |command: &str| Server {
            name: "tools".into(),
            transport: Transport::Stdio {
                command: command.into(),
                args: vec![],
                env: vec![],
                cwd: None,
            },
        };
        let (first, second) = (server("one"), server("two"));
        assert_eq!(
            listings(home.path(), std::slice::from_ref(&first)),
            [Listing { name: "tools".into(), target: "one".into(), about: None, tools: vec![] }]
        );
        remember(home.path(), &first, |memory| {
            memory.era = Some(Era::Legacy);
            memory.about = Some("Does things.".into());
            memory.tools = vec!["a".into(), "b".into()];
        });
        assert_eq!(recall(home.path(), &first).era, Some(Era::Legacy));
        assert_eq!(
            listings(home.path(), &[first, second]),
            [
                Listing {
                    name: "tools".into(),
                    target: "one".into(),
                    about: Some("Does things.".into()),
                    tools: vec!["a".into(), "b".into()]
                },
                Listing { name: "tools".into(), target: "two".into(), about: None, tools: vec![] },
            ]
        );
    }
}
