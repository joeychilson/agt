//! A session's agent: one state machine driven by model, process and control
//! events, shared by every frontend.
//!
//! Frontends (the terminal UI, ACP and print mode) pass an agent the events
//! of one channel and render the updates it produces. It never blocks: model
//! requests and processes run on their own threads and report back as events,
//! and waits are deadlines the frontend's loop honors through [`Agent::poll`].
//!
//! The agent's state has parts with one job each: the conversation
//! ([`Context`]), what waits for the model ([`Inbox`]), the session's
//! processes ([`Procs`]), its log ([`Session`]) and the turn in progress.

mod compaction;
mod context;
mod inbox;
mod replay;
mod tools;
mod turn;

use std::borrow::Cow;
use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bash::{self, Outcome, Procs, Task, ToolCall};
use crate::config::{Config, Settings};
use crate::control::{self, Socket};
use crate::item::{Content, Item, Kind, input_text};
use crate::prompt::{self, Instructions, Place, Skill};
use crate::store::{self, Record, Session};
use crate::{llm, mcp, models};
use compaction::{Compacting, Refresh};
pub(crate) use context::mask;
use context::{Context, Limits};
use inbox::Inbox;
pub(crate) use inbox::{Notice, is_notices};
pub(crate) use replay::{replay, replay_item};

/// A request that produces nothing for this long is sent again.
const STALL: Duration = Duration::from_secs(600);
/// The least time between live output updates of one call.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// What an agent's threads report, which frontends pass to [`Agent::handle`].
#[derive(Debug)]
pub(crate) enum Event {
    /// A model event, tagged with the request epoch that produced it.
    Llm(u64, llm::Event),
    Proc(bash::Event),
    /// A message sent to the session through its control socket: for the
    /// agent's next step, or with `later` once it is done.
    Send {
        text: String,
        later: bool,
    },
}

/// Delivers events to the frontend's event loop from any thread.
pub(crate) type Notify = Arc<dyn Fn(Event) + Send + Sync>;

/// What frontends render.
#[derive(Debug)]
pub(crate) enum Update {
    Text(String),
    /// Reasoning text, or an empty string when reasoning starts, which is all
    /// that shows of reasoning a model keeps hidden.
    Thinking(String),
    /// Text streamed since the last response ended is void: the attempt that
    /// produced it failed.
    Reset,
    /// A model response is complete.
    ResponseEnd,
    /// A message the model received: the text written and the saved images
    /// attached. It comes when the message enters the history, live and in
    /// replays alike.
    User {
        text: String,
        images: Vec<String>,
        origin: Origin,
    },
    ToolStart(ToolCall),
    ToolProgress {
        call_id: String,
        lines: Vec<String>,
    },
    ToolEnd {
        call_id: String,
        /// The output as the model gets it: text, or parts that show images.
        output: Value,
        outcome: Outcome,
    },
    Notice(String),
    Error(String),
    /// Compaction started.
    Compacting,
    /// Compaction left about `after` of the context's `before` tokens.
    Compacted {
        before: u64,
        after: u64,
    },
    Usage(Usage),
    TurnEnd(Stop),
}

impl Update {
    /// The words an update agt tells is said in, for frontends that show it as
    /// a line and for the log: a notice, or a compaction starting and ending.
    pub(crate) fn notice(&self) -> Option<String> {
        match self {
            Self::Notice(text) => Some(text.clone()),
            Self::Compacting => Some("compacting context…".to_owned()),
            Self::Compacted { before, after } => {
                Some(format!("compacted context from about {before} to {after} tokens"))
            }
            _ => None,
        }
    }
}

/// Where a message came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Origin {
    /// The person using the frontend.
    #[default]
    User,
    /// `agt send`, from another process.
    Send,
}

impl Origin {
    pub(crate) fn is_user(&self) -> bool {
        *self == Self::User
    }
}

/// A message for the model.
#[derive(Debug)]
pub(crate) struct Message {
    /// Its parts: text, and images that are prepared and saved.
    pub(crate) content: Vec<Value>,
    /// The text written, where skills may be invoked.
    pub(crate) typed: String,
    pub(crate) origin: Origin,
}

impl Message {
    /// A message of `text` alone from the user.
    pub(crate) fn text(text: &str) -> Self {
        Self { content: vec![input_text(text)], typed: text.to_owned(), origin: Origin::User }
    }
}

