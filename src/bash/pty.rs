//! Starting a command in a pseudo-terminal of its own, and the threads that
//! read its output, wait for its exit and type its input.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::Duration;

use pty_process::blocking::{Command, Pty};
use rustix::process::{Pid, Resource, Rlimit, Signal};

use super::log::Log;
use super::{Event, Exit, OnEvent, SESSION_DIR, lock};
use crate::control;

/// The size of a command's terminal.
pub(super) const ROWS: u16 = 40;
pub(super) const COLS: u16 = 120;
/// How long an exit report waits for the reader to drain the terminal.
const DRAIN: Duration = Duration::from_millis(200);
/// The soft limit on open files macOS accepts at most.
const OPEN_MAX: u64 = 10_240;

/// How commands start: the shell, the working directory and the environment.
pub(super) struct Launcher {
    shell: &'static str,
    pub(super) cwd: PathBuf,
    /// Variables every command gets.
    vars: Vec<(&'static str, OsString)>,
    /// Whether commands can reach the session's control socket; a session
    /// without one must not reach the socket of an agt it runs in.
    socket: bool,
}

impl Launcher {
    /// Starts commands in `cwd` for the session kept in directory `session`,
    /// whose control socket is `socket`.
    pub(super) fn new(session: &Path, cwd: PathBuf, socket: Option<&Path>) -> Self {
        raise_file_limit();
        let path = std::env::var_os("PATH");
        let shell = if path.as_deref().and_then(|path| which("bash", path)).is_some() {
            "bash"
        } else {
            "sh"
        };
        let mut vars = vec![
            ("TERM", "xterm-256color".into()),
            ("PAGER", "cat".into()),
            ("GIT_PAGER", "cat".into()),
            ("NO_COLOR", "1".into()),
            (SESSION_DIR, session.into()),
        ];
        if let Some(socket) = socket {
            vars.push((control::SOCKET, socket.into()));
        }
        let exe = std::env::current_exe().ok();
        if let Some(path) =
            path.zip(exe).and_then(|(path, exe)| commands_path(session, &path, &exe))
        {
            vars.push(("PATH", path));
        }
        Self { shell, cwd, vars, socket: socket.is_some() }
    }

