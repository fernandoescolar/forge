//! A thread: one conversation with an ACP agent, independent of how it is shown. It owns
//! the agent connection (its own `AcpRuntime`), the transcript, the writes waiting for
//! review, permission requests, the agent's terminals, sign-in and the saved history.
//! Views (the Agents panel, the thread tab) render it and call its methods; it tells them
//! about changes with [`ThreadEvent`].

use crate::{
    auth::{AuthAction, AuthMethod, is_auth_error, parse_auth_methods},
    diff::{DiffView, Edit, Hunk, apply_hunks, hunks},
    history::{self, RecordEntry, SessionRecord, SessionSummary},
    mentions::{ActiveContext, Mention, prompt_blocks, resolve_mentions},
    project_fs::{ApprovedEdits, ProjectFs, ReviewDecision, ReviewPolicy, WriteRecord, WriteReview},
    terminals::{TerminalRegistry, ZedTerminals, spawn_interactive},
};
use acp_client::{AcpRuntime, PROTOCOL_VERSION};
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, Focusable as _, WeakEntity, Window};
use gpui_tokio::Tokio;
use ide_api::{AgentEvent, AgentService as _, AgentSpec, IdeEvent, PermissionOutcome, TerminalRequest};
use language::LanguageRegistry;
use markdown::Markdown;
use project::Project;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::Arc,
};
use terminal_view::TerminalView;
use ui::Color;
use workspace::Workspace;

mod auth;
mod changes;
mod checks;
mod connection;
mod events;
mod modes;
mod proposals;
mod saved;
mod tools;

pub(crate) use tools::forge_tool_title;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Disconnected,
    Connecting,
    Ready,
    Busy,
}

/// The extra answer to a `git commit` permission request: make the commit from Forge.
pub(crate) const COMMIT_IN_FORGE: &str = "forge_commit";
/// The same for `git push`.
pub(crate) const PUSH_IN_FORGE: &str = "forge_push";

/// A `git commit` (with its message) or `git push` the agent asked to run.
enum GitRedirect {
    Commit(String),
    Push,
}

pub(crate) struct PermissionOption {
    pub id: String,
    pub name: String,
    pub allow: bool,
    /// ACP option kind: `allow_once`, `allow_always`, `reject_once`, `reject_always`.
    pub kind: String,
}

pub(crate) enum Entry {
    /// The message and labels of the context sent with it.
    User(String, Vec<String>),
    Agent(Entity<Markdown>),
    Thought(Entity<Markdown>),
    /// `command`: the command line of a command whose title is the agent's description of it.
    Tool { id: String, title: String, kind: String, status: String, detail: Option<String>, terminal: Option<String>, diffs: Vec<DiffView>, command: Option<String> },
    Plan(Vec<(String, String)>),
    /// `tool_call_id`: the tool call it asks about; its card shows the question when it is
    /// in the thread (instead of a second card).
    Permission { request_id: String, tool_call_id: Option<String>, title: String, options: Vec<PermissionOption>, resolved: Option<String>, diffs: Vec<DiffView> },
    /// A write waiting for the user's decision (see `ProjectFs`).
    Review { diff: DiffView, hunks: Vec<(Hunk, bool)>, reply: Option<oneshot::Sender<ReviewDecision>>, outcome: Option<&'static str> },
    System(String, Color),
    /// The changed files checked after a turn (`None` while the language servers catch up).
    Check(Option<Vec<crate::verify::FileCheck>>),
    /// A commit the agent proposes (see `commit_proposal`); `reply` answers its tool call.
    Commit { message: String, files: Vec<PathBuf>, origin: crate::commit_proposal::Origin, state: crate::commit_proposal::CommitState, reply: Option<oneshot::Sender<crate::forge_mcp::ToolReply>> },
    /// A push the agent proposes (see `push_proposal`); `reply` answers its tool call.
    Push { plan: crate::push_proposal::PushPlan, origin: crate::commit_proposal::Origin, state: crate::push_proposal::PushState, reply: Option<oneshot::Sender<crate::forge_mcp::ToolReply>> },
    /// A language-server edit the agent asks for (rename, code action, formatting), waiting
    /// for the user when writes are reviewed.
    LspEdit { plan: crate::forge_tools::EditPlan, state: crate::forge_tools::EditState, reply: Option<oneshot::Sender<crate::forge_mcp::ToolReply>> },
    /// A question the agent asks (`ask_user`): its options, a box for another answer, and
    /// the answer once given (`None` while waiting; skipped questions answer "").
    Question { question: String, options: Vec<String>, input: Entity<editor::Editor>, answer: Option<String>, reply: Option<oneshot::Sender<crate::forge_mcp::ToolReply>> },
    /// An agent's call to a tool an extension offers (`forge.agents.registerTool`), waiting
    /// for the user unless the tool only reads.
    ExtensionTool { tool: forge_ui::agent_tools::AgentTool, args: Value, state: crate::forge_tools::EditState, reply: Option<oneshot::Sender<crate::forge_mcp::ToolReply>> },
    /// An agent's call to one of Forge's tools that runs code or reaches the network
    /// (`permissions::forge_tool_asks`), waiting for the user.
    ForgeTool { name: String, args: Value, state: crate::forge_tools::EditState, reply: Option<oneshot::Sender<crate::forge_mcp::ToolReply>> },
    /// A note the agent wants kept in the project's instructions (`remember`): editable
    /// until the user answers; `file` is where it goes.
    Remember { note: Entity<editor::Editor>, file: PathBuf, state: crate::forge_tools::EditState, reply: Option<oneshot::Sender<crate::forge_mcp::ToolReply>> },
    /// Sign-in card: the agent's login methods, run without leaving Forge.
    Auth { methods: Vec<AuthMethod>, terminal: Option<Entity<TerminalView>>, state: AuthState },
}

#[derive(Clone, PartialEq)]
pub(crate) enum AuthState {
    Waiting,
    Running(String),
    Done,
    Failed(String),
}

/// Where the agent is working: the file (and line) of its latest tool call or write.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentLocation {
    pub path: PathBuf,
    /// What the agent does there: the kind of its tool call (`read`, `edit`, `search`…).
    pub kind: Option<String>,
    /// Zero-based.
    pub line: Option<u32>,
}

