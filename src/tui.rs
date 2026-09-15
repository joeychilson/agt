//! The terminal UI, drawn fullscreen on the alternate screen.
//!
//! The screen reads top down: a header with where the session runs and its
//! settings, the transcript, then the input box with a status line under it.
//! Menus open as panels over the transcript, just above the input box, so a
//! draft stays where it is. Frames are drawn at most every 16 ms, and only
//! the cells that changed are written.
//!
//! The look follows the terminal's own 16-color palette, so light and dark
//! themes both work: cyan marks the user and what has focus, green and red
//! how calls ended, yellow what is still going on, and dim text what is
//! secondary.

mod attach;
mod completion;
mod draw;
mod editor;
mod markdown;
mod menu;
mod picker;
mod screen;
mod text;
mod transcript;

use std::ffi::OsStr;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    self as term, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal;
use serde_json::Value;

use crate::agent::{Agent, Delivery, Message, Notify, Open, Origin, Queued, Update, Usage};
use crate::auth::{Auth, Credential};
use crate::config::Config;
use crate::item;
use crate::models::Model;
use crate::provider::Provider;
use completion::Completion;
use draw::Panel;
use editor::{Draft, Editor, Part, Source};
use menu::{Back, Listings, Menu};
use picker::Picker;
use screen::{Screen, Terminal};
use transcript::{Move, Target, Transcript};

const FRAME: Duration = Duration::from_millis(16);
const SPINNER_INTERVAL: Duration = Duration::from_millis(80);
const QUIT_WINDOW: Duration = Duration::from_millis(1500);
/// How long a message stays in the status line.
const FLASH: Duration = Duration::from_secs(3);
/// Rows a notch of the mouse wheel scrolls.
const WHEEL_ROWS: isize = 3;
const BUSY: &str = "wait for the agent to finish, or press esc to stop it";
/// The commands with their names and what they do, in the order the command
/// list shows them. Picking one from the list runs it.
const COMMANDS: [(Command, &str, &str); 10] = [
    (Command::Login, "/login", "connect a provider"),
    (Command::Model, "/model", "choose a model"),
    (Command::Effort, "/effort", "choose the reasoning effort"),
    (Command::Resume, "/resume", "resume an earlier session"),
    (Command::New, "/new", "start a new session"),
    (Command::Tasks, "/tasks", "inspect or stop a background process"),
    (Command::Mcp, "/mcp", "see, add and remove MCP servers"),
    (Command::Compact, "/compact", "summarize the context now, with an optional focus"),
    (Command::Help, "/help", "show the keys"),
    (Command::Quit, "/quit", "exit"),
];

/// A command the input runs when it starts with the command's name.
#[derive(Clone, Copy)]
enum Command {
    Login,
    Model,
    Effort,
    Resume,
    New,
    Tasks,
    Mcp,
    Compact,
    Help,
    Quit,
}

enum Input {
    /// An agent event tagged with the session that produced it.
    Agent(u64, crate::agent::Event),
    Terminal(term::Event),
    /// The models a provider lists, or why they could not be listed.
    Models(Provider, Result<Vec<Model>, String>),
    /// How a browser sign-in ended, tagged with the sign-in it belongs to.
    Login(u64, Result<Credential, String>),
    /// A message whose images were prepared, or why they could not be.
    Prepared {
        session: u64,
        delivery: Delivery,
        typed: String,
        content: Result<Vec<Value>, String>,
    },
    /// The working directory's files, for `@`.
    Files(Vec<String>),
    /// The image on the clipboard, if it holds one.
    Clipboard(Option<Vec<u8>>),
    /// An MCP server's tools as `agt mcp tools` lists them, or why they
    /// could not be listed.
    Tools(Result<String, String>),
    Closed,
}