    /// Starts `command` as process `id`, writing its output to `log` and
    /// reporting through `on`.
    pub(super) fn spawn(
        &self,
        id: u32,
        command: &str,
        log: &Arc<Mutex<Log>>,
        on: &OnEvent,
    ) -> Result<Child, String> {
        let (pty, pts) = pty_process::blocking::open()
            .map_err(|error| format!("cannot open a terminal: {error}"))?;
        pty.resize(pty_process::Size::new(ROWS, COLS))
            .map_err(|error| format!("cannot size the terminal: {error}"))?;
        let mut shell = Command::new(self.shell)
            .arg("-c")
            .arg(command)
            .current_dir(&self.cwd)
            .envs(self.vars.iter().map(|(name, value)| (name, value)))
            .env_remove("AGT_API_KEY");
        if !self.socket {
            shell = shell.env_remove(control::SOCKET);
        }
        let mut child =
            shell.spawn(pts).map_err(|error| format!("cannot start command: {error}"))?;
        let pid = Pid::from_child(&child);
        let pty = Arc::new(pty);
        let pending = Arc::new(AtomicBool::new(false));
        let reaped = Arc::new(AtomicBool::new(false));
        let (drained, drained_rx) = mpsc::channel::<()>();

        let reader = {
            let (pty, log, pending, on) =
                (Arc::clone(&pty), Arc::clone(log), Arc::clone(&pending), Arc::clone(on));
            move || {
                let _drained = drained;
                let mut buf = vec![0; 64 * 1024];
                loop {
                    match (&*pty).read(&mut buf) {
                        Ok(0) => break,
                        Ok(read) => {
                            lock(&log).append(&buf[..read]);
                            if !pending.swap(true, Ordering::AcqRel) {
                                on(Event::Output(id));
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        // Linux reports EIO once the last terminal descriptor closes.
                        Err(_) => break,
                    }
                }
                lock(&log).finish();
            }
        };
        let waiter = {
            let (on, reaped) = (Arc::clone(on), Arc::clone(&reaped));
            move || {
                let exit = Exit::from_status(child.wait());
                reaped.store(true, Ordering::Release);
                // Output still buffered in the terminal belongs before the exit;
                // a lingering grandchild holding the terminal only delays this.
                let _ = drained_rx.recv_timeout(DRAIN);
                on(Event::Exit(id, exit));
            }
        };
        // The waiter starts first so the child is reaped even if the reader
        // cannot start.
        let threads = thread::Builder::new()
            .name(format!("agt-wait-{id}"))
            .spawn(waiter)
            .and_then(|_| thread::Builder::new().name(format!("agt-proc-{id}")).spawn(reader));
        if let Err(error) = threads {
            let _ = rustix::process::kill_process_group(pid, Signal::KILL);
            return Err(format!("cannot start command: {error}"));
        }
        Ok(Child { pid, pty: Some(pty), reaped, pending, input: None })
    }
}

/// A started process: its terminal, and what its threads share with it.
pub(super) struct Child {
    pid: Pid,
    /// The terminal, released when the process exits.
    pty: Option<Arc<Pty>>,
    /// Set by the waiter as soon as the process is reaped, after which its
    /// process group id may belong to an unrelated process.
    reaped: Arc<AtomicBool>,
    /// Set while an output event is on its way, so output events coalesce
    /// until the agent acknowledges one.
    pub(super) pending: Arc<AtomicBool>,
    /// Sends input to the thread that types it, once there is any.
    input: Option<SyncSender<Vec<u8>>>,
}

impl Child {
    /// Signals the process group, until the process is reaped.
    pub(super) fn signal(&self, signal: Signal) {
        if !self.reaped.load(Ordering::Acquire) {
            // The group may have exited since the check; nothing to do then.
            let _ = rustix::process::kill_process_group(self.pid, signal);
        }
    }

    /// Kills what is left of the process group after the shell exited, such
    /// as children that ignore SIGTERM. A group id is not reused while
    /// members remain, so this is safe even once the shell is reaped.
    pub(super) fn kill_remaining(&self) {
        let _ = rustix::process::kill_process_group(self.pid, Signal::KILL);
    }

    /// Queues `bytes` to be typed. Writes go through a thread, so a process
    /// that stops reading cannot block the agent, and they stay in order.
    pub(super) fn write(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        let Some(pty) = &self.pty else {
            return Err("it has exited".into());
        };
        let sender = match &self.input {
            Some(sender) => sender,
            None => {
                let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(1);
                let pty = Arc::clone(pty);
                thread::Builder::new()
                    .name("agt-pty-input".into())
                    .spawn(move || {
                        for bytes in receiver {
                            if (&*pty).write_all(&bytes).is_err() {
                                break;
                            }
                        }
                    })
                    .map_err(|error| format!("cannot write input: {error}"))?;
                self.input.insert(sender)
            }
        };
        sender.try_send(bytes).map_err(|error| match error {
            TrySendError::Full(_) => {
                "it has not read the previous input yet; read its output before typing more".into()
            }
            TrySendError::Disconnected(_) => "it no longer accepts input".into(),
        })
    }

    /// Releases the terminal of a process that exited.
    pub(super) fn release(&mut self) {
        self.pty = None;
        self.input = None;
    }
}

/// The bytes typed for `input`. Models expect a typed line to run, so text is
/// followed by Enter, and its newlines are typed as Enter (CR), which programs
/// reading raw keys expect. Input carrying control keys (Ctrl-C, Esc, arrow
/// keys) is sent exactly as given.
pub(super) fn keystrokes(input: &str) -> Vec<u8> {
    let keys = input.chars().any(|c| c.is_control() && c != '\n' && c != '\t');
    if keys {
        return input.as_bytes().to_vec();
    }
    let mut bytes = input.replace('\n', "\r").into_bytes();
    if !input.ends_with('\n') {
        bytes.push(b'\r');
    }
    bytes
}

/// Raises the soft limit on open files as far as it goes, once: each running
/// process holds its terminal and log open, and many run at once. macOS takes
/// no soft limit above `OPEN_MAX`, so that is tried when the hard limit fails.
fn raise_file_limit() {
    static RAISED: Once = Once::new();
    RAISED.call_once(|| {
        let limit = rustix::process::getrlimit(Resource::Nofile);
        let Some(current) = limit.current else { return };
        for target in [limit.maximum, Some(OPEN_MAX)] {
            let target = target.unwrap_or(OPEN_MAX).min(limit.maximum.unwrap_or(u64::MAX));
            if target <= current {
                return;
            }
            let raised = Rlimit { current: Some(target), maximum: limit.maximum };
            if rustix::process::setrlimit(Resource::Nofile, raised).is_ok() {
                return;
            }
        }
    });
}

/// The file `program` runs from with `path` as `PATH`.
fn which(program: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path).map(|dir| dir.join(program)).find(|file| file.is_file())
}

/// `PATH` for commands when `path` would not run this binary, `exe`, as
/// `agt`: `agt view` must write the markers this process reads, and `agt mcp`
/// reach its socket, so the session's `bin/`, where `agt` links to `exe`,
/// leads it. `None` leaves `PATH` as it is, when `agt` already is `exe` or the
/// link cannot be made.
fn commands_path(session: &Path, path: &OsStr, exe: &Path) -> Option<OsString> {
    let this = fs::canonicalize(exe).ok();
    if this.is_some() && which("agt", path).and_then(|agt| fs::canonicalize(agt).ok()) == this {
        return None;
    }
    let bin = session.join("bin");
    let link = bin.join("agt");
    if fs::read_link(&link).ok().as_deref() != Some(exe) {
        fs::create_dir_all(&bin).ok()?;
        // A session resumed by another build of agt links to that build.
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(exe, &link).ok()?;
    }
    std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(path))).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_text_presses_enter_but_keys_are_exact() {
        assert_eq!(keystrokes("print(1)"), b"print(1)\r");
        assert_eq!(keystrokes("a\nb"), b"a\rb\r");
        assert_eq!(keystrokes("y\n"), b"y\r");
        assert_eq!(keystrokes("\u{3}"), b"\x03");
        assert_eq!(keystrokes("\u{1b}[B"), b"\x1b[B");
        assert_eq!(keystrokes(":wq\u{1b}"), b":wq\x1b");
    }