pub enum ThreadEvent {
    /// Anything shown changed.
    Updated,
    /// A prompt failed for lack of sign-in; views put it back in their input.
    RestoreInput(String),
    /// The list of changed files (or a file's latest content) changed.
    ChangesUpdated,
    /// The agent asked to show the user its changes (`show_changes`), at a file if given.
    ShowChanges(Option<PathBuf>),
}

/// What the agent reported about tokens (ACP `usage_update` and each turn's `usage`;
/// agents that report nothing leave it empty).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    /// Tokens in the context window now, and its size.
    pub context_used: Option<u64>,
    pub context_size: Option<u64>,
    /// Cumulative cost of the session, with its currency.
    pub cost: Option<(f64, String)>,
    /// Summed over the turns of this connection.
    pub input: u64,
    pub output: u64,
    pub cached_read: u64,
    pub thought: u64,
    pub turns: u32,
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        *self == Usage::default()
    }

    /// Share of the context window in use, 0–1.
    pub fn context_ratio(&self) -> Option<f32> {
        let (used, size) = (self.context_used?, self.context_size?);
        (size > 0).then(|| (used as f32 / size as f32).min(1.0))
    }

    /// `usage_update` session notification.
    fn apply_update(&mut self, u: &Value) {
        if let Some(used) = u.get("used").and_then(Value::as_u64) {
            self.context_used = Some(used);
        }
        if let Some(size) = u.get("size").and_then(Value::as_u64) {
            self.context_size = Some(size);
        }
        if let Some(cost) = u.get("cost") {
            let amount = cost.get("amount").and_then(Value::as_f64);
            let currency = cost.get("currency").and_then(Value::as_str).unwrap_or("USD");
            self.cost = amount.map(|a| (a, currency.to_string()));
        }
    }

    /// A prompt response's `usage`: the tokens of that turn.
    fn add_turn(&mut self, usage: &Value) {
        let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
        self.input += n("inputTokens");
        self.output += n("outputTokens");
        self.cached_read += n("cachedReadTokens");
        self.thought += n("thoughtTokens");
        self.turns += 1;
    }
}

/// A file the agent changed since the user last kept or undid its changes.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangedFile {
    pub path: PathBuf,
    /// Before the agent's first change (`None`: the agent created it).
    pub original: Option<String>,
    /// After its latest change.
    pub current: String,
}

impl ChangedFile {
    /// (added, removed) lines against the original.
    pub fn stats(&self) -> (usize, usize) {
        crate::diff::line_stats(self.original.as_deref().unwrap_or_default(), &self.current)
    }
}

/// A message waiting for the end of the turn.
#[derive(Clone)]
pub struct Queued {
    pub text: String,
    pub active: Option<ActiveContext>,
    pub images: Vec<Arc<gpui::Image>>,
}

/// A slash command the agent offers (ACP `available_commands_update`).
#[derive(Clone, Debug, PartialEq)]
pub struct AgentCommand {
    /// Without the slash.
    pub name: String,
    pub description: String,
    /// What to type after it, when it takes input.
    pub hint: Option<String>,
}

/// The files as they were when a message was sent. Recorded as the agent works: the
/// first time it writes a file after the message, the content before that write.
#[derive(Clone, Debug)]
pub struct Checkpoint {
    /// The user's message (index in `entries`).
    pub entry: usize,
    /// Each file written since, as it was before (`None`: it didn't exist).
    pub files: std::collections::BTreeMap<PathBuf, Option<String>>,
    /// The files written during this turn only: as they were before it, and after its
    /// latest write.
    pub turn_files: std::collections::BTreeMap<PathBuf, (Option<String>, String)>,
    /// The turn's files put back as they were before it (from its summary).
    pub undone: std::collections::BTreeSet<PathBuf>,
    pub started: std::time::Instant,
    /// How long the turn took, once it ended.
    pub duration: Option<std::time::Duration>,
}