/// Runs the terminal UI. With `login` it runs only the setup menus, for
/// `agt login`, and succeeds when they end with a model and a credential.
pub(crate) fn run(
    config: Config,
    cwd: &Path,
    resume: Option<&str>,
    prompt: Option<String>,
    login: bool,
) -> io::Result<ExitCode> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "the interactive UI needs a terminal; use -p to run without one",
        ));
    }
    let (sender, events) = mpsc::channel();
    let mut app = App::new(config, cwd, sender.clone(), terminal::size()?, login);
    if app.config.model.is_some() && !login {
        // Started before the terminal changes modes, so a session that cannot
        // be resumed is reported like any other startup error.
        let open = Open { cwd, resume, replay: None, autowake: true, servers: &[] };
        let agent = Agent::open(&app.config, open, app.notify(1))?;
        app.attach(agent, 1, resume.is_some());
    } else {
        app.resume = resume.map(str::to_owned);
    }
    let guard = Terminal::enter()?;
    thread::Builder::new().name("agt-terminal".into()).spawn(move || {
        while let Ok(event) = term::read() {
            if sender.send(Input::Terminal(event)).is_err() {
                return;
            }
        }
        let _ = sender.send(Input::Closed);
    })?;
    if app.agent.is_none() {
        app.open_login();
    }
    if let Some(prompt) = prompt {
        app.send(Draft::text(prompt), Delivery::Next);
    }
    app.event_loop(&events)?;
    drop(guard);
    if let Some(agent) = &app.agent {
        println!("session {0} · resume with: agt -r {0}", agent.session_id());
    }
    if app.login {
        let endpoint = &app.config.endpoint;
        let Some(model) = app.config.model.as_ref().filter(|_| endpoint.auth != Auth::None) else {
            return Ok(ExitCode::FAILURE);
        };
        println!("using {} on {}", model.id, endpoint.provider.spec().name);
    }
    Ok(ExitCode::SUCCESS)
}

/// The session, the transcript, the input and the menus around them.
struct App {
    config: Config,
    cwd: PathBuf,
    home: Option<PathBuf>,
    /// The working directory as the header shows it.
    place: String,
    sender: mpsc::Sender<Input>,
    /// Tags agent events, so a replaced session's late events are ignored.
    session: u64,
    /// `None` until a model is chosen.
    agent: Option<Agent>,
    /// A session to resume once a model is chosen.
    resume: Option<String>,
    /// Messages waiting for a model to be chosen.
    pending: Vec<(Draft, Delivery)>,
    transcript: Transcript,
    screen: Screen,
    editor: Editor,
    menu: Option<Menu>,
    completion: Option<Completion>,
    /// The input text a completion list was dismissed on, which does not
    /// open it again until the text changes.
    dismissed: Option<String>,
    /// The working directory's files, once listed for `@`.
    files: Option<Vec<String>>,
    listing_files: bool,
    reading_clipboard: bool,
    /// Messages whose images are being prepared.
    preparing: usize,
    listings: Listings,
    /// The latest browser sign-in.
    sign_in: u64,
    usage: Usage,
    /// The branch checked out in the working directory's repository.
    branch: Option<String>,
    /// A short message for the status line, and when it was shown.
    flash: Option<(String, Instant)>,
    /// The terminal's columns and rows.
    size: (u16, u16),
    /// The screen rows the transcript took in the last frame.
    transcript_rows: std::ops::Range<usize>,
    panel: Option<Panel>,
    /// The row and columns where the last frame counted the processes running
    /// in the background, which a click lists.
    running: Option<(usize, std::ops::Range<usize>)>,
    /// When the agent last became busy, for the elapsed time of the turn.
    busy_since: Option<Instant>,
    quit_armed: Option<Instant>,
    quit: bool,
    /// Whether only setup runs, exiting when its menus end.
    login: bool,
    started: Instant,
    last_frame: Instant,
    dirty: bool,
}