    #[test]
    fn commands_find_this_binary_as_agt() {
        let root = tempfile::tempdir().expect("temp dir");
        let (session, installed, other) =
            (root.path().join("session"), root.path().join("installed"), root.path().join("other"));
        for dir in [&installed, &other] {
            fs::create_dir_all(dir).expect("directory");
            fs::write(dir.join("agt"), "").expect("binary");
        }
        let exe = installed.join("agt");
        let path = |dirs: [&Path; 2]| std::env::join_paths(dirs).expect("PATH");
        let already = commands_path(&session, &path([&installed, &other]), &exe);
        assert_eq!(already, None, "agt on PATH already is this binary");
        assert!(!session.join("bin").exists(), "and no link is made");
        let led = commands_path(&session, &path([&other, &installed]), &exe).expect("a PATH");
        let dirs: Vec<PathBuf> = std::env::split_paths(&led).collect();
        assert_eq!(dirs, [session.join("bin"), other, installed]);
        assert_eq!(fs::read_link(session.join("bin/agt")).expect("link"), exe);
    }

    #[test]
    fn the_open_file_limit_is_raised() {
        let before = rustix::process::getrlimit(Resource::Nofile);
        raise_file_limit();
        let after = rustix::process::getrlimit(Resource::Nofile);
        assert!(after.current >= before.current, "{before:?} → {after:?}");
    }
}
