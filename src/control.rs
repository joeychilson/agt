//! A live session's control socket, through which `agt` commands reach it.
//!
//! Each session serves a Unix socket while it runs. Commands in the session
//! find it in `AGT_SOCKET`, and any process finds a session's socket by the
//! session's id. `agt mcp` sends requests the session's MCP servers answer, so
//! a browser a server opened stays open from call to call, and `agt send`
//! sends messages to the session's agent. A connection carries one request
//! and its answer, served on a thread of its own so a slow server holds up no
//! other request, and closing the connection early cancels the request.

use std::fs::{self, Permissions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::mcp::Pool;
use crate::store;

/// The variable that gives a session's commands its socket.
pub(crate) const SOCKET: &str = "AGT_SOCKET";

/// What a session's socket answers.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Request {
    /// The session's MCP servers, as `agt mcp list` shows them.
    Servers,
    /// A server's tools, starting it if it is not running.
    Tools { server: String },
    /// A tool's result.
    Call { server: String, tool: String, arguments: Value },
    /// A message for the agent: at its next step, or with `later` once it is
    /// done.
    Send { text: String, later: bool },
}

/// Delivers messages sent to the session to its agent.
pub(crate) type Deliver = Arc<dyn Fn(String, bool) + Send + Sync>;

/// A session's socket, which stops taking requests when dropped.
pub(crate) struct Socket {
    path: PathBuf,
    closed: Arc<AtomicBool>,
}

impl Socket {
    /// Serves session `id`'s socket, answering MCP requests from `pool` and
    /// passing messages to `deliver`.
    pub(crate) fn serve(id: &str, pool: Arc<Pool>, deliver: Deliver) -> io::Result<Self> {
        let path = path(id)?;
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, Permissions::from_mode(0o600))?;
        let closed = Arc::new(AtomicBool::new(false));
        let accept = {
            let closed = Arc::clone(&closed);
            move || {
                for stream in listener.incoming() {
                    if closed.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let (pool, deliver) = (Arc::clone(&pool), Arc::clone(&deliver));
                    let serve = move || serve(&pool, &deliver, stream);
                    let _ = thread::Builder::new().name("agt-control".into()).spawn(serve);
                }
            }
        };
        thread::Builder::new().name("agt-control-accept".into()).spawn(accept)?;
        Ok(Self { path, closed })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        // Wakes the accepting thread, which then finds the socket closed.
        let _ = UnixStream::connect(&self.path);
        let _ = fs::remove_file(&self.path);
    }
}

/// The socket of session `id`, in a directory only this user can use. Socket
/// paths are limited to about 100 bytes, which a session directory can pass,
/// so the socket lives in the temporary directory.
pub(crate) fn path(id: &str) -> io::Result<PathBuf> {
    store::check_id(id)?;
    let uid = rustix::process::getuid().as_raw();
    let dir = std::env::temp_dir().join(format!("agt-{uid}"));
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    let metadata = fs::symlink_metadata(&dir)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{} is not a directory only this user can use",
            dir.display()
        )));
    }
    Ok(dir.join(format!("{id}.sock")))
}

/// Whether session `id` is running: its socket answers.
pub(crate) fn live(id: &str) -> bool {
    path(id).is_ok_and(|path| UnixStream::connect(path).is_ok())
}

/// Answers one request. The caller closing its end first, as when its
/// command is stopped, cancels what it asked for.
fn serve(pool: &Pool, deliver: &Deliver, mut stream: UnixStream) {
    let Ok(read) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let watch = {
        let cancel = Arc::clone(&cancel);
        move || {
            let mut buf = [0; 256];
            while matches!(reader.read(&mut buf), Ok(1..)) {}
            cancel.store(true, Ordering::Relaxed);
        }
    };
    let _ = thread::Builder::new().name("agt-control-watch".into()).spawn(watch);
    let answer = match serde_json::from_str::<Request>(&line) {
        Ok(Request::Servers) => Ok(json!(pool.status())),
        Ok(Request::Tools { server }) => pool.tools(&server, &cancel).map(|tools| json!(tools)),
        Ok(Request::Call { server, tool, arguments }) => {
            pool.call(&server, &tool, &arguments, &cancel)
        }
        Ok(Request::Send { text, later }) => {
            deliver(text, later);
            Ok(Value::Null)
        }
        Err(error) => Err(format!("the session cannot read the request: {error}")),
    };
    let reply = match answer {
        Ok(value) => json!({ "ok": value }),
        Err(message) => json!({ "error": message }),
    };
    let mut bytes = serde_json::to_vec(&reply).expect("JSON values always serialize");
    bytes.push(b'\n');
    let _ = stream.write_all(&bytes);
}

/// Sends `request` to the socket at `socket` and returns its answer.
pub(crate) fn ask(socket: &Path, request: &Request) -> Result<Value, String> {
    let unreachable = |error: io::Error| format!("cannot reach the session: {error}");
    let mut stream = UnixStream::connect(socket).map_err(unreachable)?;
    let mut line = serde_json::to_vec(request).expect("JSON values always serialize");
    line.push(b'\n');
    stream.write_all(&line).map_err(unreachable)?;
    // The connection stays open until the answer comes, since closing it
    // cancels the request.
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply).map_err(unreachable)?;
    let reply: Value = serde_json::from_str(&reply)
        .map_err(|_| "the session stopped without answering".to_owned())?;
    match (reply.get("ok"), reply["error"].as_str()) {
        (Some(value), _) => Ok(value.clone()),
        (None, Some(message)) => Err(message.to_owned()),
        (None, None) => Err("the session answered with nothing".into()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn messages_reach_the_agent_and_mcp_requests_the_pool() {
        let home = tempfile::tempdir().expect("temp dir");
        let pool = Arc::new(Pool::new(home.path().into(), home.path().into(), Vec::new(), None));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let deliver: Deliver = {
            let sent = Arc::clone(&sent);
            Arc::new(move |text, later| sent.lock().expect("sent").push((text, later)))
        };
        let id = format!("1-{:x}", std::process::id());
        let socket = Socket::serve(&id, pool, deliver).expect("socket");
        assert!(live(&id));
        let send = Request::Send { text: "steer".into(), later: false };
        assert_eq!(ask(socket.path(), &send), Ok(Value::Null));
        assert_eq!(*sent.lock().expect("sent"), [("steer".to_owned(), false)]);
        let servers = ask(socket.path(), &Request::Servers).expect("servers");
        assert_eq!(servers["servers"], json!([]));
        let tools = ask(socket.path(), &Request::Tools { server: "gone".into() });
        assert_eq!(tools, Err("no MCP servers are set up; add one with `agt mcp add`".into()));
        drop(socket);
        assert!(!live(&id), "a closed socket is not live");
    }
}