impl App {
    fn new(
        config: Config,
        cwd: &Path,
        sender: mpsc::Sender<Input>,
        size: (u16, u16),
        login: bool,
    ) -> Self {
        let now = Instant::now();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self {
            config,
            cwd: cwd.to_path_buf(),
            place: short_path(cwd, home.as_deref()),
            home,
            sender,
            session: 0,
            agent: None,
            resume: None,
            pending: Vec::new(),
            transcript: Transcript::new(),
            screen: Screen::new(),
            editor: Editor::default(),
            menu: None,
            completion: None,
            dismissed: None,
            files: None,
            listing_files: false,
            reading_clipboard: false,
            preparing: 0,
            listings: Listings::default(),
            sign_in: 0,
            usage: Usage::default(),
            branch: branch(cwd),
            flash: None,
            size,
            transcript_rows: 0..0,
            panel: None,
            running: None,
            busy_since: None,
            quit_armed: None,
            quit: false,
            login,
            started: now,
            last_frame: now.checked_sub(FRAME).unwrap_or(now),
            dirty: true,
        }
    }

    /// Handles events until the user quits, the terminal closes, or the
    /// setup menus of `agt login` end.
    fn event_loop(&mut self, events: &mpsc::Receiver<Input>) -> io::Result<()> {
        loop {
            let due = self.agent.as_mut().and_then(Agent::poll);
            if let Some(agent) = &mut self.agent {
                for update in agent.drain() {
                    match update {
                        Update::Usage(usage) => self.usage = usage,
                        update => {
                            // A turn may have switched branches.
                            if matches!(update, Update::TurnEnd(_)) {
                                self.branch = branch(&self.cwd);
                            }
                            self.transcript.update(update);
                        }
                    }
                    self.dirty = true;
                }
            }
            if self.quit || (self.login && self.menu.is_none()) {
                return Ok(());
            }
            let now = Instant::now();
            let busy = self.busy() || self.preparing > 0;
            self.busy_since = self.busy().then(|| self.busy_since.unwrap_or(now));
            let flash_ends =
                self.flash.as_ref().map(|(_, at)| *at + FLASH).filter(|end| *end > now);
            let spin = busy && now >= self.last_frame + SPINNER_INTERVAL;
            let flashed = flash_ends.is_none() && self.flash.take().is_some();
            if (self.dirty || spin || flashed) && now >= self.last_frame + FRAME {
                self.draw()?;
            }
            let redraw = if self.dirty {
                Some(self.last_frame + FRAME)
            } else {
                busy.then_some(self.last_frame + SPINNER_INTERVAL)
            };
            let deadline = [due, redraw, flash_ends].into_iter().flatten().min();
            match crate::recv(events, deadline) {
                Ok(Some(Input::Agent(session, event))) => {
                    if let Some(agent) = &mut self.agent
                        && session == self.session
                    {
                        agent.handle(event);
                        // An open process list shows output as it arrives.
                        if self.menu.as_ref().is_some_and(|menu| menu.task_id().is_some()) {
                            self.dirty = true;
                        }
                    }
                }
                Ok(Some(Input::Terminal(event))) => self.terminal(event),
                Ok(Some(Input::Models(provider, listing))) => self.models_listed(provider, listing),
                Ok(Some(Input::Login(tag, result))) => {
                    if tag == self.sign_in {
                        self.signed_in(result);
                    }
                }
                Ok(Some(Input::Prepared { session, delivery, typed, content })) => {
                    self.prepared(session, delivery, &typed, content);
                }
                Ok(Some(Input::Files(files))) => {
                    self.files = Some(files);
                    self.listing_files = false;
                    self.refresh_completion();
                    self.dirty = true;
                }
                Ok(Some(Input::Clipboard(image))) => {
                    self.reading_clipboard = false;
                    match image {
                        Some(bytes) => {
                            self.editor.attach("clipboard image", Source::Clipboard(bytes));
                        }
                        None => self.flash("there is no image on the clipboard"),
                    }
                    self.dirty = true;
                }
                Ok(Some(Input::Tools(tools))) => {
                    match tools {
                        Ok(tools) => self.transcript.notice(tools.trim_end()),
                        Err(error) => self.transcript.error(&error),
                    }
                    self.dirty = true;
                }
                Ok(Some(Input::Closed)) | Err(_) => return Ok(()),
                Ok(None) => {}
            }
        }
    }

    fn busy(&self) -> bool {
        self.agent.as_ref().is_some_and(Agent::busy)
    }

    fn notify(&self, session: u64) -> Notify {
        let sender = self.sender.clone();
        Arc::new(move |event| {
            let _ = sender.send(Input::Agent(session, event));
        })
    }