/// Context in use against the budget it is compacted within, and what the
/// session has cost.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Usage {
    pub(crate) used: u64,
    pub(crate) budget: u64,
    /// Dollars spent in the session, when the model's prices are known.
    pub(crate) cost: Option<f64>,
}

impl Usage {
    /// What the session has cost, such as `$0.42`, or `<$0.01` for less than
    /// a cent, once it has cost anything.
    pub(crate) fn spent(&self) -> Option<String> {
        match self.cost {
            Some(cost) if cost >= 0.01 => Some(format!("${cost:.2}")),
            Some(cost) if cost > 0.0 => Some("<$0.01".to_owned()),
            _ => None,
        }
    }
}

/// What a busy agent is doing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Activity {
    /// Waiting for the model to write, while it reasons.
    Thinking,
    Writing,
    /// Running tool calls.
    Working,
    /// Waiting out the delay before a failed request is sent again.
    Retrying,
    Compacting,
}

/// How a turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Stop {
    EndTurn,
    Cancelled,
    /// The reply reached the output token limit.
    MaxTokens,
    /// The model refused, or the provider filtered its reply.
    Refusal,
    Error,
}

/// When a message sent while the agent works reaches the model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Delivery {
    /// At the agent's next step: with the next request, once the running calls
    /// finish or have kept it waiting a few seconds.
    Next,
    /// Once the agent is done, as a turn of its own.
    Later,
}

/// A message waiting for its delivery.
#[derive(Debug)]
pub(crate) struct Queued {
    pub(crate) message: Message,
    pub(crate) delivery: Delivery,
}

/// How a frontend opens a session.
pub(crate) struct Open<'a> {
    /// The working directory, where a resumed session continues too.
    pub(crate) cwd: &'a Path,
    /// The session to continue, if any.
    pub(crate) resume: Option<&'a str>,
    /// Shown the updates of everything a resumed session ever logged.
    pub(crate) replay: Option<&'a mut dyn FnMut(Update)>,
    /// Whether a background process exiting starts a turn: at once while
    /// idle, or when the turn it exited during ends.
    pub(crate) autowake: bool,
    /// MCP servers the frontend's client offers the session, beside those
    /// set up for the working directory.
    pub(crate) servers: &'a [mcp::Server],
}

/// What the turn is doing.
enum Phase {
    Idle,
    Responding,
    Tools(Vec<tools::Call>),
    Compacting(Compacting),
}

/// When pending messages go to the model, which decides how full the context
/// may be before it is compacted first.
#[derive(Clone, Copy, PartialEq)]
enum Moment {
    /// The start of a turn, where compacting loses least, so it starts earlier.
    TurnStart,
    /// Within a turn, after tool results.
    MidTurn,
}

/// A session: its conversation, its processes and the turn in progress.
pub(crate) struct Agent {
    settings: Settings,
    client: llm::Client,
    session: Session,
    place: Place,
    instructions: Instructions,
    /// The tool definitions every request carries.
    tools: Vec<Value>,
    context: Context,
    limits: Limits,
    inbox: Inbox,
    procs: Procs,
    /// The session's MCP servers, which its socket and the terminal UI reach.
    pool: Arc<mcp::Pool>,
    /// The MCP servers the frontend offered, which instructions rebuilt at
    /// compaction list again.
    offered: Vec<mcp::Server>,
    /// The session's control socket, unless it could not be served. It is
    /// served while the agent lives, and dropping it removes it.
    _socket: Option<Socket>,
    phase: Phase,
    /// Tags model events with the request they belong to, so those of a
    /// cancelled or replaced request are ignored.
    epoch: u64,
    /// The request in flight; dropping it cancels the request.
    stream: Option<llm::Stream>,
    /// When the provider last sent anything, which reveals a stalled request,
    /// or, while a retry waits, when its delay ends.
    last_event: Instant,
    /// Whether the current response has streamed any visible text.
    text_started: bool,
    /// Whether requests carry images: the model accepts them, and its
    /// provider has not rejected one.
    vision: bool,
    /// Dollars spent over the whole session.
    cost: f64,
    /// Context size when compaction last failed. It is tried again once the
    /// context has grown by the recent-context budget.
    compaction_failed_at: Option<u64>,
    /// Instructions being rebuilt after a compaction, adopted when ready.
    refresh: Option<mpsc::Receiver<Refresh>>,
    /// Updates for the frontend, taken with `drain`.
    updates: Vec<Update>,
    notify: Notify,
    autowake: bool,
    /// A failed write to the session log. Work the log cannot record would be
    /// lost to a resumed session, so the turn stops before it sends a request
    /// or runs a command.
    log_error: Option<String>,
}