impl Checkpoint {
    /// (added, removed) lines over the turn's files.
    pub fn turn_stats(&self) -> (usize, usize) {
        self.turn_files.values().map(|(before, after)| crate::diff::line_stats(before.as_deref().unwrap_or_default(), after)).fold((0, 0), |(a, r), (x, y)| (a + x, r + y))
    }
}

/// A setting the agent offers for the session (ACP `configOptions`): Claude's mode,
/// model, reasoning effort, fast mode…
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// `mode`, `model`, `thought_level`… (ACP categories; free text otherwise).
    pub category: Option<String>,
    pub value: ConfigValue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConfigValue {
    /// The current choice and the choices: (value, name, description).
    Select { current: String, choices: Vec<(String, String, Option<String>)> },
    Boolean(bool),
}

impl ConfigOption {
    /// ACP `SessionConfigOption`; grouped choices are flattened.
    fn parse(v: &Value) -> Option<Self> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let value = match v.get("type").and_then(Value::as_str)? {
            "select" => {
                let mut choices = Vec::new();
                for o in v.get("options").and_then(Value::as_array).into_iter().flatten() {
                    let mut push = |o: &Value| {
                        if let (Some(value), Some(name)) = (o.get("value").and_then(Value::as_str), o.get("name").and_then(Value::as_str)) {
                            choices.push((value.to_string(), name.to_string(), o.get("description").and_then(Value::as_str).map(str::to_string)));
                        }
                    };
                    match o.get("options").and_then(Value::as_array) {
                        Some(group) => group.iter().for_each(&mut push),
                        None => push(o),
                    }
                }
                ConfigValue::Select { current: s("currentValue")?, choices }
            }
            "boolean" => ConfigValue::Boolean(v.get("currentValue")?.as_bool()?),
            _ => return None,
        };
        Some(Self { id: s("id")?, name: s("name")?, description: s("description"), category: s("category"), value })
    }

    /// The current choice's name.
    pub fn current_label(&self) -> String {
        match &self.value {
            ConfigValue::Select { current, choices } => choices.iter().find(|(v, _, _)| v == current).map(|(_, n, _)| n.clone()).unwrap_or_else(|| current.clone()),
            ConfigValue::Boolean(on) => if *on { "On".into() } else { "Off".into() },
        }
    }
}

fn parse_config_options(v: Option<&Value>) -> Vec<ConfigOption> {
    v.and_then(Value::as_array).map(|xs| xs.iter().filter_map(ConfigOption::parse).collect()).unwrap_or_default()
}