    /// Shows `text` in the status line for a few seconds.
    fn flash(&mut self, text: impl Into<String>) {
        self.flash = Some((text.into(), Instant::now()));
        self.dirty = true;
    }

    /// Makes `agent` the current session, with a transcript of its own.
    fn attach(&mut self, agent: Agent, session: u64, replay: bool) {
        self.session = session;
        self.transcript.start(agent.cwd(), agent.dir());
        if replay {
            self.transcript.replay(agent.replay_context());
        }
        self.usage = agent.usage();
        self.agent = Some(agent);
    }

    /// Replaces the session with a new one, or with session `resume`.
    fn start_session(&mut self, resume: Option<&str>) {
        if self.busy() {
            return self.flash(BUSY);
        }
        let session = self.session + 1;
        let notify = self.notify(session);
        let open = Open { cwd: &self.cwd, resume, replay: None, autowake: true, servers: &[] };
        match Agent::open(&self.config, open, notify) {
            Ok(agent) => self.attach(agent, session, resume.is_some()),
            Err(error) => self.transcript.error(&format!("cannot start a session: {error}")),
        }
    }

    /// Sends a message, preparing its images first, or holds it until a
    /// model is chosen.
    fn send(&mut self, draft: Draft, delivery: Delivery) {
        self.transcript.follow();
        let Some(agent) = &mut self.agent else {
            self.pending.push((draft, delivery));
            self.flash("choose a model to send your message");
            if self.menu.is_none() {
                self.open_login();
            }
            return;
        };
        if !draft.has_images() {
            let content = draft
                .parts
                .into_iter()
                .filter_map(|part| match part {
                    Part::Text(text) => Some(item::input_text(text)),
                    Part::Image(_) => None,
                })
                .collect();
            let message = Message { content, typed: draft.typed, origin: Origin::User };
            return agent.submit(message, delivery);
        }
        if !agent.sees_images() {
            let model = agent.settings().model.id.clone();
            self.editor.prepend(&draft.typed);
            return self.transcript.error(&format!("{model} does not take images"));
        }
        let (dir, sender, session) = (agent.dir().to_path_buf(), self.sender.clone(), self.session);
        let Draft { typed, parts } = draft;
        self.preparing += 1;
        let spawned = thread::Builder::new().name("agt-attach".into()).spawn(move || {
            let content = attach::prepare(parts, &dir);
            let _ = sender.send(Input::Prepared { session, delivery, typed, content });
        });
        if let Err(error) = spawned {
            self.preparing -= 1;
            self.transcript.error(&format!("cannot prepare the images: {error}"));
        }
    }

    /// Sends a message whose images are ready, or puts it back in the input.
    fn prepared(
        &mut self,
        session: u64,
        delivery: Delivery,
        typed: &str,
        content: Result<Vec<Value>, String>,
    ) {
        self.preparing -= 1;
        self.dirty = true;
        match (content, &mut self.agent) {
            (Ok(content), Some(agent)) if session == self.session => {
                let message = Message { content, typed: typed.to_owned(), origin: Origin::User };
                agent.submit(message, delivery);
            }
            (Ok(_), _) => {
                self.editor.prepend(typed);
                self.transcript.error("the session changed before the images were ready");
            }
            (Err(error), _) => {
                self.editor.prepend(typed);
                self.transcript.error(&error);
            }
        }
    }

    /// Puts messages that were waiting back into the input, before any draft.
    fn take_back(&mut self, waiting: Vec<Queued>) {
        let texts: Vec<String> = waiting.into_iter().map(|queued| queued.message.typed).collect();
        if !texts.is_empty() {
            self.editor.prepend(&texts.join("\n"));
        }
    }