impl Agent {
    /// Opens a session with the settings in `config`, reporting events through
    /// `notify`.
    pub(crate) fn open(config: &Config, open: Open<'_>, notify: Notify) -> io::Result<Self> {
        let settings = config.settings().ok_or_else(|| {
            io::Error::other(
                "no model is configured: choose one in agt, or with --model or AGT_MODEL",
            )
        })?;
        let Open { cwd, resume, replay: show, autowake, servers: offered } = open;
        // Two agents appending to one log would each lose the other's work.
        if let Some(id) = resume.filter(|id| control::live(id)) {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!(
                    "session {id} is running in another agt; agt send {id} '<message>' reaches it"
                ),
            ));
        }
        let (session, restored) = match (resume, show) {
            (Some(id), Some(show)) => {
                let mut each =
                    |record: &Record<'_>| replay(record).into_iter().for_each(&mut *show);
                Session::resume(&config.home, id, cwd, Some(&mut each))?
            }
            (Some(id), None) => Session::resume(&config.home, id, cwd, None)?,
            (None, _) => (Session::create(&config.home, cwd)?, store::Restored::default()),
        };
        let place = Place {
            cwd: cwd.to_path_buf(),
            home: config.home.clone(),
            user_home: std::env::var_os("HOME").map(Into::into),
            session: session.dir.clone(),
            created: session.created,
        };
        let limits = Limits { window: settings.window(), tier: settings.model.tier };
        let (servers, mut warnings) = mcp::servers(&config.home, cwd, offered);
        let instructions = prompt::build(&place, limits.budget(), &servers);
        warnings.splice(0..0, instructions.warnings.iter().cloned());
        let logs = session.dir.join("mcp");
        let pool =
            Arc::new(mcp::Pool::new(config.home.clone(), cwd.into(), offered.to_vec(), Some(logs)));
        let deliver: control::Deliver = {
            let notify = Arc::clone(&notify);
            Arc::new(move |text, later| notify(Event::Send { text, later }))
        };
        let socket = match Socket::serve(&session.id, Arc::clone(&pool), deliver) {
            Ok(socket) => Some(socket),
            Err(error) => {
                warnings.push(format!(
                    "agt send and MCP servers are unavailable in this session: {error}"
                ));
                None
            }
        };
        let procs = {
            let notify = Arc::clone(&notify);
            let socket = socket.as_ref().map(Socket::path);
            Procs::new(
                &session.dir,
                cwd.to_path_buf(),
                socket,
                Arc::new(move |event| notify(Event::Proc(event))),
            )?
        };
        // Reasoning is sent back only to the provider and model that made it.
        let provider = settings.endpoint.provider.spec().id;
        let same_model = restored
            .model
            .as_ref()
            .filter(|last| last.provider == provider && last.model == settings.model.id);
        let reasoning_from = same_model.map_or(restored.history.len(), |last| last.since);
        let new_model = same_model.is_none();
        let vision = settings.model.images;
        let (cost, lost) = (restored.cost, restored.lost.clone());
        let mut agent = Self {
            client: llm::Client::new(&settings.endpoint),
            tools: vec![bash::definition(vision)],
            context: Context::restored(restored, reasoning_from),
            settings,
            session,
            place,
            instructions,
            limits,
            inbox: Inbox::default(),
            procs,
            pool,
            offered: offered.to_vec(),
            _socket: socket,
            phase: Phase::Idle,
            epoch: 0,
            stream: None,
            last_event: Instant::now(),
            text_started: false,
            vision,
            cost,
            compaction_failed_at: None,
            refresh: None,
            updates: Vec::new(),
            notify,
            autowake,
            log_error: None,
        };
        if new_model {
            agent.log_model();
        }
        for warning in warnings {
            agent.notice(warning);
        }
        if !lost.is_empty() {
            agent.tell(Notice::Lost(lost));
        }
        agent.close_dangling_calls();
        Ok(agent)
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session.id
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.session.cwd
    }

    /// The session directory, which holds its log, process logs and images.
    pub(crate) fn dir(&self) -> &Path {
        &self.session.dir
    }

    pub(crate) fn sees_images(&self) -> bool {
        self.vision
    }

    pub(crate) fn settings(&self) -> &Settings {
        &self.settings
    }

    pub(crate) fn skills(&self) -> &[Skill] {
        &self.instructions.skills
    }

    /// The MCP servers, as the instructions list them.
    pub(crate) fn servers(&self) -> &[mcp::Listing] {
        &self.instructions.servers
    }

    /// The session's MCP servers and their connections.
    pub(crate) fn pool(&self) -> &Arc<mcp::Pool> {
        &self.pool
    }

    /// The updates that show the live context again, from its latest
    /// compaction, for a frontend showing a resumed session.
    pub(crate) fn replay_context(&self) -> impl Iterator<Item = Update> + '_ {
        let compacted =
            self.context.items.first().is_some_and(|item| {
                item.kind() == Kind::Compaction || context::is_checkpoint(item)
            });
        compacted
            .then(|| Update::Notice("earlier conversation was compacted".into()))
            .into_iter()
            .chain(self.context.items.iter().flat_map(|item| replay_item(item, Origin::User)))
    }

    pub(crate) fn busy(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    /// What the agent is doing, when busy.
    pub(crate) fn activity(&self) -> Option<Activity> {
        match &self.phase {
            Phase::Idle => None,
            Phase::Responding | Phase::Compacting(_) if self.last_event > Instant::now() => {
                Some(Activity::Retrying)
            }
            Phase::Responding if self.text_started => Some(Activity::Writing),
            Phase::Responding => Some(Activity::Thinking),
            Phase::Tools(_) => Some(Activity::Working),
            Phase::Compacting(_) => Some(Activity::Compacting),
        }
    }

    /// How many processes run in the background: running, and not waited on
    /// by a call the agent is making.
    pub(crate) fn background(&self) -> usize {
        let waited = |id: u32| match &self.phase {
            Phase::Tools(calls) => calls.iter().any(|call| call.waits_on(id)),
            _ => false,
        };
        self.procs.running().filter(|task| !waited(task.id)).count()
    }

    /// The session's processes, newest first.
    pub(crate) fn tasks(&self) -> impl Iterator<Item = Task<'_>> {
        self.procs.tasks()
    }

    /// The last lines process `id` printed.
    pub(crate) fn task_tail(&self, id: u32, lines: usize) -> Vec<String> {
        self.procs.tail(id, lines)
    }

    /// Stops process `id` for the user, and tells the model.
    pub(crate) fn stop_task(&mut self, id: u32) {
        let command = self.procs.task(id).map(|task| label(task.command));
        match command {
            Some(command) if self.procs.kill(id, Instant::now()) => {
                self.tell(Notice::Stopped { id, command });
            }
            _ => self.notice(format!("process {id} is no longer running")),
        }
    }

    /// Tells the model of something that happened, with its next request, and
    /// shows the frontend what it was told. A notice that wakes the agent
    /// starts a turn while it is idle.
    pub(crate) fn tell(&mut self, notice: Notice) {
        self.notice(notice.summary());
        let wake = self.autowake && notice.wakes();
        self.inbox.notify(store::now(), notice, wake);
        if wake && !self.busy() {
            self.advance(Moment::TurnStart);
        }
    }

    /// Messages sent while busy that the model has not received, oldest first.
    pub(crate) fn queued(&self) -> &[Queued] {
        self.inbox.messages()
    }

    /// Takes back the newest waiting message, which is then never sent.
    pub(crate) fn unqueue(&mut self) -> Option<Queued> {
        self.inbox.pop()
    }

    /// The context in use, the budget it is compacted within and the cost.
    pub(crate) fn usage(&self) -> Usage {
        Usage {
            used: self.context_tokens(),
            budget: self.limits.budget(),
            cost: (self.settings.model.pricing.is_some() || self.cost > 0.0).then_some(self.cost),
        }
    }

    /// Updates produced since the last call.
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = Update> + '_ {
        self.updates.drain(..)
    }

    /// Runs whatever is due now and returns when to call again, if anything
    /// will be due.
    pub(crate) fn poll(&mut self) -> Option<Instant> {
        let now = Instant::now();
        if self.deadline().is_some_and(|deadline| deadline <= now) {
            self.tick(now);
        }
        self.deadline()
    }

    /// Sends a message: at once while idle, and otherwise when `delivery`
    /// says.
    pub(crate) fn submit(&mut self, message: Message, delivery: Delivery) {
        self.inbox.push(Queued { message, delivery });
        if self.busy() {
            self.hurry(Instant::now());
        } else {
            // Notices of what happened while idle go before the message.
            self.advance(Moment::TurnStart);
        }
    }

    /// Applies `settings` to later requests. Returns false while the agent is
    /// busy, when nothing changes.
    pub(crate) fn configure(&mut self, settings: Settings) -> bool {
        if self.busy() {
            return false;
        }
        let current = &self.settings;
        let endpoint_changed = settings.endpoint != current.endpoint;
        let limits_changed = settings.endpoint.provider != current.endpoint.provider
            || settings.model != current.model
            || settings.window() != current.window();
        let reasoning_changed = endpoint_changed || settings.model.id != current.model.id;
        if endpoint_changed {
            self.client = llm::Client::new(&settings.endpoint);
        }
        if limits_changed {
            // A chosen window replaces one learned from an overflow, and images
            // go to a new model even if they were rejected before.
            self.limits = Limits { window: settings.window(), tier: settings.model.tier };
            self.vision = settings.model.images;
            self.tools = vec![bash::definition(settings.model.images)];
        }
        self.settings = settings;
        if reasoning_changed {
            // Encrypted reasoning replays only to the account and model that
            // produced it; the log marks where the new span begins.
            self.context.reasoning_from = self.context.items.len();
            self.log_model();
        }
        if limits_changed {
            self.report_usage();
        }
        true
    }

    /// Stops the current turn, interrupting commands it is waiting on, and
    /// returns the messages that were waiting, which are not sent.
    pub(crate) fn cancel(&mut self) -> Vec<Queued> {
        self.stream = None;
        self.epoch += 1;
        let waiting = self.inbox.take_messages();
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Idle => return waiting,
            Phase::Responding | Phase::Compacting(_) => {}
            Phase::Tools(calls) => self.cancel_calls(calls),
        }
        self.end_turn(Stop::Cancelled);
        waiting
    }

    pub(crate) fn handle(&mut self, event: Event) {
        match event {
            Event::Llm(epoch, event) if epoch == self.epoch => self.on_llm(event),
            // Late events of a request that was cancelled or replaced.
            Event::Llm(..) => {}
            Event::Proc(bash::Event::Output(id)) => self.on_output(id),
            Event::Proc(bash::Event::Exit(id, exit)) => self.on_exit(id, exit),
            Event::Send { text, later } => match compact_command(&text) {
                Some(focus) => self.compact(focus),
                None => {
                    let message = Message {
                        content: vec![input_text(text.as_str())],
                        typed: text,
                        origin: Origin::Send,
                    };
                    let delivery = if later { Delivery::Later } else { Delivery::Next };
                    self.submit(message, delivery);
                }
            },
        }
    }

    /// The next instant `tick` must run, if any.
    fn deadline(&self) -> Option<Instant> {
        let mut deadline = self.procs.next_deadline();
        let mut consider = |instant: Instant| {
            deadline = Some(deadline.map_or(instant, |current| current.min(instant)));
        };
        match &self.phase {
            Phase::Idle => {}
            Phase::Responding | Phase::Compacting(_) => consider(self.last_event + STALL),
            Phase::Tools(calls) => {
                for call in calls {
                    call.deadline(&self.procs).into_iter().for_each(&mut consider);
                }
            }
        }
        deadline
    }

    /// Handles expired deadlines.
    fn tick(&mut self, now: Instant) {
        if now.duration_since(self.last_event) >= STALL {
            match self.phase {
                Phase::Responding => {
                    self.notice(
                        "the provider sent nothing for 10 minutes; sending the request again",
                    );
                    self.updates.push(Update::Reset);
                    self.respond(Moment::MidTurn);
                }
                Phase::Compacting(_) => self.request_summary(),
                Phase::Idle | Phase::Tools(_) => {}
            }
        }
        self.poll_calls(now);
    }

    /// Shows the frontend `text`, and logs it.
    fn notice(&mut self, text: impl Into<String>) {
        self.show(Update::Notice(text.into()));
    }

    /// Shows the frontend an update agt tells, and logs its words.
    fn show(&mut self, update: Update) {
        if let Some(text) = update.notice() {
            self.record(&Record::Notice { at: store::now(), text: text.into() });
        }
        self.updates.push(update);
    }

    /// Shows the frontend an error, and logs it.
    fn error(&mut self, text: String) {
        self.record(&Record::Error { at: store::now(), text: text.as_str().into() });
        self.updates.push(Update::Error(text));
    }

    /// Writes `record` to the log, remembering a failure.
    fn record(&mut self, record: &Record<'_>) {
        if let Err(error) = self.session.write(record) {
            self.log_error = Some(format!("cannot write the session log: {error}"));
        }
    }

    fn log_model(&mut self) {
        let provider = self.settings.endpoint.provider.spec().id;
        let model = self.settings.model.id.clone();
        self.record(&Record::Model { provider: provider.into(), model: model.into() });
    }

    /// Adds `item` to the history and the log.
    fn push(&mut self, item: Item, origin: Origin) {
        self.record(&Record::Item { at: store::now(), origin, item: Cow::Borrowed(&item) });
        self.context.push(item);
    }

    /// Answers function calls left without output by an interrupted run, so
    /// the resumed history is valid input and clients see them end.
    fn close_dangling_calls(&mut self) {
        let answered: HashSet<&str> = self
            .context
            .items
            .iter()
            .filter(|item| item.kind() == Kind::Output)
            .filter_map(|item| item.str("call_id"))
            .collect();
        let dangling: Vec<String> = self
            .context
            .items
            .iter()
            .filter(|item| item.kind() == Kind::Call)
            .filter_map(|item| item.str("call_id"))
            .filter(|id| !answered.contains(id))
            .map(str::to_owned)
            .collect();
        for id in dangling {
            let interrupted = "[interrupted: agt stopped before this call finished]";
            let output = ended(&mut self.updates, &id, interrupted.into(), Outcome::Failed);
            self.push(Item::output(&id, output.into_output()), Origin::User);
        }
    }

    fn context_tokens(&self) -> u64 {
        let tools: usize = self.tools.iter().map(context::value_size).sum();
        let fixed = models::tokens(self.instructions.text.len() + tools);
        self.context.tokens(fixed)
    }

    fn report_usage(&mut self) {
        self.updates.push(Update::Usage(self.usage()));
    }

    /// Adds what a response cost to the session's total.
    fn add_cost(&mut self, usage: &llm::Usage) {
        if let Some(cost) = usage.cost.or_else(|| self.settings.model.cost(usage)) {
            self.cost += cost;
            self.record(&Record::Cost { usd: cost });
        }
    }
}

