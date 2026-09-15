//! The bash tool: every command runs in its own pseudo-terminal.
//!
//! One code path serves plain commands, progress bars, REPLs, password
//! prompts and full-screen programs. A process keeps running when a call stops
//! waiting for it, so background work needs no separate mode, and the agent is
//! told when it exits. Output goes to a log file as a terminal shows it, which
//! the model can grep and which doubles as the read buffer, so memory stays
//! flat however much a process prints.

mod log;
mod pty;
mod tool;

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rustix::process::Signal;
use serde::{Deserialize, Serialize};

use crate::item::Content;
use log::Log;
pub(crate) use log::bytes;
pub(crate) use tool::{Action, CANCELLED, Header, Outcome, Tool, ToolCall, definition, elapsed};

/// The variable that gives commands their session's directory, where
/// `agt view` saves the images it shows and `agt fetch` the pages it reads.
pub(crate) const SESSION_DIR: &str = "AGT_SESSION_DIR";
/// Silence after new output that ends an input or read call.
const QUIET: Duration = Duration::from_millis(500);
/// Time between SIGTERM and SIGKILL when stopping a process.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// Output kept from the start and the end of a long result, as the tool
/// description tells the model.
const HEAD: u64 = 4 * 1024;
const TAIL: u64 = 8 * 1024;
/// Unread output an exit notice carries. A notice can outlive tool output
/// that masking elides, so it carries less.
const EXIT_HEAD: u64 = 1024;
const EXIT_TAIL: u64 = 3 * 1024;
/// Processes kept track of before the oldest that exited are forgotten; their
/// logs stay on disk.
const MAX_ENTRIES: usize = 64;
/// The folder of a session's directory that holds its process logs.
const PROCS: &str = "procs";

/// Reports what process threads see, from any thread.
pub(crate) type OnEvent = Arc<dyn Fn(Event) + Send + Sync>;

/// What process threads report.
#[derive(Debug)]
pub(crate) enum Event {
    /// Process `id` has new output. Events coalesce until one is acknowledged
    /// with `Procs::on_output`.
    Output(u32),
    Exit(u32, Exit),
}

/// How a process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Exit {
    Code(i32),
    Signal(i32),
    Unknown,
}

impl Exit {
    fn from_status(status: io::Result<ExitStatus>) -> Self {
        match status {
            Ok(status) => status
                .code()
                .map(Self::Code)
                .or_else(|| status.signal().map(Self::Signal))
                .unwrap_or(Self::Unknown),
            Err(_) => Self::Unknown,
        }
    }

    pub(crate) fn success(self) -> bool {
        self == Self::Code(0)
    }

    /// Reads an exit as `Display` writes it.
    fn parse(text: &str) -> Option<Self> {
        match text.split_once(' ') {
            Some(("exit", code)) => code.parse().ok().map(Self::Code),
            Some(("signal", signal)) => signal.parse().ok().map(Self::Signal),
            _ => (text == "unknown exit").then_some(Self::Unknown),
        }
    }
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Code(code) => write!(f, "exit {code}"),
            Self::Signal(signal) => write!(f, "signal {signal}"),
            Self::Unknown => f.write_str("unknown exit"),
        }
    }
}

/// What a call waits for before its result is collected.
#[derive(Debug)]
pub(crate) struct Wait {
    pub(crate) id: u32,
    until: Instant,
    /// Returns early once output newer than this instant has settled.
    settle: Option<Instant>,
    /// Whether the call started the process.
    started: bool,
    /// A process that was already running the same command.
    duplicate: Option<u32>,
}

impl Wait {
    /// Ends the wait by `until` at the latest.
    pub(crate) fn hurry(&mut self, until: Instant) {
        self.until = self.until.min(until);
    }
}

/// A collected call result.
pub(crate) struct Report {
    /// The result header, then the new output with the images shown in it.
    pub(crate) output: Content,
    pub(crate) exit: Option<Exit>,
}

/// A process of the session, as lists and checkpoints show it.
pub(crate) struct Task<'a> {
    pub(crate) id: u32,
    pub(crate) command: &'a str,
    /// How the process ended, or `None` while it runs.
    pub(crate) exit: Option<Exit>,
    pub(crate) started: Instant,
    pub(crate) log: &'a Path,
}