    fn terminal(&mut self, event: term::Event) {
        // Keys, pastes and clicks can change the input, which completion follows.
        let edits = matches!(
            event,
            term::Event::Key(_)
                | term::Event::Paste(_)
                | term::Event::Mouse(MouseEvent { kind: MouseEventKind::Down(_), .. })
        );
        match event {
            term::Event::Key(key) => {
                self.transcript.clear_selection();
                self.key(key);
            }
            term::Event::Paste(text) => self.paste(&text),
            term::Event::Mouse(mouse) => {
                if !self.mouse(mouse) {
                    return;
                }
            }
            term::Event::Resize(width, height) => {
                self.size = (width, height);
                self.screen.invalidate();
            }
            _ => return,
        }
        // Messages written before setup go out once its menus are closed.
        if !self.pending.is_empty() && self.agent.is_some() && self.menu.is_none() {
            for (draft, delivery) in std::mem::take(&mut self.pending) {
                self.send(draft, delivery);
            }
        }
        if edits {
            self.refresh_completion();
        }
        self.dirty = true;
    }

    /// Pasted text goes to the open menu's search, or into the input, where
    /// image files a terminal pastes as paths, as it does dropped files,
    /// become attachments.
    fn paste(&mut self, text: &str) {
        if let Some(menu) = &mut self.menu {
            return menu.picker.paste(text);
        }
        self.transcript.deselect();
        let takes_images = self.agent.as_ref().is_none_or(Agent::sees_images);
        match attach::image_paths(text, &self.cwd, self.home.as_deref()) {
            Some(paths) if takes_images => {
                for path in paths {
                    let name = path.file_name().map(|name| name.to_string_lossy().into_owned());
                    self.editor.attach(name.as_deref().unwrap_or("image"), Source::File(path));
                }
            }
            _ => self.editor.paste(text),
        }
    }

    /// Reads the clipboard's image on a thread, for Ctrl-V.
    fn paste_image(&mut self) {
        if let Some(agent) = &self.agent
            && !agent.sees_images()
        {
            let model = agent.settings().model.id.clone();
            return self.flash(format!("{model} does not take images"));
        }
        if self.reading_clipboard {
            return;
        }
        self.reading_clipboard = true;
        let sender = self.sender.clone();
        let spawned = thread::Builder::new().name("agt-clipboard".into()).spawn(move || {
            let _ = sender.send(Input::Clipboard(attach::clipboard_image()));
        });
        if spawned.is_err() {
            self.reading_clipboard = false;
            self.flash("cannot read the clipboard");
        }
    }

    /// The list a menu or completion shows.
    fn list(&mut self) -> Option<&mut Picker> {
        match (&mut self.menu, &mut self.completion) {
            (Some(menu), _) => Some(&mut menu.picker),
            (None, Some(completion)) => Some(&mut completion.picker),
            (None, None) => None,
        }
    }

    /// Puts `text` on the clipboard through the terminal (OSC 52), saying it
    /// copied `what`.
    fn copy(&mut self, text: &str, what: &str) {
        match self.screen.copy(text) {
            Ok(()) => self.flash(format!("copied {what}")),
            Err(error) => self.flash(format!("cannot copy: {error}")),
        }
    }

    /// Opens `target` in the system's default application. A link's address
    /// shows in the status line, since its text need not say where it goes.
    fn open_target(&mut self, target: Target) {
        match target {
            Target::Image(path) => open(path),
            Target::Link(address) => {
                open(&address);
                self.flash(format!("opened {address}"));
            }
        }
    }

