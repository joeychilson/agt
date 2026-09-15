//! Servers agt starts: newline-delimited JSON-RPC over their standard
//! streams, with the requests of any number of callers matched to responses
//! by id.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal};
use serde_json::{Value, json};

use super::connection::{Error, Wait, answer, outcome};
use super::lock;

/// How often a waiting request looks at whether its caller gave up.
const TICK: Duration = Duration::from_millis(100);
/// How long a stopping server gets at each step: its input closed, then
/// SIGTERM, then SIGKILL.
const GRACE: Duration = Duration::from_secs(2);
/// Bytes of a server's log quoted when it stops unexpectedly.
const LOG_TAIL: u64 = 1024;

/// A server process.
pub(super) struct Process {
    child: Option<Child>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    state: Arc<Mutex<State>>,
    next_id: AtomicU64,
}

/// What the reader thread shares with callers.
#[derive(Default)]
struct State {
    waiting: HashMap<u64, mpsc::Sender<Result<Value, Error>>>,
    /// Why the server stopped, once it has.
    stopped: Option<String>,
}

impl Process {
    /// Starts `command` in `cwd` in a process group of its own, with its
    /// standard error in `log`, or agt's own without one.
    pub(super) fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
        log: Option<&Path>,
    ) -> Result<Self, String> {
        let stderr = match log {
            Some(path) => {
                let file = path
                    .parent()
                    .map_or(Ok(()), fs::create_dir_all)
                    .and_then(|()| File::create(path))
                    .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
                Stdio::from(file)
            }
            None => Stdio::inherit(),
        };
        let mut child = Command::new(command)
            .args(args)
            // As in the agent's commands, agt's own key is not passed on.
            .env_remove("AGT_API_KEY")
            .envs(env.iter().map(|(name, value)| (name, value)))
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .process_group(0)
            .spawn()
            .map_err(|error| format!("cannot start {command}: {error}"))?;
        let stdout = child.stdout.take().ok_or("the server has no output")?;
        let stdin = Arc::new(Mutex::new(child.stdin.take()));
        let state = Arc::new(Mutex::new(State::default()));
        let reader = {
            let (stdin, state) = (Arc::clone(&stdin), Arc::clone(&state));
            let log = log.map(Path::to_path_buf);
            move || read(stdout, &stdin, &state, log)
        };
        thread::Builder::new()
            .name("agt-mcp-stdio".into())
            .spawn(reader)
            .map_err(|error| format!("cannot read the server: {error}"))?;
        Ok(Self { child: Some(child), stdin, state, next_id: AtomicU64::new(1) })
    }

    pub(super) fn alive(&self) -> bool {
        lock(&self.state).stopped.is_none()
    }

    pub(super) fn request(
        &self,
        method: &str,
        params: Value,
        wait: Wait<'_>,
    ) -> Result<Value, Error> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        {
            let mut state = lock(&self.state);
            if let Some(reason) = &state.stopped {
                return Err(Error::Failed(reason.clone()));
            }
            state.waiting.insert(id, sender);
        }
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(error) = write(&self.stdin, &message) {
            lock(&self.state).waiting.remove(&id);
            return Err(error);
        }
        loop {
            let step = wait
                .until
                .map_or(TICK, |until| until.saturating_duration_since(Instant::now()).min(TICK));
            match receiver.recv_timeout(step) {
                Ok(reply) => return reply,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let reason = lock(&self.state).stopped.clone();
                    return Err(Error::Failed(
                        reason.unwrap_or_else(|| "the server stopped".into()),
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let late = wait.until.is_some_and(|until| Instant::now() >= until);
                    if !wait.cancelled() && !late {
                        continue;
                    }
                    lock(&self.state).waiting.remove(&id);
                    let reason = if late { "timed out" } else { "the caller stopped waiting" };
                    let cancelled = json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/cancelled",
                        "params": { "requestId": id, "reason": reason },
                    });
                    let _ = write(&self.stdin, &cancelled);
                    return Err(if late { Error::TimedOut } else { Error::Cancelled });
                }
            }
        }
    }

    pub(super) fn notify(&self, method: &str) -> Result<(), Error> {
        write(&self.stdin, &json!({ "jsonrpc": "2.0", "method": method }))
    }
}

impl Drop for Process {
    /// Closes the server's input, which asks it to exit, and makes sure on a
    /// thread that it does.
    fn drop(&mut self) {
        lock(&self.stdin).take();
        if let Some(child) = self.child.take() {
            let _ = thread::Builder::new().name("agt-mcp-stop".into()).spawn(move || stop(child));
        }
    }
}

fn stop(mut child: Child) {
    let pid = Pid::from_child(&child);
    for signal in [None, Some(Signal::TERM), Some(Signal::KILL)] {
        if let Some(signal) = signal {
            // Signalled only while unreaped, when its group id is still its own.
            let _ = rustix::process::kill_process_group(pid, signal);
        }
        let deadline = Instant::now() + GRACE;
        while Instant::now() < deadline {
            if !matches!(child.try_wait(), Ok(None)) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    let _ = child.wait();
}

fn write(stdin: &Mutex<Option<ChildStdin>>, message: &Value) -> Result<(), Error> {
    let mut stdin = lock(stdin);
    let stdin =
        stdin.as_mut().ok_or_else(|| Error::Failed("the server's input is closed".into()))?;
    let mut line = serde_json::to_vec(message).expect("JSON values always serialize");
    line.push(b'\n');
    stdin
        .write_all(&line)
        .and_then(|()| stdin.flush())
        .map_err(|error| Error::Failed(format!("cannot write to the server: {error}")))
}

/// Reads the server's messages until it closes its output: responses go to
/// their callers, a legacy server's requests are answered, and notifications
/// are dropped. Callers still waiting then learn why it stopped.
fn read(
    stdout: impl Read,
    stdin: &Mutex<Option<ChildStdin>>,
    state: &Mutex<State>,
    log: Option<PathBuf>,
) {
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        match (message.get("id"), message["method"].as_str()) {
            (Some(id), None) => {
                let sender = id.as_u64().and_then(|id| lock(state).waiting.remove(&id));
                if let Some(sender) = sender {
                    let _ = sender.send(outcome(&message));
                }
            }
            (Some(id), Some(method)) => {
                let _ = write(stdin, &answer(id, method));
            }
            (None, _) => {}
        }
    }
    let mut reason = String::from("the server stopped");
    if let Some(tail) = log.as_deref().and_then(tail) {
        reason.push_str(": ");
        reason.push_str(&tail);
    }
    let mut state = lock(state);
    state.stopped = Some(reason.clone());
    for (_, sender) in state.waiting.drain() {
        let _ = sender.send(Err(Error::Failed(reason.clone())));
    }
}

/// The last lines of a log, as one line.
fn tail(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL))).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    let last = &lines[lines.len().saturating_sub(3)..];
    (!last.is_empty()).then(|| last.join(" / "))
}
