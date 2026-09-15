//! Menus that choose the provider and how to sign in to it, the model, the
//! reasoning effort, a session to resume, a process to inspect and the MCP
//! servers, and the list of keys.
//!
//! Choices are saved as they are made, so the next launch starts with them.

use std::borrow::Cow;
use std::sync::Arc;
use std::thread;
use std::time::SystemTime;

use super::picker::{Choice, Column, Item, Picker};
use super::text::{Line, Style, fit, wrap_text};
use super::{App, BUSY, Input, short_path};
use crate::agent::{Agent, Notice};
use crate::auth::{Credential, Done, Login};
use crate::models::{self, Effort, Model};
use crate::provider::{Access, Provider};
use crate::{config, control, mcp, store};

/// The effort choice that leaves the effort to the provider.
const DEFAULT_EFFORT: &str = "default";
/// Output lines of the selected process shown under the process list.
const PREVIEW_LINES: usize = 6;
/// Output lines a process shows in the transcript when it is inspected.
const INSPECT_LINES: usize = 20;
/// The keys and what they do, as the key list shows them.
const KEYS: [(&str, &str); 16] = [
    ("enter", "send; while the agent works, send for its next step"),
    ("tab", "while the agent works, send once it is done"),
    ("shift-enter", "add a line; so do alt-enter and ctrl-j"),
    ("↑", "in an empty input, take back a waiting message, then history"),
    ("esc", "stop the agent and return waiting messages; close a menu"),
    ("ctrl-c", "clear the input; press twice to quit"),
    ("ctrl-d", "quit"),
    ("/", "commands and skills"),
    ("@", "mention a file"),
    ("ctrl-v", "attach the image on the clipboard; drop or paste image files too"),
    ("page up / down", "scroll; the wheel scrolls and a click opens what it lands on"),
    ("ctrl-o", "browse: ↑↓ entries, shift-↑↓ your messages, enter opens"),
    ("drag", "select text in the transcript and copy it"),
    ("ctrl-l", "choose a model"),
    ("shift-tab", "next reasoning effort"),
    ("$skill", "load a skill anywhere in a message"),
];

/// A model's id, context window, prices in dollars per million input and
/// output tokens, and whether it takes only text.
const MODEL_COLUMNS: &[Column] = &[
    Column::Label("model"),
    Column::Right("context"),
    Column::Right("in $/M"),
    Column::Right("out $/M"),
    Column::Left(""),
];
/// A session's title, how long ago it was used and where it ran.
const SESSION_COLUMNS: &[Column] = &[Column::Label(""), Column::Right(""), Column::Text];
/// A process's id, command and status.
const TASK_COLUMNS: &[Column] = &[Column::Right(""), Column::Label(""), Column::Left("")];
/// An MCP server's name, transport and the program or address serving it.
const SERVER_COLUMNS: &[Column] = &[Column::Label(""), Column::Left(""), Column::Text];
/// The choice after the MCP servers that adds one.
const ADD_SERVER: &str = "add a server…";
/// Servers as adding one takes them, for the menu to show.
const SERVER_EXAMPLES: [&str; 2] = [
    "playwright npx -y @playwright/mcp@latest",
    "github https://api.githubcopilot.com/mcp/ -H 'Authorization: Bearer ${GITHUB_TOKEN}'",
];

/// What is known of the models a provider lists.
enum Listing {
    Loading,
    Listed(Vec<Model>),
    Failed(String),
}

/// Each provider's models, as far as they are known. A listing is kept for
/// the process; one that failed is fetched again when a menu next shows it.
#[derive(Default)]
pub(super) struct Listings([Option<Listing>; Provider::ALL.len()]);

impl Listings {
    fn get(&self, provider: Provider) -> Option<&Listing> {
        self.0[provider as usize].as_ref()
    }

    fn set(&mut self, provider: Provider, listing: Listing) {
        self.0[provider as usize] = Some(listing);
    }
}

/// A way to authenticate with a provider.
#[derive(Clone, Copy, PartialEq)]
enum Method {
    Saved,
    Browser,
    Key,
}

/// What an open menu asks for.
enum Step {
    Provider,
    Method(Provider, Vec<Method>),
    Browser(Provider, Login),
    Key(Provider),
    /// A model of one of `providers`, whose models show a tab at a time.
    /// With `login`, choosing a model moves on to the effort.
    Model {
        providers: Vec<Provider>,
        tab: usize,
        login: bool,
    },
    Effort,
    /// Session ids, in the order of the picker's items.
    Session(Vec<String>),
    Task(Vec<u32>),
    /// The MCP servers' names in the order of the picker's items, which
    /// end with adding one, and the problems with their settings.
    Servers {
        names: Vec<String>,
        problems: Vec<String>,
    },
    /// A server to add, typed as `agt mcp add` takes it, and why the one
    /// typed last could not be added.
    AddServer(Option<String>),
    Keys,
}