    /// Scrolls with the wheel, selects text with a drag and opens what a
    /// click lands on. Returns false for events that change nothing.
    fn mouse(&mut self, mouse: MouseEvent) -> bool {
        let y = usize::from(mouse.row);
        let panel = self.panel.as_ref().filter(|panel| panel.rows.contains(&y));
        let choice = panel.and_then(|panel| panel.choices.iter().find(|(row, _)| *row == y));
        let choice = choice.map(|(_, position)| *position);
        let on_panel = panel.is_some();
        let on_url = panel.is_some_and(|panel| panel.url.contains(&y));
        let on_running = !on_panel
            && self.running.as_ref().is_some_and(|(row, columns)| {
                *row == y && columns.contains(&usize::from(mouse.column))
            });
        let tab = panel
            .and_then(|panel| panel.tabs.as_ref())
            .filter(|(row, _)| *row == y)
            .and_then(|(_, tabs)| {
                tabs.iter().position(|columns| columns.contains(&usize::from(mouse.column)))
            });
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if on_panel => {
                let delta = if mouse.kind == MouseEventKind::ScrollUp { -1 } else { 1 };
                if let Some(list) = self.list() {
                    list.step(delta);
                }
            }
            MouseEventKind::ScrollUp => self.transcript.scroll(-WHEEL_ROWS),
            MouseEventKind::ScrollDown => self.transcript.scroll(WHEEL_ROWS),
            MouseEventKind::Down(MouseButton::Left) if let Some(tab) = tab => self.set_tab(tab),
            MouseEventKind::Down(MouseButton::Left) if let Some(position) = choice => {
                if let Some(list) = self.list() {
                    list.choose(position);
                }
                if self.menu.is_some() {
                    self.confirm();
                } else {
                    self.accept_completion(true);
                }
            }
            MouseEventKind::Down(MouseButton::Left) if on_url => {
                if let Some(url) = self.menu.as_ref().and_then(Menu::url).map(str::to_owned) {
                    self.copy(&url, "the address");
                }
            }
            MouseEventKind::Down(MouseButton::Left) if on_running => self.open_tasks(),
            MouseEventKind::Down(MouseButton::Left)
                if !on_panel && self.transcript_rows.contains(&y) =>
            {
                self.transcript.press(usize::from(mouse.column), y);
            }
            MouseEventKind::Drag(MouseButton::Left) if self.transcript.is_dragging() => {
                // Dragging past either end of the transcript scrolls it.
                if y < self.transcript_rows.start {
                    self.transcript.scroll(-1);
                } else if y >= self.transcript_rows.end {
                    self.transcript.scroll(1);
                }
                self.transcript.drag(usize::from(mouse.column), y);
            }
            MouseEventKind::Up(MouseButton::Left) if self.transcript.is_dragging() => {
                match self.transcript.release() {
                    Some(text) => self.copy(&text, "the selection"),
                    None if self.transcript_rows.contains(&y) => {
                        if let Some(target) = self.transcript.click(usize::from(mouse.column), y) {
                            self.open_target(target);
                        }
                    }
                    None => {}
                }
            }
            _ => return false,
        }
        true
    }

    fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(menu) = &mut self.menu {
            if ctrl
                && key.code == KeyCode::Char('x')
                && let Some(id) = menu.task_id()
            {
                if let Some(agent) = &mut self.agent {
                    agent.stop_task(id);
                }
                self.open_tasks();
                return;
            }
            if ctrl
                && key.code == KeyCode::Char('x')
                && let Some(name) = menu.server().map(str::to_owned)
            {
                self.remove_server(&name);
                self.open_servers();
                return;
            }
            let tabs = menu.tabs().is_some();
            match key.code {
                KeyCode::Esc => self.close_menu(),
                KeyCode::Char('c') if ctrl => self.menu = None,
                KeyCode::Enter => self.confirm(),
                KeyCode::Tab | KeyCode::Right if tabs => self.switch_tab(false),
                KeyCode::BackTab | KeyCode::Left if tabs => self.switch_tab(true),
                _ => menu.picker.key(key),
            }
            return;
        }
        if self.transcript.is_browsing() {
            return self.browse(key);
        }
        let completing =
            self.completion.as_ref().is_some_and(|completion| !completion.picker.is_empty());
        match key.code {
            KeyCode::PageUp => self.transcript.scroll(-self.transcript.page()),
            KeyCode::PageDown => self.transcript.scroll(self.transcript.page()),
            KeyCode::Char('o') if ctrl => self.transcript.select(Move::Last),
            KeyCode::Char('v') if ctrl => self.paste_image(),
            KeyCode::Char('c') if ctrl => {
                if let Some(agent) = &mut self.agent
                    && agent.busy()
                {
                    let waiting = agent.cancel();
                    self.take_back(waiting);
                } else if !self.editor.text().is_empty() {
                    self.editor.clear();
                } else if self.quit_armed.is_some_and(|at| at.elapsed() < QUIT_WINDOW) {
                    self.quit = true;
                } else {
                    self.quit_armed = Some(Instant::now());
                    self.flash("press ctrl-c again to quit");
                }
            }
            KeyCode::Char('d') if ctrl && self.editor.text().is_empty() => self.quit = true,
            KeyCode::Char('l') if ctrl => self.command(Command::Model, ""),
            KeyCode::Char('?') if !ctrl && self.editor.text().is_empty() => self.open_keys(),
            KeyCode::Esc if self.completion.is_some() => {
                self.dismissed = Some(self.editor.text().to_owned());
                self.completion = None;
            }
            KeyCode::Esc => match &mut self.agent {
                Some(agent) if agent.busy() => {
                    let waiting = agent.cancel();
                    self.take_back(waiting);
                }
                _ => self.editor.clear(),
            },
            KeyCode::BackTab => self.cycle_effort(),
            KeyCode::Tab => {
                if !self.accept_completion(false) && self.busy() {
                    self.submit(Delivery::Later);
                }
            }
            KeyCode::Up | KeyCode::Down if completing => {
                if let Some(completion) = &mut self.completion {
                    completion.picker.key(key);
                }
            }
            KeyCode::Up if self.editor.text().is_empty() => {
                match self.agent.as_mut().and_then(Agent::unqueue) {
                    Some(queued) => self.editor.set(queued.message.typed),
                    None => self.editor.key(key, self.input_width()),
                }
            }
            KeyCode::Enter if key.modifiers.is_empty() => {
                if !self.accept_completion(true) {
                    self.submit(Delivery::Next);
                }
            }
            _ => self.editor.key(key, self.input_width()),
        }
    }

    /// Keys while browsing the transcript. A typed character goes back to
    /// the input.
    fn browse(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Up if shift => self.transcript.select(Move::PreviousMessage),
            KeyCode::Down if shift => self.transcript.select(Move::NextMessage),
            KeyCode::Up => self.transcript.select(Move::Previous),
            KeyCode::Down => self.transcript.select(Move::Next),
            KeyCode::Home => self.transcript.select(Move::First),
            KeyCode::End => self.transcript.select(Move::Last),
            KeyCode::PageUp => self.transcript.scroll(-self.transcript.page()),
            KeyCode::PageDown => self.transcript.scroll(self.transcript.page()),
            KeyCode::Enter | KeyCode::Left | KeyCode::Right => {
                if let Some(target) = self.transcript.open_selected() {
                    self.open_target(target);
                }
            }
            KeyCode::Esc => self.transcript.deselect(),
            KeyCode::Char('o' | 'c') if ctrl => self.transcript.deselect(),
            KeyCode::Char(_) if !ctrl => {
                self.transcript.deselect();
                self.editor.key(key, self.input_width());
            }
            _ => {}
        }
    }

    /// Columns of text in the input box.
    fn input_width(&self) -> usize {
        usize::from(self.size.0).saturating_sub(6).max(1)
    }

    fn submit(&mut self, delivery: Delivery) {
        let draft = self.editor.submit();
        let trimmed = draft.typed.trim();
        if trimmed.is_empty() && !draft.has_images() {
            return;
        }
        if !draft.has_images() {
            let (name, argument) = trimmed
                .split_once(char::is_whitespace)
                .map_or((trimmed, ""), |(name, argument)| (name, argument.trim()));
            if let Some(&(command, ..)) = COMMANDS.iter().find(|(_, known, _)| *known == name) {
                return self.command(command, argument);
            }
        }
        self.send(draft, delivery);
    }

    fn command(&mut self, command: Command, argument: &str) {
        let waits = matches!(
            command,
            Command::Login
                | Command::Model
                | Command::Effort
                | Command::Resume
                | Command::New
                | Command::Compact
        );
        if waits && self.busy() {
            return self.flash(BUSY);
        }
        match command {
            Command::Quit => self.quit = true,
            Command::Help => self.open_keys(),
            Command::Login => self.open_login(),
            Command::Model if argument.is_empty() => self.open_models(),
            Command::Model => self.use_model(self.config.endpoint.provider, false, argument),
            Command::Effort if argument.is_empty() => self.open_efforts(),
            Command::Effort => self.set_effort_named(argument),
            Command::Resume => self.open_sessions(),
            Command::Tasks => self.open_tasks(),
            Command::Mcp => self.mcp_command(argument),
            Command::New if self.config.model.is_none() => {
                self.resume = None;
                self.open_login();
            }
            Command::New => self.start_session(None),
            Command::Compact => match &mut self.agent {
                Some(agent) => {
                    self.transcript.user(format!("/compact {argument}").trim_end());
                    agent.compact((!argument.is_empty()).then(|| argument.to_owned()));
                }
                None => self.flash("there is no conversation to compact"),
            },
        }
    }

    /// Closes the open menu, or goes back to the menu it came from.
    fn close_menu(&mut self) {
        match self.menu.take().and_then(|menu| menu.back()) {
            Some(Back::Login) => return self.open_login(),
            Some(Back::Servers) => return self.open_servers(),
            None => {}
        }
        if self.agent.is_none() && !self.login {
            self.flash("choose a provider and model with /login to start");
        }
    }

    fn draw(&mut self) -> io::Result<()> {
        self.refresh_tasks();
        let now = Instant::now();
        let mut frame = self.screen.frame(usize::from(self.size.0), usize::from(self.size.1));
        self.paint(&mut frame, now);
        self.last_frame = now;
        self.dirty = false;
        self.screen.draw(frame)
    }
}