impl Task<'_> {
    /// `running`, or how the process ended, as result headers say it.
    pub(crate) fn status(&self) -> String {
        self.exit.map_or_else(|| "running".to_owned(), |exit| exit.to_string())
    }
}

struct Proc {
    command: String,
    child: pty::Child,
    log: Arc<Mutex<Log>>,
    /// The file that holds all of the process's output.
    log_path: PathBuf,
    started: Instant,
    /// The offset of the first output the model has not seen.
    cursor: u64,
    last_output: Option<Instant>,
    exit: Option<Exit>,
    /// When the agent interrupted or stopped the process; an exit soon after
    /// is not news, but one from a process that survived it is.
    stopped_at: Option<Instant>,
    kill_at: Option<Instant>,
    /// Calls still waiting on the process, which keep it from being forgotten.
    waits: usize,
}

impl Proc {
    fn unread(&self) -> bool {
        lock(&self.log).unread(self.cursor)
    }

    fn task(&self, id: u32) -> Task<'_> {
        Task {
            id,
            command: &self.command,
            exit: self.exit,
            started: self.started,
            log: &self.log_path,
        }
    }

    fn take(&mut self, head: u64, tail: u64) -> Content {
        lock(&self.log).take(&mut self.cursor, head, tail, &self.log_path)
    }
}

/// The file that holds all output of process `id` of the session kept in
/// directory `session`.
pub(crate) fn log_path(session: &Path, id: u32) -> PathBuf {
    session.join(PROCS).join(format!("{id}.log"))
}

/// The processes of one session.
pub(crate) struct Procs {
    /// The session directory, which holds the process logs.
    dir: PathBuf,
    launcher: pty::Launcher,
    next_id: u32,
    procs: BTreeMap<u32, Proc>,
    on: OnEvent,
}

impl Procs {
    /// Manages the processes of the session kept in `dir`, started in `cwd`
    /// with the session's control socket at `socket`. Numbering continues
    /// after logs already there, so a resumed session never overwrites them.
    pub(crate) fn new(
        dir: &Path,
        cwd: PathBuf,
        socket: Option<&Path>,
        on: OnEvent,
    ) -> io::Result<Self> {
        let logs = dir.join(PROCS);
        fs::create_dir_all(&logs)?;
        let next_id = fs::read_dir(&logs)?
            .filter_map(|entry| {
                let name = entry.ok()?.file_name();
                name.to_str()?.strip_suffix(".log")?.parse::<u32>().ok()
            })
            .max()
            .map_or(1, |id| id.saturating_add(1));
        let launcher = pty::Launcher::new(dir, cwd, socket);
        Ok(Self { dir: dir.to_path_buf(), launcher, next_id, procs: BTreeMap::new(), on })
    }

    /// Starts `command`, waiting up to `wait` for it to exit.
    pub(crate) fn run(
        &mut self,
        command: &str,
        wait: Duration,
        now: Instant,
    ) -> Result<Wait, String> {
        let duplicate = self.running().find(|task| task.command == command).map(|task| task.id);
        let id = self.spawn(command, now)?;
        Ok(self.wait(id, now + wait, None, true, duplicate))
    }

    /// Types `input` into process `id`, waiting up to `wait` for its output to
    /// settle.
    pub(crate) fn type_into(
        &mut self,
        id: u32,
        input: &str,
        wait: Duration,
        now: Instant,
    ) -> Result<Wait, String> {
        self.get_mut(id)?
            .child
            .write(pty::keystrokes(input))
            .map_err(|error| format!("cannot type into process {id}: {error}"))?;
        Ok(self.wait(id, now + wait, Some(now), false, None))
    }

    /// Reads process `id`'s new output, waiting up to `wait` for some.
    pub(crate) fn read(&mut self, id: u32, wait: Duration, now: Instant) -> Result<Wait, String> {
        let started = self.get_mut(id)?.started;
        Ok(self.wait(id, now + wait, Some(started), false, None))
    }