/// The menu Esc goes back to.
pub(super) enum Back {
    /// The providers, while signing in.
    Login,
    Servers,
}

/// A menu: what it asks for, and the list its search filters.
pub(super) struct Menu {
    step: Step,
    pub(super) picker: Picker,
}

impl Menu {
    fn new(step: Step, picker: Picker) -> Self {
        Self { step, picker }
    }

    pub(super) fn title(&self) -> String {
        match &self.step {
            Step::Provider => "Connect a provider".into(),
            Step::Method(provider, _) => format!("Sign in to {}", provider.spec().name),
            Step::Browser(provider, _) => {
                format!("Sign in to {} in the browser", provider.spec().name)
            }
            Step::Key(provider) => format!("{} API key", provider.spec().name),
            Step::Model { providers, .. } => match providers.as_slice() {
                [provider] => format!("Model · {}", provider.spec().name),
                _ => "Model".into(),
            },
            Step::Effort => "Reasoning effort".into(),
            Step::Session(_) => "Resume a session".into(),
            Step::Task(_) => "Processes".into(),
            Step::Servers { .. } => "MCP servers".into(),
            Step::AddServer(_) => "Add an MCP server".into(),
            Step::Keys => "Keys".into(),
        }
    }

    /// The search row's text, with keys hidden.
    pub(super) fn input(&self) -> String {
        match self.step {
            Step::Key(_) => "•".repeat(self.picker.query().chars().count()),
            _ => self.picker.query().to_owned(),
        }
    }