/// `path` with the home directory `home` written as `~`.
fn short_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The branch checked out in the repository holding `cwd`, or the commit a
/// detached head points at. It reads `HEAD` directly, which is cheap enough
/// for the end of every turn.
fn branch(cwd: &Path) -> Option<String> {
    let dot = cwd.ancestors().map(|dir| dir.join(".git")).find(|path| path.exists())?;
    let git = if dot.is_file() {
        // A worktree's `.git` file names its git directory.
        let pointer = std::fs::read_to_string(&dot).ok()?;
        let target = PathBuf::from(pointer.trim().strip_prefix("gitdir: ")?);
        dot.parent()?.join(target)
    } else {
        dot
    };
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_owned()),
        None => head.get(..7).map(str::to_owned),
    }
}

/// Opens a URL or file in the user's default application.
pub(crate) fn open(target: impl AsRef<OsStr>) {
    let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let child = std::process::Command::new(program)
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        // Reaped on a thread so no zombie is left behind.
        let _ = thread::Builder::new().name("agt-open".into()).spawn(move || child.wait());
    }
}

/// An app with default settings in a temporary home, for the UI's tests.
#[cfg(test)]
fn test_app(size: (u16, u16)) -> (App, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let (sender, _) = mpsc::channel();
    let empty = serde_json::Map::new();
    let config =
        crate::config::resolve(dir.path().into(), &empty, &empty, Default::default(), |_| None)
            .expect("config");
    (App::new(config, dir.path(), sender, size, false), dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_under_home_are_shortened() {
        let home = Path::new("/Users/me");
        assert_eq!(short_path(Path::new("/Users/me"), Some(home)), "~");
        assert_eq!(short_path(Path::new("/Users/me/work/agt"), Some(home)), "~/work/agt");
        assert_eq!(short_path(Path::new("/srv/agt"), Some(home)), "/srv/agt");
        assert_eq!(short_path(Path::new("/Users/me"), None), "/Users/me");
    }

    #[test]
    fn the_branch_is_read_from_head() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("repo/.git")).expect("git dir");
        std::fs::create_dir_all(dir.path().join("repo/src")).expect("src");
        std::fs::write(dir.path().join("repo/.git/HEAD"), "ref: refs/heads/main\n").expect("head");
        assert_eq!(branch(&dir.path().join("repo/src")).as_deref(), Some("main"));
        std::fs::write(dir.path().join("repo/.git/HEAD"), "0123456789abcdef\n").expect("head");
        assert_eq!(branch(&dir.path().join("repo")).as_deref(), Some("0123456"));
        assert_eq!(branch(dir.path()), None);
    }
}