    /// Stops process `id` for a call, which waits for it to go.
    pub(crate) fn stop(&mut self, id: u32, now: Instant) -> Result<Wait, String> {
        self.get_mut(id)?;
        self.kill(id, now);
        Ok(self.wait(id, now + KILL_GRACE * 2, None, false, None))
    }

    fn wait(
        &mut self,
        id: u32,
        until: Instant,
        settle: Option<Instant>,
        started: bool,
        duplicate: Option<u32>,
    ) -> Wait {
        if let Some(proc) = self.procs.get_mut(&id) {
            proc.waits += 1;
        }
        Wait { id, until, settle, started, duplicate }
    }

    fn get_mut(&mut self, id: u32) -> Result<&mut Proc, String> {
        if !self.procs.contains_key(&id) {
            return Err(self.unknown(id));
        }
        Ok(self.procs.get_mut(&id).expect("the process is tracked"))
    }

    /// Explains a process id that is not tracked: forgotten, or never started.
    fn unknown(&self, id: u32) -> String {
        let log = log_path(&self.dir, id);
        if id < self.next_id && log.is_file() {
            return format!("process {id} has exited; its output is in {}", log.display());
        }
        let running: Vec<String> = self.running().map(|task| task.id.to_string()).collect();
        if running.is_empty() {
            format!("no process with id {id}, and no process is running")
        } else {
            format!("no process with id {id}; running: {}", running.join(", "))
        }
    }

    fn spawn(&mut self, command: &str, now: Instant) -> Result<u32, String> {
        self.prune();
        let id = self.next_id;
        let next_id = id.checked_add(1).ok_or("process ids exhausted; start a new session")?;
        let path = log_path(&self.dir, id);
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
        // The log now holds the id, so a command that fails to start below
        // leaves the next command a fresh one.
        self.next_id = next_id;
        let log = Arc::new(Mutex::new(Log::new(file)));
        let child = self.launcher.spawn(id, command, &log, &self.on)?;
        let proc = Proc {
            command: command.to_owned(),
            child,
            log,
            log_path: path,
            started: now,
            cursor: 0,
            last_output: None,
            exit: None,
            stopped_at: None,
            kill_at: None,
            waits: 0,
        };
        self.procs.insert(id, proc);
        Ok(id)
    }

    /// Forgets the oldest exited processes no call waits on, preferring those
    /// fully read. Their logs stay on disk.
    fn prune(&mut self) {
        while self.procs.len() >= MAX_ENTRIES {
            let exited =
                self.procs.iter().filter(|(_, proc)| proc.exit.is_some() && proc.waits == 0);
            let Some(id) = exited
                .clone()
                .find(|(_, proc)| !proc.unread())
                .or_else(|| exited.clone().next())
                .map(|(id, _)| *id)
            else {
                break;
            };
            self.procs.remove(&id);
        }
    }

    /// Acknowledges an output event.
    pub(crate) fn on_output(&mut self, id: u32, now: Instant) {
        if let Some(proc) = self.procs.get_mut(&id) {
            proc.child.pending.store(false, Ordering::Release);
            proc.last_output = Some(now);
        }
    }

    /// Records an exit. Returns true when the exit is news to the model: the
    /// agent did not just stop the process itself.
    pub(crate) fn on_exit(&mut self, id: u32, exit: Exit) -> bool {
        let Some(proc) = self.procs.get_mut(&id) else {
            return false;
        };
        if proc.kill_at.take().is_some() {
            // The shell went before children that ignore SIGTERM.
            proc.child.kill_remaining();
        }
        proc.exit = Some(exit);
        proc.child.release();
        lock(&proc.log).exited();
        proc.stopped_at.is_none_or(|at| at.elapsed() >= KILL_GRACE * 2)
    }

    /// Whether `wait` is over at `now`.
    pub(crate) fn ready(&self, wait: &Wait, now: Instant) -> bool {
        let Some(proc) = self.procs.get(&wait.id) else {
            return true;
        };
        proc.exit.is_some()
            || now >= wait.until
            || wait.settle.is_some_and(|since| {
                proc.last_output
                    .is_some_and(|last| last > since && now.duration_since(last) >= QUIET)
                    && proc.unread()
            })
    }