    /// What to type, shown while the search row is empty.
    pub(super) fn hint(&self) -> &'static str {
        match self.step {
            Step::Model { .. } => "type to filter, or type a model id",
            Step::Browser(..) => "or paste the address the browser ends on",
            Step::Key(_) => "paste the key",
            Step::AddServer(_) => "a name, then the command that starts the server or its address",
            _ => "type to filter",
        }
    }

    /// The keys the menu answers to, as its bottom border shows them.
    pub(super) fn keys(&self) -> &'static str {
        match self.step {
            Step::Task(_) => "↑↓ choose · enter inspect · ctrl-x stop · esc close",
            Step::Servers { .. } => "↑↓ choose · enter tools · ctrl-x remove · esc close",
            Step::AddServer(_) => "enter add · esc back",
            Step::Browser(..) | Step::Key(_) => "enter confirm · esc back",
            Step::Method(..) | Step::Model { login: true, .. } => {
                "↑↓ choose · enter select · esc back"
            }
            Step::Model { .. } if self.tabs().is_some() => {
                "tab provider · ↑↓ choose · enter select · esc close"
            }
            Step::Keys => "esc close",
            _ => "↑↓ choose · enter select · esc close",
        }
    }

    /// Whether the menu lists choices, rather than only taking typed input.
    pub(super) fn lists(&self) -> bool {
        !matches!(self.step, Step::Browser(..) | Step::Key(_) | Step::AddServer(_))
    }

    /// The menu Esc goes back to, rather than closing this one.
    pub(super) fn back(&self) -> Option<Back> {
        match self.step {
            Step::Method(..)
            | Step::Browser(..)
            | Step::Key(_)
            | Step::Model { login: true, .. } => Some(Back::Login),
            Step::AddServer(_) => Some(Back::Servers),
            _ => None,
        }
    }

    /// The name of the MCP server selected.
    pub(super) fn server(&self) -> Option<&str> {
        match (&self.step, self.picker.choice()) {
            (Step::Servers { names, .. }, Some(Choice::Item(index))) => {
                names.get(index).map(String::as_str)
            }
            _ => None,
        }
    }

    /// The providers the menu shows a tab for and the tab shown, when it
    /// shows more than one.
    pub(super) fn tabs(&self) -> Option<(&[Provider], usize)> {
        match &self.step {
            Step::Model { providers, tab, .. } if providers.len() > 1 => Some((providers, *tab)),
            _ => None,
        }
    }

    pub(super) fn task_id(&self) -> Option<u32> {
        match (&self.step, self.picker.choice()) {
            (Step::Task(ids), Some(Choice::Item(index))) => ids.get(index).copied(),
            _ => None,
        }
    }

    /// The address a browser sign-in opens, which a click on it copies.
    pub(super) fn url(&self) -> Option<&str> {
        match &self.step {
            Step::Browser(_, login) => Some(&login.url),
            _ => None,
        }
    }

    /// Rows the menu shows under its list, `width` columns wide: the address
    /// of a browser sign-in, the newest output of the selected process, the
    /// problems with the MCP servers' settings, or servers to add as examples.
    pub(super) fn preview(&self, agent: Option<&Agent>, width: usize) -> Vec<Line> {
        let wrapped =
            |text: &str, style| wrap_text(text, style, width, 0).into_iter().map(|row| row.line);
        match &self.step {
            Step::Servers { problems, .. } => {
                problems.iter().flat_map(|problem| wrapped(problem, Style::RED)).collect()
            }
            Step::AddServer(problem) => {
                let examples = SERVER_EXAMPLES.iter().map(|example| format!("such as {example}"));
                let problem = problem.iter().flat_map(|problem| wrapped(problem, Style::RED));
                let examples =
                    examples.flat_map(|example| wrapped(&example, Style::DIM).collect::<Vec<_>>());
                problem.chain(examples).collect()
            }
            Step::Browser(_, login) => {
                let note = "if the browser did not open, click the address to copy it:";
                let rows = wrap_text(note, Style::DIM, width, 0);
                let rows = rows.into_iter().chain(wrap_text(&login.url, Style::CODE, width, 0));
                rows.map(|row| row.line).collect()
            }
            Step::Task(_) => match (self.task_id(), agent) {
                (Some(id), Some(agent)) => agent
                    .task_tail(id, PREVIEW_LINES)
                    .iter()
                    .map(|line| fit(&[(line, Style::DIM)], width))
                    .collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }
}

impl App {
    pub(super) fn open_login(&mut self) {
        let home = &self.config.home;
        let items = Provider::ALL
            .into_iter()
            .map(|provider| {
                let spec = provider.spec();
                let tag = match (&spec.access, config::signed_in(home, provider)) {
                    (Access::Subscription(_), true) => "signed in",
                    (Access::Key { .. }, true) => "key found",
                    (_, false) => "",
                };
                Item::new(spec.name).cell(spec.about).tag(tag)
            })
            .collect();
        let mut picker = Picker::new(items);
        picker.select(self.config.endpoint.provider.spec().name);
        self.menu = Some(Menu::new(Step::Provider, picker));
    }

    /// Opens the models of every provider with a credential, and of the
    /// configured provider, on its tab.
    pub(super) fn open_models(&mut self) {
        let providers = config::usable_providers(&self.config.home, self.config.endpoint.provider);
        self.show_models(providers, false);
    }

    /// Shows tab `tab` of the model menu, keeping what is typed.
    pub(super) fn set_tab(&mut self, tab: usize) {
        let Some(Menu { step: Step::Model { providers, tab: shown, .. }, picker }) = &mut self.menu
        else {
            return;
        };
        let Some(&provider) = providers.get(tab) else { return };
        *shown = tab;
        let current = self.config.model.as_ref().map(|model| model.id.as_str());
        let current = current.filter(|_| provider == self.config.endpoint.provider);
        fill_models(picker, self.listings.get(provider), current);
    }

    /// Shows the model menu's next tab, or with `back` the one before, going
    /// round at the ends.
    pub(super) fn switch_tab(&mut self, back: bool) {
        let Some((providers, tab)) = self.menu.as_ref().and_then(Menu::tabs) else { return };
        let count = providers.len();
        self.set_tab(if back { (tab + count - 1) % count } else { (tab + 1) % count });
    }

    pub(super) fn open_efforts(&mut self) {
        let current = self.config.reasoning.map_or(DEFAULT_EFFORT, Effort::as_str);
        let efforts = models::efforts(self.config.model.as_ref());
        let items = std::iter::once((DEFAULT_EFFORT, "the provider's default"))
            .chain(efforts.iter().map(|effort| (effort.as_str(), "")))
            .map(|(effort, detail)| {
                Item::new(effort).cell(detail).tag(if effort == current { "current" } else { "" })
            })
            .collect();
        let mut picker = Picker::new(items);
        picker.select(current);
        self.menu = Some(Menu::new(Step::Effort, picker));
    }

    pub(super) fn open_sessions(&mut self) {
        let sessions = match store::list(&self.config.home) {
            Ok(sessions) => sessions,
            Err(error) => return self.transcript.error(&format!("cannot list sessions: {error}")),
        };
        let current = self.agent.as_ref().map(Agent::session_id);
        let now = SystemTime::now();
        let (ids, items): (Vec<String>, Vec<Item>) = sessions
            .into_iter()
            .filter(|session| !session.title.is_empty() && current != Some(session.id.as_str()))
            .map(|session| {
                let place = short_path(&session.cwd, self.home.as_deref());
                // A session running in another agt says so, and cannot be resumed.
                let when =
                    if control::live(&session.id) { "running".into() } else { session.ago(now) };
                (session.id, Item::new(session.title).cell(when).cell(place))
            })
            .unzip();
        if ids.is_empty() {
            return self.flash("there are no other sessions to resume");
        }
        let picker = Picker::new(items).columns(SESSION_COLUMNS);
        self.menu = Some(Menu::new(Step::Session(ids), picker));
    }

    pub(super) fn open_tasks(&mut self) {
        let (ids, items) = task_items(self.agent.as_ref());
        if ids.is_empty() {
            return self.flash("no processes in this session");
        }
        self.menu = Some(Menu::new(Step::Task(ids), Picker::new(items).columns(TASK_COLUMNS)));
    }

    /// Opens the MCP servers set up for the working directory, with those
    /// the session runs marked.
    pub(super) fn open_servers(&mut self) {
        let status = match &self.agent {
            Some(agent) => agent.pool().status(),
            None => mcp::Pool::new(self.config.home.clone(), self.cwd.clone(), Vec::new(), None)
                .status(),
        };
        let mut names = Vec::with_capacity(status.servers.len());
        let mut items = Vec::with_capacity(status.servers.len() + 1);
        for server in status.servers {
            let running = if server.running { "running" } else { "" };
            items.push(
                Item::new(&server.name).cell(server.transport).cell(server.target).tag(running),
            );
            names.push(server.name);
        }
        items.push(Item::new(ADD_SERVER));
        let picker = Picker::new(items).columns(SERVER_COLUMNS);
        self.menu = Some(Menu::new(Step::Servers { names, problems: status.problems }, picker));
    }

    /// Runs `/mcp`: without an argument it opens the servers, and `add` and
    /// `remove` change them as `agt mcp` does.
    pub(super) fn mcp_command(&mut self, argument: &str) {
        let (action, rest) = argument
            .split_once(char::is_whitespace)
            .map_or((argument, ""), |(action, rest)| (action, rest.trim()));
        match action {
            "" => self.open_servers(),
            "add" => {
                if let Err(problem) = self.add_server(rest) {
                    self.flash(problem);
                }
            }
            "remove" if !rest.is_empty() => self.remove_server(rest),
            _ => self.flash("/mcp shows the servers; /mcp add <name> <command or address> and /mcp remove <name> change them"),
        }
    }

    /// Adds the server `text` describes, as `agt mcp add` takes it, and tells
    /// the agent about it.
    pub(super) fn add_server(&mut self, text: &str) -> Result<(), String> {
        let words = words(text)?;
        let server = crate::cli::parse_server(&mut lexopt::Parser::from_args(words))
            .map_err(|error| error.to_string())?
            .ok_or("type a name, then the command that starts the server or its address")?;
        mcp::add(&self.config.home, &server)
            .map_err(|error| format!("cannot save the MCP servers: {error}"))?;
        self.flash(format!("added {}", server.name));
        if let Some(agent) = &mut self.agent {
            let mcp::Server { name, transport } = server;
            agent.tell(Notice::ServerAdded { name, target: transport.to_string() });
        }
        Ok(())
    }

    /// Removes server `name` from agt's own settings, and tells the agent.
    pub(super) fn remove_server(&mut self, name: &str) {
        match mcp::remove(&self.config.home, name) {
            Ok(true) => {
                self.flash(format!("removed {name}"));
                if let Some(agent) = &mut self.agent {
                    agent.tell(Notice::ServerRemoved { name: name.to_owned() });
                }
            }
            Ok(false) => {
                self.flash(format!("{name} is set up in the project's .mcp.json; remove it there"))
            }
            Err(error) => self.transcript.error(&format!("cannot save the MCP servers: {error}")),
        }
    }

    /// Lists the tools of server `name` in the transcript, starting it on a
    /// thread if the session has not yet.
    fn list_tools(&mut self, name: &str) {
        let Some(agent) = &self.agent else {
            return self.flash("MCP servers run in a session; choose a model to start one");
        };
        let (pool, sender, name) = (Arc::clone(agent.pool()), self.sender.clone(), name.to_owned());
        self.flash(format!("listing the tools of {name}…"));
        let spawned = thread::Builder::new().name("agt-mcp-tools".into()).spawn(move || {
            let listed = pool.tools(&name, &mcp::NEVER);
            let _ = sender.send(Input::Tools(listed.map(|tools| tools.describe(&name))));
        });
        if let Err(error) = spawned {
            self.transcript.error(&format!("cannot list the tools: {error}"));
        }
    }

    pub(super) fn open_keys(&mut self) {
        let items = KEYS.iter().map(|(keys, action)| Item::new(*keys).cell(*action)).collect();
        self.menu = Some(Menu::new(Step::Keys, Picker::new(items)));
    }

    /// Keeps an open process list current, with the same process selected.
    pub(super) fn refresh_tasks(&mut self) {
        let Some(Menu { step: Step::Task(ids), picker }) = &mut self.menu else {
            return;
        };
        let selected = match picker.choice() {
            Some(Choice::Item(index)) => ids.get(index).copied(),
            _ => None,
        };
        let (new_ids, items) = task_items(self.agent.as_ref());
        picker.set_items(items);
        if let Some(index) = selected.and_then(|id| new_ids.iter().position(|new| *new == id)) {
            picker.select_item(index);
        }
        *ids = new_ids;
    }

    /// Acts on Enter in the open menu.
    pub(super) fn confirm(&mut self) {
        let Some(Menu { step, picker }) = self.menu.take() else {
            return;
        };
        let query = picker.query().trim().to_owned();
        match (step, picker.choice()) {
            (Step::Provider, Some(Choice::Item(index))) => {
                self.provider_chosen(Provider::ALL[index]);
            }
            (Step::Method(provider, methods), Some(Choice::Item(index))) => {
                self.method_chosen(provider, methods[index]);
            }
            (Step::Browser(provider, login), _) if !query.is_empty() => {
                login.paste(&query);
                self.flash("checking the sign-in…");
                let picker = Picker::new(Vec::new());
                self.menu = Some(Menu::new(Step::Browser(provider, login), picker));
            }
            (Step::Key(provider), _) => self.key_entered(provider, query, picker),
            (Step::Model { providers, tab, login }, Some(choice)) => {
                let id = match choice {
                    Choice::Item(index) => picker.label(index).to_owned(),
                    Choice::Typed(id) => id,
                };
                self.use_model(providers[tab], login, &id);
            }
            // The default is the one choice that names no effort.
            (Step::Effort, Some(Choice::Item(index))) => {
                self.set_effort(Effort::parse(picker.label(index)));
            }
            (Step::Session(ids), Some(Choice::Item(index))) => {
                if let Some(id) = ids.get(index) {
                    self.resume_session(id);
                }
            }
            // Inspecting never takes the output the model has not read.
            (Step::Task(ids), Some(Choice::Item(index))) => {
                let inspected = ids.get(index).and_then(|&id| {
                    let agent = self.agent.as_ref()?;
                    let task = agent.tasks().find(|task| task.id == id)?;
                    Some(format!(
                        "Process {id} · {}\n$ {}\nLog: {}\n\n{}",
                        task.status(),
                        task.command,
                        task.log.display(),
                        agent.task_tail(id, INSPECT_LINES).join("\n")
                    ))
                });
                if let Some(inspected) = inspected {
                    self.transcript.notice(&inspected);
                }
            }
            (Step::Servers { names, .. }, Some(Choice::Item(index))) => match names.get(index) {
                Some(name) => self.list_tools(name),
                None => self.menu = Some(Menu::new(Step::AddServer(None), Picker::new(Vec::new()))),
            },
            (Step::AddServer(_), _) => match self.add_server(&query) {
                Ok(()) => self.open_servers(),
                Err(problem) => self.menu = Some(Menu::new(Step::AddServer(Some(problem)), picker)),
            },
            (Step::Keys, _) => {}
            (step, _) => self.menu = Some(Menu::new(step, picker)),
        }
    }

    /// Switches to model `id` on `provider` and saves the choice. With `login`,
    /// the provider's credential may be new, and the effort is chosen next.
    pub(super) fn use_model(&mut self, provider: Provider, login: bool, id: &str) {
        if self.busy() {
            return self.flash(BUSY);
        }
        let listed = match self.listings.get(provider) {
            Some(Listing::Listed(models)) => models.iter().find(|model| model.id == id).cloned(),
            _ => None,
        };
        let model = listed.unwrap_or_else(|| models::find(provider, id));
        if let Err(error) = config::save_model(&self.config.home, provider, &model) {
            self.transcript.error(&format!("cannot save settings: {error}"));
        }
        let switched = login || provider != self.config.endpoint.provider;
        if switched && let Err(error) = self.config.set_provider(provider) {
            return self.transcript.error(&error);
        }
        self.config.model = Some(model);
        if let Some(agent) = &mut self.agent {
            if let Some(settings) = self.config.settings() {
                agent.configure(settings);
            }
            self.flash(format!("using {id} on {}", provider.spec().name));
        } else if !self.login {
            let resume = self.resume.take();
            self.start_session(resume.as_deref());
        }
        // An effort the model does not accept would fail every request.
        if let Some(effort) = self.config.reasoning
            && !models::efforts(self.config.model.as_ref()).contains(&effort)
        {
            self.set_effort(None);
        }
        if login {
            self.open_efforts();
        }
    }

    /// Sets and saves the reasoning effort; `None` leaves it to the provider.
    pub(super) fn set_effort(&mut self, effort: Option<Effort>) {
        if self.busy() {
            return self.flash(BUSY);
        }
        if let Err(error) = config::save_effort(&self.config.home, effort) {
            self.transcript.error(&format!("cannot save settings: {error}"));
        }
        self.config.reasoning = effort;
        if let (Some(agent), Some(settings)) = (&mut self.agent, self.config.settings()) {
            agent.configure(settings);
        }
    }

    /// Sets the effort `name` names, as `/effort name` asks, when the model
    /// takes it.
    pub(super) fn set_effort_named(&mut self, name: &str) {
        if name == DEFAULT_EFFORT {
            return self.set_effort(None);
        }
        let efforts = models::efforts(self.config.model.as_ref());
        match Effort::parse(name).filter(|effort| efforts.contains(effort)) {
            Some(effort) => self.set_effort(Some(effort)),
            None => self.flash(format!("the model takes no effort named {name}")),
        }
    }

    /// Moves to the model's next reasoning effort, and from the last back to
    /// the default.
    pub(super) fn cycle_effort(&mut self) {
        let efforts = models::efforts(self.config.model.as_ref());
        let next = match self.config.reasoning {
            None => efforts.first(),
            Some(current) => efforts
                .iter()
                .position(|effort| *effort == current)
                .and_then(|index| efforts.get(index + 1)),
        }
        .copied();
        if self.busy() {
            return self.flash(BUSY);
        }
        self.set_effort(next);
        self.flash(format!("reasoning effort: {}", next.map_or(DEFAULT_EFFORT, Effort::as_str)));
    }

    /// Records the models `provider` lists, showing them when its tab shows.
    pub(super) fn models_listed(
        &mut self,
        provider: Provider,
        listing: Result<Vec<Model>, String>,
    ) {
        self.listings.set(provider, listing.map_or_else(Listing::Failed, Listing::Listed));
        let shown = match &self.menu {
            Some(Menu { step: Step::Model { providers, tab, .. }, .. }) => {
                (providers.get(*tab) == Some(&provider)).then_some(*tab)
            }
            _ => None,
        };
        if let Some(tab) = shown {
            self.set_tab(tab);
        }
        self.dirty = true;
    }

    /// Finishes a browser sign-in.
    pub(super) fn signed_in(&mut self, result: Result<Credential, String>) {
        let Some(Menu { step: Step::Browser(provider, _), .. }) = &self.menu else {
            return;
        };
        let provider = *provider;
        match result {
            Ok(credential) => {
                // Closing the menu stops listening for the browser.
                self.menu = None;
                match config::save_credential(&self.config.home, provider, &credential) {
                    Ok(()) => self.flash(format!("signed in to {}", provider.spec().name)),
                    Err(error) => {
                        self.transcript.error(&format!("cannot save the sign-in: {error}"));
                    }
                }
                self.show_models(vec![provider], true);
            }
            Err(error) => self.transcript.error(&error),
        }
        self.dirty = true;
    }

    fn provider_chosen(&mut self, provider: Provider) {
        let access = &provider.spec().access;
        let subscription = matches!(access, Access::Subscription(_));
        let mut methods = Vec::new();
        if config::signed_in(&self.config.home, provider) {
            methods.push(Method::Saved);
        }
        let browser = matches!(access, Access::Key { sign_in: Some(_), .. });
        if subscription || browser {
            methods.push(Method::Browser);
        }
        if !subscription {
            methods.push(Method::Key);
        }
        if let [method] = methods.as_slice() {
            return self.method_chosen(provider, *method);
        }
        let items = methods
            .iter()
            .map(|method| {
                let label = match method {
                    Method::Saved if subscription => "Use the saved sign-in",
                    Method::Saved => "Use the key found",
                    Method::Browser => "Sign in with the browser",
                    Method::Key => "Paste an API key",
                };
                Item::new(label)
            })
            .collect();
        let picker = Picker::new(items);
        self.menu = Some(Menu::new(Step::Method(provider, methods), picker));
    }

    fn method_chosen(&mut self, provider: Provider, method: Method) {
        match method {
            Method::Saved => self.show_models(vec![provider], true),
            Method::Key => {
                let picker = Picker::new(Vec::new());
                self.menu = Some(Menu::new(Step::Key(provider), picker));
            }
            Method::Browser => {
                self.sign_in += 1;
                let (sender, tag) = (self.sender.clone(), self.sign_in);
                let done: Done = Arc::new(move |result| {
                    let _ = sender.send(Input::Login(tag, result));
                });
                match Login::start(provider, done) {
                    Ok(login) => {
                        super::open(&login.url);
                        let picker = Picker::new(Vec::new());
                        self.menu = Some(Menu::new(Step::Browser(provider, login), picker));
                    }
                    Err(error) => self.transcript.error(&format!("cannot sign in: {error}")),
                }
            }
        }
    }

    fn key_entered(&mut self, provider: Provider, key: String, picker: Picker) {
        if key.is_empty() {
            self.flash("paste the API key, or press esc to go back");
            self.menu = Some(Menu::new(Step::Key(provider), picker));
            return;
        }
        if let Err(error) =
            config::save_credential(&self.config.home, provider, &Credential::Key(key))
        {
            self.transcript.error(&format!("cannot save the API key: {error}"));
        }
        self.show_models(vec![provider], true);
    }

    /// Opens the models of `providers`, a tab each, on the configured
    /// provider's tab when it has one, and lists those not yet known.
    fn show_models(&mut self, providers: Vec<Provider>, login: bool) {
        for &provider in &providers {
            self.fetch_models(provider);
        }
        let current = self.config.endpoint.provider;
        let tab = providers.iter().position(|provider| *provider == current).unwrap_or(0);
        let picker = Picker::new(Vec::new()).columns(MODEL_COLUMNS).accepting_typed();
        self.menu = Some(Menu::new(Step::Model { providers, tab, login }, picker));
        self.set_tab(tab);
    }

    /// Lists the models of `provider` on a thread, unless they are known or
    /// being listed. Those agt knows without asking are known at once.
    fn fetch_models(&mut self, provider: Provider) {
        if matches!(self.listings.get(provider), Some(Listing::Loading | Listing::Listed(_))) {
            return;
        }
        if let Some(models) = models::known(provider) {
            return self.listings.set(provider, Listing::Listed(models));
        }
        self.listings.set(provider, Listing::Loading);
        let (sender, base_url) = (self.sender.clone(), config::base_url(provider));
        let spawned = thread::Builder::new().name("agt-models".into()).spawn(move || {
            let _ = sender.send(Input::Models(provider, models::list(provider, &base_url)));
        });
        if let Err(error) = spawned {
            self.listings.set(provider, Listing::Failed(error.to_string()));
        }
    }

    fn resume_session(&mut self, id: &str) {
        if self.config.model.is_none() {
            self.resume = Some(id.to_owned());
            return self.open_login();
        }
        self.start_session(Some(id));
    }
}

/// Lists the models `listing` holds in `picker`, keeping its query, with
/// `current`, the model in use, tagged and selected.
fn fill_models(picker: &mut Picker, listing: Option<&Listing>, current: Option<&str>) {
    let models = match listing {
        Some(Listing::Listed(models)) => models.as_slice(),
        _ => &[],
    };
    let items = models.iter().map(|model| {
        let prices = model.pricing.map(|pricing| pricing.base);
        let tag = if current == Some(model.id.as_str()) { "current" } else { "" };
        Item::new(model.id.as_str())
            .cell(models::token_count(model.window))
            .cell(prices.map(|rates| models::price(rates.input)).unwrap_or_default())
            .cell(prices.map(|rates| models::price(rates.output)).unwrap_or_default())
            .cell(if model.images { "" } else { "text only" })
            .tag(tag)
    });
    picker.set_items(items.collect());
    picker.set_empty(match listing {
        None | Some(Listing::Loading) => Cow::Borrowed("loading the models…"),
        Some(Listing::Listed(models)) if models.is_empty() => "no models listed; type an id".into(),
        Some(Listing::Listed(_)) => "nothing matches".into(),
        Some(Listing::Failed(error)) => format!("cannot list the models: {error}").into(),
    });
    if let Some(current) = current {
        picker.select(current);
    }
}

/// The words of `text` as a shell splits them, without expanding anything:
/// quotes keep spaces in a word, and a backslash outside single quotes keeps
/// the character after it as it is.
fn words(text: &str) -> Result<Vec<String>, String> {
    let unclosed = |quote: char| format!("a {quote} quote is not closed");
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => words.extend(word.take()),
            '\'' => {
                let word = word.get_or_insert_default();
                loop {
                    match chars.next().ok_or_else(|| unclosed('\''))? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                let word = word.get_or_insert_default();
                loop {
                    match chars.next().ok_or_else(|| unclosed('"'))? {
                        '"' => break,
                        '\\' => word.extend(chars.next()),
                        c => word.push(c),
                    }
                }
            }
            '\\' => word.get_or_insert_default().extend(chars.next()),
            c => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    Ok(words)
}

/// The session's processes as list items, with their ids in the same order.
fn task_items(agent: Option<&Agent>) -> (Vec<u32>, Vec<Item>) {
    agent
        .into_iter()
        .flat_map(Agent::tasks)
        .map(|task| {
            (task.id, Item::new(task.command).cell(task.id.to_string()).cell(task.status()))
        })
        .unzip()
}

#[cfg(test)]
mod tests {
    use super::super::test_app;
    use super::super::text::plain;
    use super::*;

    /// The rows of the open menu's list, without styles.
    fn listed(app: &mut App) -> Vec<String> {
        let menu = app.menu.as_mut().expect("a menu is open");
        let shown = menu.picker.show(80, 20);
        shown.rows.iter().map(|row| plain(row).trim_end().to_owned()).collect()
    }

    #[test]
    fn model_tabs_list_each_providers_models_and_choosing_one_switches_provider() {
        let (mut app, _dir) = test_app((100, 30));
        // Setup starts no session, so a choice changes only the settings.
        app.login = true;
        app.config.model = Some(models::find(Provider::OpenAi, "gpt-5.6-sol"));
        app.show_models(vec![Provider::OpenAi, Provider::Codex, Provider::Grok], false);
        let rows = listed(&mut app);
        assert!(
            rows.iter().any(|row| row.starts_with(" gpt-5.6-sol") && row.ends_with("current")),
            "{rows:#?}"
        );
        app.switch_tab(false);
        let rows = listed(&mut app);
        assert!(
            rows.iter().any(|row| row.starts_with(" gpt-5.6-sol"))
                && !rows.iter().any(|row| row.ends_with("current")),
            "the same id on another provider is not the model in use: {rows:#?}"
        );
        app.menu.as_mut().expect("menu").picker.set_query("4.6");
        app.switch_tab(false);
        assert_eq!(
            listed(&mut app),
            [" model     context", " grok-4.6     500k", " use \"4.6\""],
            "the query stays as the tab changes"
        );
        app.confirm();
        assert_eq!(app.config.endpoint.provider, Provider::Grok);
        assert_eq!(app.config.model.as_ref().map(|model| model.id.as_str()), Some("grok-4.6"));

        // A listing still on its way is fetched once, and shows when it ends.
        app.listings.set(Provider::OpenRouter, Listing::Loading);
        app.show_models(vec![Provider::Grok, Provider::OpenRouter], false);
        app.switch_tab(true);
        assert_eq!(listed(&mut app), [" loading the models…"]);
        app.models_listed(Provider::OpenRouter, Err("offline".into()));
        assert_eq!(listed(&mut app), [" cannot list the models: offline"]);
        app.models_listed(Provider::Grok, Ok(Vec::new()));
        assert_eq!(listed(&mut app), [" cannot list the models: offline"], "another tab's listing");
    }

    #[test]
    fn mcp_servers_are_added_and_removed_from_their_menu() {
        let (mut app, _dir) = test_app((100, 30));
        app.open_servers();
        assert_eq!(listed(&mut app), [" add a server…"]);
        app.confirm();
        let add = |app: &mut App, text: &str| {
            app.menu.as_mut().expect("the add menu is open").picker.set_query(text);
            app.confirm();
        };
        add(&mut app, "docs https://docs.example/mcp -H Authorization");
        let menu = app.menu.as_ref().expect("the add menu stays open");
        assert_eq!(menu.title(), "Add an MCP server", "a header needs a name and a value");
        add(&mut app, "docs https://docs.example/mcp -H 'Authorization: Bearer t0ken'");
        assert_eq!(
            listed(&mut app),
            [" docs           http  https://docs.example/mcp", " add a server…"]
        );
        app.remove_server("docs");
        app.open_servers();
        assert_eq!(listed(&mut app), [" add a server…"]);
    }

    #[test]
    fn typed_words_split_as_a_shell_splits_them() {
        assert_eq!(
            words(r#"github https://x.dev -H 'Authorization: Bearer ${TOKEN}' "a \"b\"" c\ d"#),
            Ok(vec![
                "github".into(),
                "https://x.dev".into(),
                "-H".into(),
                "Authorization: Bearer ${TOKEN}".into(),
                "a \"b\"".into(),
                "c d".into(),
            ])
        );
        assert_eq!(words("  "), Ok(Vec::new()));
        assert_eq!(words("say 'hi"), Err("a ' quote is not closed".into()));
    }
}