/// A session mode the agent offers (ACP `modes`): Claude's Manual, Accept edits, Plan…
#[derive(Clone, Debug, PartialEq)]
pub struct SessionMode {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

pub struct Thread {
    runtime: Arc<AcpRuntime>,
    agents: Vec<AgentSpec>,
    selected: usize,
    status: Status,
    /// Agent process + session currently connected.
    connected: Option<(String, String)>,
    agent_name: Option<String>,
    pub(crate) entries: Vec<Entry>,
    pub(crate) usage: Usage,
    /// Files the agent changed, oldest first (kept until the user keeps or undoes them).
    pub(crate) changes: Vec<ChangedFile>,
    /// One per message sent: the files as they were before it (see [`Checkpoint`]).
    pub(crate) checkpoints: Vec<Checkpoint>,
    /// Errors and warnings per file when the current turn started (to tell new ones).
    diagnostics_before: HashMap<PathBuf, (usize, usize)>,
    /// Keeps the latest check current as the language servers report on its files.
    check_watch: Option<gpui::Subscription>,
    /// Files another thread also changed, already warned about.
    conflicts_warned: std::collections::HashSet<PathBuf>,
    /// The agent's session modes and the current one, when it has modes.
    pub(crate) modes: Vec<SessionMode>,
    pub(crate) current_mode: Option<String>,
    /// The session settings the agent offers (model, mode, effort…), when it offers them.
    pub(crate) config_options: Vec<ConfigOption>,
    /// Slash commands the agent offers (`available_commands_update`), without the slash.
    agent_commands: Vec<AgentCommand>,
    /// Hidden session that answers `/usage` (see `plan_usage`), and its answer so far.
    usage_session: Option<String>,
    /// Hidden sessions answering [`Thread::ask_aside`], and their answers so far.
    side_sessions: HashMap<String, String>,
    usage_reply: Option<String>,
    stderr: VecDeque<String>,
    languages: Arc<LanguageRegistry>,
    root: PathBuf,
    workspace: WeakEntity<Workspace>,
    project: WeakEntity<Project>,
    /// Terminals the agent created (shared with the `TerminalHost`) and their embedded views.
    terminals: TerminalRegistry,
    terminal_views: HashMap<String, Entity<TerminalView>>,
    /// Terminals that show the output of commands the agent ran itself (`_meta.terminal_info`),
    /// by their id; their views are in `terminal_views`.
    display_terminals: HashMap<String, Entity<terminal::Terminal>>,
    approved_edits: ApprovedEdits,
    /// What the agent may do without asking (see `permissions`).
    permissions: crate::permissions::SharedPermissions,
    _permissions_subscription: Option<gpui::Subscription>,
    /// Agent accepts embedded resources in prompts (`promptCapabilities.embeddedContext`).
    embedded_context: bool,
    accepts_images: bool,
    /// MCP servers (ACP JSON) passed to every session.
    mcp_servers: Vec<Value>,
    /// This thread's place on Forge's own MCP server (`forge_mcp`), for agents that take
    /// HTTP MCP servers.
    forge_mcp: Option<crate::forge_mcp::Registration>,
    /// Permission requests to run `git commit` or `git push`, and what Forge offers instead.
    git_redirects: HashMap<String, GitRedirect>,
    /// The session got Forge's tools.
    forge_tools: bool,
    /// Already told the user the agent isn't signed in (once per connection).
    warned_auth: bool,
    /// Login methods the connected agent offers (from `initialize`).
    pub(crate) auth_methods: Vec<AuthMethod>,
    /// A prompt that failed for lack of authentication; restored after signing in.
    pub(crate) retry_prompt: Option<String>,
    /// A prompt sent while connecting (`ask`), sent once the agent is ready.
    pub(crate) queued_prompt: Option<String>,
    /// The session already has the user's standing instructions (`rules`).
    rules_sent: bool,
    /// Messages written while the agent works, sent in order as each turn ends.
    pub(crate) queue: Vec<Queued>,
    /// Next connect keeps the transcript and restarts the agent process (after a login).
    restart_keeping_transcript: bool,
    /// The agent said it isn't signed in before `initialize`'s auth methods were processed;
    /// show the card once the connection completes.
    auth_pending: Option<String>,
    /// Where conversations are saved (`<data>/agent-sessions`).
    history_dir: PathBuf,
    /// Session to resume (via `session/load` when supported) on the next connect.
    resume: Option<SessionRecord>,
    /// Ignore replayed updates while `session/load` runs: the saved transcript is shown.
    replaying: bool,
    started_at: u64,
    location: Option<AgentLocation>,
    /// What this thread was built from, for new threads like it.
    config: Option<crate::config::AgentsConfig>,
    /// The git worktree this thread works in, instead of the project's folder.
    worktree: Option<crate::worktree::AgentWorktree>,
    /// Its worktree was removed: there is nowhere left to work.
    ended: bool,
}

impl EventEmitter<ThreadEvent> for Thread {}

impl Thread {
    /// A thread with the configured agents (see `settings`), talking to `agent` (an index
    /// into them) or to the default agent.
    pub fn new(workspace: &Workspace, agent: Option<usize>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_in(workspace, agent, None, window, cx)
    }

    /// [`Thread::new`], working in `worktree` (see `worktree`) when given.
    pub fn new_in(workspace: &Workspace, agent: Option<usize>, worktree: Option<crate::worktree::AgentWorktree>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = crate::settings::global(cx);
        let (config, history_dir) = {
            let settings = settings.read(cx);
            (settings.config().clone(), settings.history_dir().clone())
        };
        let mut this = Self::with_config_in(workspace, Some(config), history_dir, worktree, window, cx);
        if let Some(agent) = agent.filter(|ix| *ix < this.agents.len()) {
            this.selected = agent;
        }
        if this.agents.is_empty() {
            this.system("No agents configured. Add one in Forge › Settings › Agents.", Color::Warning);
        }
        this
    }

    /// Builds a thread from an already-loaded config (embedders and tests use this to stay
    /// off the user's config files).
    pub fn with_config(workspace: &Workspace, config: Option<crate::config::AgentsConfig>, history_dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_config_in(workspace, config, history_dir, None, window, cx)
    }