    /// When `wait` is over at the latest, given the output so far.
    pub(crate) fn deadline(&self, wait: &Wait) -> Instant {
        let settle = wait.settle.and_then(|since| {
            let proc = self.procs.get(&wait.id)?;
            let last = proc.last_output?;
            // Settling cannot end the wait until there is unread output.
            (last > since && proc.unread()).then_some(last + QUIET)
        });
        settle.map_or(wait.until, |settle| settle.min(wait.until))
    }

    /// Collects the result of a finished wait.
    pub(crate) fn finish(&mut self, wait: &Wait, now: Instant) -> Report {
        let Some(proc) = self.procs.get_mut(&wait.id) else {
            return Report { output: format!("no process with id {}", wait.id).into(), exit: None };
        };
        proc.waits = proc.waits.saturating_sub(1);
        let output = proc.take(HEAD, TAIL);
        let header = Header::Process {
            id: wait.id,
            exit: proc.exit,
            ran: now.duration_since(proc.started),
            duplicate: wait.duplicate,
            reported: wait.started && proc.exit.is_none(),
        };
        let mut report = Content::from(header.to_string());
        if !output.is_empty() {
            report.push_str("\n");
            report.append(output);
        }
        Report { output: report, exit: proc.exit }
    }

    /// Stops process `id` and its children: SIGTERM now, and SIGKILL once the
    /// grace period passes. Returns false when no such process is running.
    pub(crate) fn kill(&mut self, id: u32, now: Instant) -> bool {
        let Some(proc) = self.procs.get_mut(&id).filter(|proc| proc.exit.is_none()) else {
            return false;
        };
        proc.stopped_at = Some(now);
        proc.child.signal(Signal::TERM);
        proc.kill_at = Some(now + KILL_GRACE);
        true
    }

    /// Interrupts process `id` as Ctrl-C would, as when the user cancels.
    pub(crate) fn interrupt(&mut self, id: u32) {
        if let Some(proc) = self.procs.get_mut(&id).filter(|proc| proc.exit.is_none()) {
            proc.stopped_at = Some(Instant::now());
            proc.child.signal(Signal::INT);
        }
    }

    /// Escalates stops whose grace period has passed.
    pub(crate) fn tick(&mut self, now: Instant) {
        for proc in self.procs.values_mut() {
            if proc.kill_at.is_some_and(|at| at <= now) {
                proc.child.signal(Signal::KILL);
                proc.kill_at = None;
            }
        }
    }

    /// When `tick` must run next, if a stop is waiting to escalate.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.procs.values().filter_map(|proc| proc.kill_at).min()
    }

    /// Process `id`, if it is tracked.
    pub(crate) fn task(&self, id: u32) -> Option<Task<'_>> {
        self.procs.get(&id).map(|proc| proc.task(id))
    }

    /// The output of process `id` the model has not read, with the images
    /// shown in it, bounded for an exit notice.
    pub(crate) fn take_unread(&mut self, id: u32) -> Option<Content> {
        let proc = self.procs.get_mut(&id)?;
        proc.unread().then(|| proc.take(EXIT_HEAD, EXIT_TAIL)).filter(|output| !output.is_empty())
    }

    /// The processes tracked, newest first.
    pub(crate) fn tasks(&self) -> impl Iterator<Item = Task<'_>> {
        self.procs.iter().rev().map(|(&id, proc)| proc.task(id))
    }

    /// The processes still running, oldest first.
    pub(crate) fn running(&self) -> impl Iterator<Item = Task<'_>> {
        self.procs.iter().map(|(&id, proc)| proc.task(id)).filter(|task| task.exit.is_none())
    }

    /// The last non-empty lines process `id` printed, for showing it live
    /// without consuming the model's unread output.
    pub(crate) fn tail(&self, id: u32, lines: usize) -> Vec<String> {
        self.procs.get(&id).map(|proc| lock(&proc.log).tail(lines)).unwrap_or_default()
    }
}