/// Shows frontends that call `call_id` ended, returning its output for the
/// history.
fn ended(updates: &mut Vec<Update>, call_id: &str, output: Content, outcome: Outcome) -> Content {
    let shown = output.clone().into_output();
    updates.push(Update::ToolEnd { call_id: call_id.to_owned(), output: shown, outcome });
    output
}

/// The focus a message asks compaction for when it is `/compact [focus]`, or
/// `None` for any other message.
pub(crate) fn compact_command(text: &str) -> Option<Option<String>> {
    let focus = text.trim().strip_prefix("/compact")?;
    if !focus.is_empty() && !focus.starts_with(char::is_whitespace) {
        return None;
    }
    let focus = focus.trim();
    Some((!focus.is_empty()).then(|| focus.to_owned()))
}

/// The first line of `command`, shortened to name it in notices.
fn label(command: &str) -> String {
    let mut label: String = command.lines().next().unwrap_or_default().chars().take(80).collect();
    if label.len() < command.trim_end().len() {
        label.push('…');
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_commands_name_their_focus() {
        assert_eq!(compact_command("/compact"), Some(None));
        assert_eq!(compact_command(" /compact  the parser "), Some(Some("the parser".into())));
        assert_eq!(compact_command("/compaction"), None);
        assert_eq!(compact_command("please /compact"), None);
    }

    #[test]
    fn labels_are_first_lines_marked_when_cut() {
        assert_eq!(label("cargo test"), "cargo test");
        assert_eq!(label("cat > a <<'EOF'\nbody\nEOF"), "cat > a <<'EOF'…");
        assert_eq!(label(&"x".repeat(90)), format!("{}…", "x".repeat(80)));
    }
}