    /// [`Thread::with_config`], working in `worktree` when given.
    pub fn with_config_in(
        workspace: &Workspace,
        config: Option<crate::config::AgentsConfig>,
        history_dir: PathBuf,
        worktree: Option<crate::worktree::AgentWorktree>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = workspace.project().clone();
        let root = match &worktree {
            Some(worktree) => worktree.path.clone(),
            None => workspace
                .visible_worktrees(cx)
                .next()
                .map(|wt| wt.read(cx).abs_path().to_path_buf())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
        };
        let languages = workspace.app_state().languages.clone();

        let (bus, mut bus_rx) = tokio::sync::broadcast::channel::<IdeEvent>(1024);
        let review_writes = config.as_ref().is_none_or(|c| c.review_writes);
        let approved_edits = ApprovedEdits::default();
        let (review_tx, mut review_rx) = mpsc::unbounded::<WriteReview>();
        // Permissions follow the settings page while the thread is open.
        let permissions = crate::permissions::SharedPermissions::default();
        permissions.set(config.as_ref().map(|c| c.permissions.clone()).unwrap_or_default());
        let permissions_subscription = crate::settings::try_global(cx).map(|settings| {
            let permissions = permissions.clone();
            cx.observe(&settings, move |_, settings, cx| permissions.set(settings.read(cx).config().permissions.clone()))
        });
        let (written_tx, mut written_rx) = mpsc::unbounded::<WriteRecord>();
        let policy = ReviewPolicy { reviews: review_writes.then_some(review_tx), approved: approved_edits.clone(), permissions: permissions.clone(), written: Some(written_tx) };
        let fs = Arc::new(ProjectFs::new(&project, root.clone(), policy, cx));
        let terminals = TerminalRegistry::default();
        let terminal_host = Arc::new(ZedTerminals::new(&project, terminals.clone(), cx));
        let runtime = Arc::new(AcpRuntime::new(bus, fs).with_terminals(terminal_host).with_terminal_auth().with_display_terminals());
        // Closing the window or the thread stops its agents: tasks still waiting on them
        // would otherwise keep them (and what they started) running until Forge quits.
        cx.on_release(|this: &mut Self, _| this.runtime.kill_all()).detach();

        // broadcast (tokio) → unbounded (GPUI): the thread consumes events on the UI thread.
        let (tx, mut rx) = mpsc::unbounded();
        Tokio::spawn(cx, async move {
            loop {
                match bus_rx.recv().await {
                    Ok(ev) => {
                        if tx.unbounded_send(ev).is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => log::warn!("agent thread dropped {n} events"),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .detach();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
            while let Some(ev) = rx.next().await {
                if this.update_in(cx, |this, window, cx| this.handle_event(ev, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            while let Some(record) = written_rx.next().await {
                if this.update(cx, |this, cx| this.record_write(record, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
            while let Some(review) = review_rx.next().await {
                if this.update_in(cx, |this, window, cx| this.add_review(review, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        let forge_mcp = crate::forge_mcp::register(&Tokio::handle(cx), root.clone()).map(|(registration, mut requests)| {
            cx.spawn_in(window, async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
                while let Some(request) = requests.next().await {
                    if this.update_in(cx, |this, window, cx| this.handle_tool_request(request, window, cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
            registration
        });
        let mcp_servers: Vec<Value> = config.as_ref().map(|c| c.mcp_servers.iter().map(|m| m.to_acp()).collect()).unwrap_or_default();
        let agents = config.as_ref().map(|c| c.agents.clone()).unwrap_or_default();
        Self {
            runtime,
            selected: config.as_ref().map(|c| c.default_index()).unwrap_or(0),
            agents,
            status: Status::Disconnected,
            connected: None,
            agent_name: None,
            entries: vec![],
            usage: Usage::default(),
            changes: Vec::new(),
            checkpoints: Vec::new(),
            diagnostics_before: HashMap::new(),
            check_watch: None,
            conflicts_warned: Default::default(),
            modes: Vec::new(),
            current_mode: None,
            config_options: Vec::new(),
            agent_commands: Vec::new(),
            usage_session: None,
            side_sessions: HashMap::new(),
            usage_reply: None,
            stderr: VecDeque::new(),
            languages,
            root,
            workspace: workspace.weak_handle(),
            project: project.downgrade(),
            terminals,
            terminal_views: HashMap::new(),
            display_terminals: HashMap::new(),
            approved_edits,
            permissions,
            _permissions_subscription: permissions_subscription,
            embedded_context: false,
            accepts_images: false,
            mcp_servers,
            forge_mcp,
            git_redirects: HashMap::new(),
            forge_tools: false,
            warned_auth: false,
            auth_methods: vec![],
            retry_prompt: None,
            queued_prompt: None,
            rules_sent: false,
            queue: Vec::new(),
            restart_keeping_transcript: false,
            auth_pending: None,
            history_dir,
            resume: None,
            replaying: false,
            started_at: history::now(),
            location: None,
            config,
            worktree,
            ended: false,
        }
    }

    /// What a new thread like this one is built from (`with_config`'s arguments): the same
    /// agents, MCP servers, review policy and history.
    pub fn setup(&self) -> (Option<crate::config::AgentsConfig>, PathBuf) {
        (self.config.clone(), self.history_dir.clone())
    }

    pub fn status(&self) -> Status {
        self.status
    }

    /// The agent session, while connected or once resumed from history.
    pub fn session_id(&self) -> Option<&str> {
        self.connected.as_ref().map(|(_, s)| s.as_str()).or(self.resume.as_ref().map(|r| r.session_id.as_str()))
    }

    pub fn agents(&self) -> &[AgentSpec] {
        &self.agents
    }

    pub fn selected_agent(&self) -> usize {
        self.selected
    }

    pub fn select_agent(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.agents.len() && self.status == Status::Disconnected {
            self.selected = ix;
            self.changed(cx);
        }
    }

    pub fn agent_name(&self) -> Option<&str> {
        self.agent_name.as_deref()
    }

    /// The agent's display name, or its id before connecting.
    pub fn agent_label(&self) -> String {
        self.agent_name.clone().or_else(|| self.agents.get(self.selected).map(|a| a.id.clone())).unwrap_or_else(|| "the agent".into())
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// The git worktree the thread works in, if it has its own.
    pub fn worktree(&self) -> Option<&crate::worktree::AgentWorktree> {
        self.worktree.as_ref()
    }

    /// Applies the worktree's changes to the project, as uncommitted changes.
    pub(crate) fn apply_worktree(&mut self, cx: &mut Context<Self>) {
        let Some(worktree) = self.worktree.clone() else { return };
        if self.status == Status::Busy {
            self.system("The agent is still working; apply its changes once it is done.", Color::Warning);
            return self.changed(cx);
        }
        let repo = worktree.repo.clone();
        let task = cx.background_spawn(async move { crate::worktree::apply(&worktree) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(crate::worktree::Applied::Clean { files: 0 }) => this.system("The worktree has no changes to apply.", Color::Muted),
                    Ok(crate::worktree::Applied::Clean { files }) => this.system(
                        format!("Applied the changes to {files} file{} in the project. They are uncommitted: review them in the Git panel.", if files == 1 { "" } else { "s" }),
                        Color::Success,
                    ),
                    Ok(crate::worktree::Applied::Conflicts(files)) => {
                        this.system(format!("Applied, with conflicts in {}: you and the agent changed the same lines. Resolve them in the merge editor (Git › Open Merge Editor…).", files.join(", ")), Color::Warning);
                        if let (Some(workspace), Some(first)) = (this.workspace.upgrade(), files.first()) {
                            let path = repo.join(first).to_string_lossy().into_owned();
                            let toast = workspace::Toast::new(workspace::notifications::NotificationId::named("forge-worktree-conflicts".into()), format!("Conflicts in {}", files.join(", ")))
                                .on_click("Open Merge Editor", move |window, cx| window.dispatch_action(Box::new(forge_ui::OpenMergeEditor { path: Some(path.clone()) }), cx));
                            workspace.update(cx, |ws, cx| ws.show_toast(toast, cx));
                        }
                    }
                    Err(e) => this.system(format!("{e:#}"), Color::Error),
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Ends the thread's work in its worktree: stops the agent and removes the worktree
    /// (and its branch unless `keep_branch`).
    pub(crate) fn remove_worktree(&mut self, keep_branch: bool, cx: &mut Context<Self>) {
        let Some(worktree) = self.worktree.clone() else { return };
        self.disconnect(cx);
        let task = cx.background_spawn(async move { crate::worktree::remove(&worktree, keep_branch).map(|_| worktree) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(worktree) => {
                        this.worktree = None;
                        this.ended = true;
                        let kept = if keep_branch { format!("; its branch {} is still there", worktree.branch()) } else { String::new() };
                        this.system(format!("Removed the worktree{kept}. This thread is over."), Color::Muted);
                    }
                    Err(e) => this.system(format!("Could not remove the worktree: {e:#}"), Color::Error),
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    pub fn languages(&self) -> &Arc<LanguageRegistry> {
        &self.languages
    }

    pub fn workspace(&self) -> &WeakEntity<Workspace> {
        &self.workspace
    }

    /// The first thing the user asked, as a title; `None` before that.
    pub fn title(&self) -> Option<String> {
        self.entries.iter().find_map(|e| match e {
            Entry::User(text, _) => Some(text.lines().next().unwrap_or_default().chars().take(60).collect()),
            _ => None,
        })
    }

    /// Writes waiting for the user's decision.
    pub fn pending_reviews(&self) -> usize {
        self.entries.iter().filter(|e| matches!(e, Entry::Review { reply: Some(_), .. } | Entry::Permission { resolved: None, .. } | Entry::Commit { reply: Some(_), .. } | Entry::Push { reply: Some(_), .. } | Entry::LspEdit { reply: Some(_), .. } | Entry::ExtensionTool { reply: Some(_), .. } | Entry::ForgeTool { reply: Some(_), .. } | Entry::Remember { reply: Some(_), .. } | Entry::Question { answer: None, .. })).count()
    }

    /// Where the agent last read or wrote.
    pub fn location(&self) -> Option<&AgentLocation> {
        self.location.as_ref()
    }

    /// The pending write to `path`, if any: its entry index and (+, −) counts.
    pub fn pending_review_for(&self, path: &std::path::Path) -> Option<(usize, usize, usize)> {
        self.entries.iter().enumerate().find_map(|(ix, e)| match e {
            Entry::Review { diff, reply: Some(_), .. } if std::path::Path::new(&diff.edit.path) == path => {
                let (added, removed) = diff.edit.stats();
                Some((ix, added, removed))
            }
            _ => None,
        })
    }

    /// One line about where the thread stands, for lists.
    pub fn summary(&self) -> (String, Color) {
        let reviews = self.pending_reviews();
        match self.status {
            _ if reviews > 0 => (format!("{reviews} to review"), Color::Accent),
            Status::Busy => ("Working".into(), Color::Info),
            Status::Connecting => ("Connecting…".into(), Color::Info),
            _ if self.entries.iter().any(|e| matches!(e, Entry::Agent(_))) => ("Answered".into(), Color::Muted),
            _ => ("New".into(), Color::Muted),
        }
    }

    /// Files the agent proposed or made changes to, with their latest +/− counts.
    pub fn touched_files(&self) -> Vec<(PathBuf, usize, usize)> {
        let mut files: Vec<(PathBuf, usize, usize)> = Vec::new();
        let diffs = self.entries.iter().flat_map(|e| match e {
            Entry::Tool { diffs, .. } | Entry::Permission { diffs, .. } => diffs.iter().collect::<Vec<_>>(),
            Entry::Review { diff, .. } => vec![diff],
            _ => vec![],
        });
        for diff in diffs {
            let path = PathBuf::from(&diff.edit.path);
            let (added, removed) = diff.edit.stats();
            match files.iter_mut().find(|(p, _, _)| *p == path) {
                Some(file) => *file = (path, added, removed),
                None => files.push((path, added, removed)),
            }
        }
        files
    }

    /// A terminal that only shows output, embedded like the agent's own terminals.
    fn add_display_terminal(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project.upgrade() else { return };
        if self.display_terminals.contains_key(id) {
            return;
        }
        let path_style = project.read(cx).path_style(cx);
        let builder = terminal::TerminalBuilder::new_display_only(Default::default(), terminal::terminal_settings::AlternateScroll::On, None, 0, cx.background_executor(), path_style);
        let terminal = cx.new(|cx| builder.subscribe(cx));
        let (workspace, project) = (self.workspace.clone(), self.project.clone());
        let view = cx.new(|cx| {
            let mut view = TerminalView::new(terminal.clone(), workspace, None, project, window, cx);
            view.set_embedded_mode(Some(15), cx);
            view
        });
        self.display_terminals.insert(id.to_string(), terminal);
        self.terminal_views.insert(id.to_string(), view);
    }

    pub(crate) fn terminal_view(&self, id: &str) -> Option<Entity<TerminalView>> {
        self.terminal_views.get(id).cloned()
    }

    pub(crate) fn changed(&self, cx: &mut Context<Self>) {
        cx.emit(ThreadEvent::Updated);
        cx.notify();
    }

    pub(crate) fn system(&mut self, text: impl Into<String>, color: Color) {
        self.entries.push(Entry::System(text.into(), color));
    }

    /// Sends `prompt`, connecting the selected agent first if needed.
    pub fn ask(&mut self, prompt: String, window: &mut Window, cx: &mut Context<Self>) {
        match self.status {
            Status::Ready => {
                self.send(prompt, None, cx);
            }
            Status::Disconnected if !self.agents.is_empty() => {
                self.queued_prompt = Some(prompt);
                self.connect(window, cx);
            }
            Status::Disconnected => self.system("Configure an agent in agents.json to ask it.", Color::Warning),
            Status::Connecting => self.queued_prompt = Some(prompt),
            Status::Busy => self.queue.push(Queued { text: prompt, active: None, images: vec![] }),
        }
        self.changed(cx);
    }

    /// The slash commands: the user's prompts (`library`), then the ones the agent offers
    /// (a prompt hides an agent command of the same name).
    pub fn commands(&self) -> Vec<AgentCommand> {
        let prompts = crate::library::prompts(&self.root);
        let mut commands: Vec<AgentCommand> = prompts
            .iter()
            .map(|p| AgentCommand {
                name: p.name.clone(),
                description: if p.description.is_empty() { "Your prompt".into() } else { format!("{} (your prompt)", p.description) },
                hint: p.body.contains("$ARGUMENTS").then(|| "…".to_string()),
            })
            .collect();
        commands.extend(self.agent_commands.iter().filter(|c| !prompts.iter().any(|p| p.name == c.name)).cloned());
        commands
    }

    /// Another thread has unreviewed changes to `path`, which this one just wrote: both
    /// threads say so, once per file.
    fn warn_about_conflict(&mut self, path: &PathBuf, cx: &mut Context<Self>) {
        let me = cx.entity();
        let Some(other) = crate::agent_review::other_thread_changing(path, &me, cx) else { return };
        if !self.conflicts_warned.insert(path.clone()) {
            return;
        }
        let name = path.strip_prefix(&self.root).unwrap_or(path).to_string_lossy().into_owned();
        let other_title = other.read(cx).title().unwrap_or_else(|| "another thread".into());
        let my_title = self.title().unwrap_or_else(|| "another thread".into());
        self.system(
            format!("⚠ {name} also has changes from “{other_title}” you haven't reviewed. This thread's changes are on top of them: undoing them here puts the file back as it was before this thread."),
            Color::Warning,
        );
        let path = path.clone();
        other.update(cx, |other, cx| {
            if other.conflicts_warned.insert(path) {
                other.system(format!("⚠ “{my_title}” changed {name} too, on top of this thread's changes. Undoing them here also undoes that thread's."), Color::Warning);
                other.changed(cx);
            }
        });
    }

    /// A checkpoint for the message just added.
    pub(crate) fn begin_checkpoint(&mut self) {
        self.checkpoints.push(Checkpoint { entry: self.entries.len().saturating_sub(1), files: Default::default(), turn_files: Default::default(), undone: Default::default(), started: std::time::Instant::now(), duration: None });
    }

    /// The turn the message at `entry` started (its checkpoint), files changed or not.
    pub(crate) fn turn_at(&self, entry: usize) -> Option<&Checkpoint> {
        self.checkpoints.iter().find(|c| c.entry == entry)
    }

    /// What the agent is doing now, while it works, and whether it is waiting for the user.
    pub(crate) fn activity(&self) -> Option<(String, bool)> {
        if self.status != Status::Busy {
            return None;
        }
        let waiting = self.entries.iter().rev().find_map(|e| match e {
            Entry::Permission { title, resolved: None, .. } => Some(title.clone()),
            Entry::Review { diff, outcome: None, .. } => Some(format!("review the change to {}", std::path::Path::new(&diff.edit.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())),
            Entry::Commit { reply: Some(_), .. } => Some("the proposed commit".to_string()),
            Entry::Push { reply: Some(_), .. } => Some("the proposed push".to_string()),
            Entry::LspEdit { plan, reply: Some(_), .. } => Some(plan.summary()),
            Entry::ExtensionTool { tool, reply: Some(_), .. } => Some(format!("let {} run", tool.title)),
            Entry::ForgeTool { name, reply: Some(_), .. } => Some(format!("let it {}", forge_tool_title(name).to_lowercase())),
            Entry::Remember { reply: Some(_), .. } => Some("keep a note".to_string()),
            Entry::Question { question, answer: None, .. } => Some(format!("answer \"{question}\"")),
            _ => None,
        });
        if let Some(title) = waiting {
            return Some((format!("Waiting for you: {title}"), true));
        }
        let running = self.entries.iter().rev().take_while(|e| !matches!(e, Entry::User(..))).find_map(|e| match e {
            Entry::Tool { title, status, .. } if status == "in_progress" || status == "pending" => Some(title.clone()),
            _ => None,
        });
        if let Some(title) = running {
            return Some((title, false));
        }
        Some(match self.entries.last() {
            Some(Entry::Thought(_)) => ("Thinking".into(), false),
            Some(Entry::Agent(_)) => ("Writing the answer".into(), false),
            _ => ("Working".into(), false),
        })
    }

    /// The title of the tool call running now, if any.
    pub(crate) fn running_tool(&self) -> Option<&str> {
        self.entries.iter().rev().take_while(|e| !matches!(e, Entry::User(..))).find_map(|e| match e {
            Entry::Tool { title, status, .. } if status == "in_progress" || status == "pending" => Some(title.as_str()),
            _ => None,
        })
    }

    /// The tool call running a command right now, with its terminal.
    pub(crate) fn running_terminal(&self) -> Option<(String, String)> {
        self.entries.iter().rev().take_while(|e| !matches!(e, Entry::User(..))).find_map(|e| match e {
            Entry::Tool { id, status, terminal: Some(terminal), .. } if status == "in_progress" || status == "pending" => Some((id.clone(), terminal.clone())),
            _ => None,
        })
    }

}

#[cfg(test)]
mod usage_tests {
    use super::Usage;
    use serde_json::json;

    #[test]
    fn reads_session_usage() {
        let mut usage = Usage::default();
        assert!(usage.is_empty() && usage.context_ratio().is_none());
        usage.apply_update(&json!({"sessionUpdate": "usage_update", "used": 50_000, "size": 200_000, "cost": {"amount": 0.42, "currency": "USD"}}));
        assert_eq!(usage.context_ratio(), Some(0.25));
        assert_eq!(usage.cost, Some((0.42, "USD".into())));
        usage.add_turn(&json!({"totalTokens": 1500, "inputTokens": 1200, "outputTokens": 300, "cachedReadTokens": 800}));
        usage.add_turn(&json!({"inputTokens": 100, "outputTokens": 50}));
        assert_eq!((usage.input, usage.output, usage.cached_read, usage.turns), (1300, 350, 800, 2));
    }

    #[test]
    fn formats_token_counts() {
        use crate::threads::tokens;
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(1_000), "1k");
        assert_eq!(tokens(12_345), "12.3k");
        assert_eq!(tokens(200_000), "200k");
        assert_eq!(tokens(1_500_000), "1.5M");
    }
}