impl Drop for Procs {
    /// Hangs up on running processes, as closing their terminal would.
    fn drop(&mut self) {
        for proc in self.procs.values().filter(|proc| proc.exit.is_none()) {
            proc.child.signal(Signal::HUP);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};

    use super::*;

    fn procs(dir: &Path) -> (Procs, Receiver<Event>) {
        let (sender, receiver) = mpsc::channel();
        let on: OnEvent = Arc::new(move |event| {
            let _ = sender.send(event);
        });
        let procs = Procs::new(dir, dir.to_path_buf(), None, on).expect("process table");
        (procs, receiver)
    }

    /// Drives one wait the way the agent's event loop does.
    fn complete(procs: &mut Procs, events: &Receiver<Event>, wait: Wait) -> Report {
        loop {
            let now = Instant::now();
            procs.tick(now);
            if procs.ready(&wait, now) {
                return procs.finish(&wait, now);
            }
            let deadline = procs
                .next_deadline()
                .map_or(procs.deadline(&wait), |kill| kill.min(procs.deadline(&wait)));
            match events.recv_timeout(deadline.saturating_duration_since(now)) {
                Ok(Event::Output(id)) => procs.on_output(id, Instant::now()),
                Ok(Event::Exit(id, exit)) => {
                    procs.on_exit(id, exit);
                }
                Err(_) => {}
            }
        }
    }

    fn run(procs: &mut Procs, events: &Receiver<Event>, command: &str, wait: f64) -> Report {
        let wait =
            procs.run(command, Duration::from_secs_f64(wait), Instant::now()).expect("starts");
        complete(procs, events, wait)
    }

    /// Waits for the next exit and records it.
    fn next_exit(procs: &mut Procs, events: &Receiver<Event>) -> u32 {
        loop {
            match events.recv_timeout(Duration::from_secs(10)).expect("process events") {
                Event::Output(id) => procs.on_output(id, Instant::now()),
                Event::Exit(id, exit) => {
                    procs.on_exit(id, exit);
                    return id;
                }
            }
        }
    }

    #[test]
    fn exhausted_ids_never_overwrite_a_process_log() {
        let dir = tempfile::tempdir().expect("temp dir");
        let logs = dir.path().join("procs");
        fs::create_dir(&logs).expect("logs");
        let path = logs.join(format!("{}.log", u32::MAX));
        fs::write(&path, "keep this output").expect("old log");
        let (mut procs, _) = procs(dir.path());
        assert!(procs.run("true", Duration::ZERO, Instant::now()).is_err());
        assert_eq!(fs::read_to_string(path).expect("log survives"), "keep this output");
    }

    #[test]
    fn a_command_that_cannot_start_leaves_later_commands_working() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut procs, events) = procs(dir.path());
        procs.launcher.cwd = dir.path().join("deleted");
        let error = procs.run("true", Duration::ZERO, Instant::now()).expect_err("no directory");
        assert!(error.starts_with("cannot start command: "), "{error}");
        procs.launcher.cwd = dir.path().to_path_buf();
        let report = run(&mut procs, &events, "true", 10.0);
        assert!(report.output.text().starts_with("[id 2 · exit 0"), "{}", report.output.text());
    }

    #[test]
    fn blocked_input_is_reported_without_waiting_or_growing_the_queue() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut procs, _) = procs(dir.path());
        let wait = procs.run("sleep 30", Duration::ZERO, Instant::now()).expect("process");
        let proc = procs.get_mut(wait.id).expect("process");
        // More lines than a terminal holds for a process that never reads
        // them: the input thread blocks writing them, so of the writes after,
        // at most one waits in the queue and the rest are refused at once.
        assert_eq!(proc.child.write(b"y\n".repeat(64 * 1024)), Ok(()));
        let refused: Vec<String> =
            (0..3).filter_map(|_| proc.child.write(vec![b'n']).err()).collect();
        let message = "it has not read the previous input yet; read its output before typing more";
        assert!(refused.len() >= 2 && refused.iter().all(|error| error == message), "{refused:?}");
        proc.child.signal(Signal::KILL);
    }

    #[test]
    fn missing_forgotten_and_exited_processes_are_explained() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut procs, events) = procs(dir.path());
        let now = Instant::now();
        assert_eq!(
            procs.read(7, Duration::ZERO, now).err(),
            Some("no process with id 7, and no process is running".into())
        );
        let running = procs.run("sleep 30", Duration::ZERO, now).expect("process");
        assert_eq!(
            procs.read(7, Duration::ZERO, now).err(),
            Some(format!("no process with id 7; running: {}", running.id))
        );
        run(&mut procs, &events, "true", 5.0);
        assert_eq!(
            procs.type_into(running.id + 1, "y", Duration::ZERO, now).err(),
            Some(format!("cannot type into process {}: it has exited", running.id + 1))
        );
        procs.procs.remove(&(running.id + 1));
        let log = log_path(dir.path(), running.id + 1);
        assert_eq!(
            procs.read(running.id + 1, Duration::ZERO, now).err(),
            Some(format!(
                "process {} has exited; its output is in {}",
                running.id + 1,
                log.display()
            ))
        );
        procs.get_mut(running.id).expect("process").child.signal(Signal::KILL);
    }

    #[test]
    fn command_output_and_exit_code_are_reported() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut procs, events) = procs(dir.path());
        let report = run(&mut procs, &events, "echo hello; exit 3", 10.0);
        assert_eq!(report.exit, Some(Exit::Code(3)));
        let (header, output) = Header::parse(report.output.text()).expect("header");
        assert!(
            matches!(header, Header::Process { id: 1, exit: Some(Exit::Code(3)), .. }),
            "{header:?}"
        );
        assert_eq!(output, "\nhello");
        assert_eq!(fs::read_to_string(log_path(dir.path(), 1)).expect("log"), "hello\n");
    }

    #[test]
    fn processes_keep_running_and_accept_input() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut procs, events) = procs(dir.path());
        let report =
            run(&mut procs, &events, "printf 'name? '; read -r name; echo \"hi $name\"", 0.3);
        assert_eq!(report.exit, None);
        assert_eq!(report.output.text().lines().nth(1), Some("name?"));
        let wait =
            procs.type_into(1, "ada", Duration::from_secs(10), Instant::now()).expect("typed");
        let report = complete(&mut procs, &events, wait);
        assert!(report.output.text().ends_with("\nhi ada"), "{}", report.output.text());
        assert_eq!(procs.running().count(), 0);
    }

    #[test]
    fn full_screen_programs_return_the_screen_and_can_be_stopped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut procs, events) = procs(dir.path());
        let report = run(
            &mut procs,
            &events,
            "printf '\\033[?1049h\\033[2J\\033[5;10HTOP SCREEN'; sleep 30",
            1.0,
        );
        assert!(report.output.text().contains("[screen]"), "{}", report.output.text());
        assert!(report.output.text().contains("TOP SCREEN"), "{}", report.output.text());
        let wait = procs.stop(1, Instant::now()).expect("stopping");
        let report = complete(&mut procs, &events, wait);
        assert!(matches!(report.exit, Some(Exit::Signal(_))), "{}", report.output.text());
    }

    #[test]
    fn stops_reach_children_that_outlive_the_shell() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut procs, events) = procs(dir.path());
        let pid_file = dir.path().join("child.pid");
        let command =
            format!("(trap '' TERM HUP; exec sleep 30) & echo $! > {}; wait", pid_file.display());
        run(&mut procs, &events, &command, 0.5);
        let wait = procs.stop(1, Instant::now()).expect("stopping");
        complete(&mut procs, &events, wait);
        let pid = fs::read_to_string(&pid_file).expect("child pid");
        let pid = rustix::process::Pid::from_raw(pid.trim().parse().expect("number")).expect("pid");
        let deadline = Instant::now() + Duration::from_secs(5);
        while rustix::process::test_kill_process(pid).is_ok() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(rustix::process::test_kill_process(pid).is_err(), "the child survived");
    }

    #[test]
    fn processes_a_call_waits_on_are_never_forgotten() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut procs, events) = procs(dir.path());
        for _ in 0..MAX_ENTRIES {
            run(&mut procs, &events, "true", 10.0);
        }
        let read = procs.read(1, Duration::ZERO, Instant::now()).expect("read");
        procs.run("true", Duration::ZERO, Instant::now()).expect("start");
        assert!(procs.finish(&read, Instant::now()).output.text().starts_with("[id 1 "));
    }

    #[test]
    fn many_processes_run_at_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut procs, events) = procs(dir.path());
        let now = Instant::now();
        for n in 0..80 {
            procs.run(&format!("sleep 0.5; echo done-{n}"), Duration::ZERO, now).expect("starts");
        }
        assert_eq!(procs.running().count(), 80);
        let mut exited = 0;
        while exited < 80 {
            next_exit(&mut procs, &events);
            exited += 1;
        }
        let output = fs::read_to_string(log_path(dir.path(), 80)).expect("log");
        assert_eq!(output, "done-79\n");
    }

    #[test]
    fn new_commands_say_when_they_keep_running_or_repeat_a_running_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut procs, events) = procs(dir.path());
        let first = run(&mut procs, &events, "sleep 30", 0.1);
        assert!(
            first.output.text().starts_with("[id 1 · running · ")
                && first.output.text().ends_with(" · you will be told when it exits]"),
            "{}",
            first.output.text()
        );
        let second = run(&mut procs, &events, "sleep 30", 0.1);
        assert!(
            second.output.text().contains(" · process 1 already runs this command · "),
            "{}",
            second.output.text()
        );
        let read = procs.read(1, Duration::ZERO, Instant::now()).expect("read");
        let read = complete(&mut procs, &events, read);
        assert!(read.output.text().ends_with("s]"), "{}", read.output.text());
        let done = run(&mut procs, &events, "true", 10.0);
        assert!(done.output.text().ends_with("s]"), "{}", done.output.text());
        for id in [1, 2] {
            procs.get_mut(id).expect("process").child.signal(Signal::KILL);
        }
    }

    #[test]
    fn exit_notices_take_bounded_unread_output_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut procs, events) = procs(dir.path());
        run(&mut procs, &events, "sleep 0.2; seq 1 5000", 0.0);
        next_exit(&mut procs, &events);
        let output = procs.take_unread(1).expect("unread output");
        let output = output.text();
        assert!(output.ends_with("\n5000"), "{output}");
        assert!(output.contains("omitted; full output in "), "{output}");
        assert!(output.len() < (EXIT_HEAD + EXIT_TAIL) as usize + 200);
        assert_eq!(procs.take_unread(1), None, "unread output is taken once");
    }

    #[test]
    fn reads_of_quiet_processes_wait_for_their_deadline() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut procs, events) = procs(dir.path());
        let report = run(&mut procs, &events, "printf hi; sleep 30", 0.5);
        assert!(report.output.text().ends_with("\nhi"), "{}", report.output.text());
        let now = Instant::now();
        let wait = procs.read(1, Duration::from_secs(5), now).expect("read starts");
        assert!(!procs.ready(&wait, now));
        assert_eq!(procs.deadline(&wait), now + Duration::from_secs(5));
        procs.get_mut(1).expect("process").child.signal(Signal::KILL);
    }

    #[test]
    fn ids_continue_after_existing_logs() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(dir.path().join("procs")).expect("procs dir");
        fs::write(dir.path().join("procs/7.log"), "").expect("old log");
        let (mut procs, events) = procs(dir.path());
        let report = run(&mut procs, &events, "true", 10.0);
        assert!(report.output.text().starts_with("[id 8 "), "{}", report.output.text());
    }

    #[test]
    fn exits_read_back_as_they_are_written() {
        for exit in [Exit::Code(0), Exit::Code(-1), Exit::Signal(9), Exit::Unknown] {
            assert_eq!(Exit::parse(&exit.to_string()), Some(exit));
        }
        assert_eq!(Exit::parse("exit x"), None);
        assert_eq!(serde_json::to_string(&Exit::Code(101)).expect("JSON"), r#"{"code":101}"#);
    }
}
