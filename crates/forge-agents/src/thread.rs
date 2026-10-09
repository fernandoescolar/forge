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
    /// Each file's problems before the agent first changed it in this thread: what
    /// `check_file` tells new problems from.
    problem_baseline: HashMap<PathBuf, Vec<crate::verify::Problem>>,
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
        let policy = ReviewPolicy { reviews: review_writes.then_some(review_tx), approved: approved_edits.clone(), permissions: permissions.clone(), written: Some(written_tx), problems_read: Default::default() };
        let fs = Arc::new(ProjectFs::new(&project, root.clone(), policy, cx));
        let terminals = TerminalRegistry::default();
        let terminal_host = Arc::new(ZedTerminals::new(&project, terminals.clone(), cx));
        let runtime = Arc::new(AcpRuntime::new(bus, fs).with_terminals(terminal_host).with_terminal_auth().with_display_terminals());

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
            problem_baseline: HashMap::new(),
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
        self.entries.iter().filter(|e| matches!(e, Entry::Review { reply: Some(_), .. } | Entry::Permission { resolved: None, .. } | Entry::Commit { reply: Some(_), .. } | Entry::Push { reply: Some(_), .. } | Entry::LspEdit { reply: Some(_), .. } | Entry::ExtensionTool { reply: Some(_), .. } | Entry::Remember { reply: Some(_), .. } | Entry::Question { answer: None, .. })).count()
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

    pub fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ended {
            self.system("This thread's worktree was removed; start a new thread.", Color::Warning);
            return self.changed(cx);
        }
        let Some(mut spec) = self.agents.get(self.selected).cloned() else { return };
        self.persist(cx);
        self.warned_auth = false;
        self.auth_pending = None;
        self.status = Status::Connecting;
        let resume = self.resume.take();
        self.replaying = resume.is_some();
        let restart = std::mem::take(&mut self.restart_keeping_transcript);
        if resume.is_none() && !restart {
            self.entries.clear();
            self.checkpoints.clear();
            self.started_at = history::now();
        }
        self.stderr.clear();
        self.system(format!("Starting {} ({} {})…", spec.id, spec.command, spec.args.join(" ")), Color::Muted);
        // Agents get the login-shell environment of the project root (PATH, nvm, asdf, …),
        // like Zed's terminals. Without it, a Forge launched from Finder can't find `npx`.
        let shell_env = self.project.upgrade().map(|project| {
            let root: Arc<std::path::Path> = self.root.clone().into();
            project.update(cx, |p, cx| p.environment().update(cx, |env, cx| env.directory_environment(root, cx)))
        });
        let (rt, root, id) = (self.runtime.clone(), self.root.clone(), spec.id.clone());
        let resume_id = resume.as_ref().map(|r| r.session_id.clone());
        let mut mcp = self.mcp_servers.clone();
        let forge_server = self.forge_mcp.as_ref().map(|r| r.acp_server());
        let spec_for_auth = spec.clone();
        // The agent's `initialize` answer, kept even when `session/new` fails: its login
        // methods are what that failure (not signed in) needs.
        let initialized: Arc<std::sync::Mutex<Option<Value>>> = Arc::default();
        let initialized_slot = initialized.clone();
        cx.spawn_in(window, async move |this, cx| {
            let mut env: Vec<(String, String)> = match shell_env {
                Some(task) => task.await.unwrap_or_default().into_iter().collect(),
                None => vec![],
            };
            // Entries from agents.json win over the shell environment.
            env.append(&mut spec.env);
            spec.env = env;
            let task = cx.update(|_, cx| {
                let id = id.clone();
                Tokio::spawn_result(cx, async move {
                    if restart && rt.running().contains(&spec.id) {
                        // Pick up fresh credentials: agents read them at startup.
                        rt.stop(&spec.id).await?;
                        for _ in 0..50 {
                            if !rt.running().contains(&spec.id) {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }
                    if !rt.running().contains(&spec.id) {
                        rt.start(spec).await?;
                    }
                    let init = rt.initialize(&id, PROTOCOL_VERSION).await?;
                    *initialized_slot.lock().unwrap() = Some(init.clone());
                    // Forge's own tools, for agents that take HTTP MCP servers.
                    if let (Some(server), true) = (forge_server, init.pointer("/agentCapabilities/mcpCapabilities/http").and_then(Value::as_bool).unwrap_or(false)) {
                        mcp.push(server);
                    }
                    let can_load = init.pointer("/agentCapabilities/loadSession").and_then(Value::as_bool).unwrap_or(false);
                    if let (Some(sid), true) = (resume_id, can_load) {
                        // Agents may not know the session anymore (e.g. it was restarted); fall back.
                        if let Ok(loaded) = rt.load_session(&id, &sid, &root, &mcp).await {
                            return anyhow::Ok((init, json!({ "sessionId": sid, "resumed": true, "modes": loaded.get("modes").cloned().unwrap_or_default(), "configOptions": loaded.get("configOptions").cloned().unwrap_or_default() })));
                        }
                    }
                    let session = rt.new_session(&id, &root, &mcp).await?;
                    anyhow::Ok((init, session))
                })
            });
            let result = match task {
                Ok(task) => task.await,
                Err(e) => Err(e),
            };
            this.update_in(cx, |this, _window, cx| {
                this.replaying = false;
                match result.and_then(|(init, session)| {
                    let sid = session.get("sessionId").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("agent returned no sessionId: {session}"))?;
                    let resumed = session.get("resumed").and_then(Value::as_bool).unwrap_or(false);
                    Ok((init, sid.to_string(), resumed, session.get("modes").cloned().unwrap_or_default(), parse_config_options(session.get("configOptions"))))
                }) {
                    Ok((init, sid, resumed, modes, config_options)) => {
                        this.read_modes(&modes);
                        this.config_options = config_options;
                        this.auth_methods = parse_auth_methods(&init, &spec_for_auth);
                        this.agent_name = init.pointer("/agentInfo/name").and_then(Value::as_str).map(str::to_string);
                        this.embedded_context = init.pointer("/agentCapabilities/promptCapabilities/embeddedContext").and_then(Value::as_bool).unwrap_or(false);
                        this.accepts_images = init.pointer("/agentCapabilities/promptCapabilities/image").and_then(Value::as_bool).unwrap_or(false);
                        this.forge_tools = this.forge_mcp.is_some() && init.pointer("/agentCapabilities/mcpCapabilities/http").and_then(Value::as_bool).unwrap_or(false);
                        this.connected = Some((id.clone(), sid));
                        // Needs the connection: it changes the session's mode.
                        this.apply_policy_mode(cx);
                        // A resumed session got them with its first message.
                        this.rules_sent = resumed;
                        this.status = Status::Ready;
                        let name = this.agent_name.clone().unwrap_or(id);
                        match (resumed, this.entries.iter().any(|e| matches!(e, Entry::User(..)))) {
                            (true, _) => this.system(format!("Resumed the session with {name}."), Color::Success),
                            (false, true) => this.system(format!("{name} can't resume that session; started a new one (the conversation above is for reference only)."), Color::Warning),
                            (false, false) => this.system(format!("Connected to {name}."), Color::Success),
                        }
                        if let Some(reason) = this.auth_pending.take() {
                            this.show_auth(&reason, cx);
                        }
                        if let Some(text) = this.retry_prompt.take() {
                            cx.emit(ThreadEvent::RestoreInput(text));
                            this.system("Your last message is back in the input; press Enter to send it again.", Color::Muted);
                        }
                        if let Some(text) = this.queued_prompt.take() {
                            this.send(text, None, cx);
                        }
                    }
                    Err(e) => {
                        this.status = Status::Disconnected;
                        this.system(format!("Could not connect: {e:#}"), Color::Error);
                        if let Some(init) = initialized.lock().unwrap().take() {
                            this.auth_methods = parse_auth_methods(&init, &spec_for_auth);
                        }
                        if is_auth_error(&format!("{e:#}")) {
                            this.show_auth("Signing in should fix this.", cx);
                        }
                        for line in this.stderr.iter().rev().take(5).collect::<Vec<_>>().into_iter().rev() {
                            this.entries.push(Entry::System(line.clone(), Color::Muted));
                        }
                    }
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
        self.changed(cx);
    }

    pub fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.persist(cx);
        if let Some((id, _)) = self.connected.take() {
            let rt = self.runtime.clone();
            Tokio::spawn(cx, async move { rt.stop(&id).await }).detach();
        }
        self.status = Status::Disconnected;
        self.changed(cx);
    }

    /// Sends `text` (with `active`, the editor context the view chose to attach). Returns
    /// whether it was sent.
    pub fn send(&mut self, text: String, active: Option<ActiveContext>, cx: &mut Context<Self>) -> bool {
        self.send_with_images(text, active, vec![], cx)
    }

    /// Whether the agent takes images in prompts (ACP `promptCapabilities.image`).
    pub fn accepts_images(&self) -> bool {
        self.accepts_images
    }

    /// [`Thread::send`] with images (pasted or dropped in the composer).
    pub fn send_with_images(&mut self, text: String, active: Option<ActiveContext>, images: Vec<Arc<gpui::Image>>, cx: &mut Context<Self>) -> bool {
        let text = text.trim().to_string();
        if text.is_empty() || self.status != Status::Ready {
            return false;
        }
        let Some((agent, session)) = self.connected.clone() else { return false };
        // `/name …` for one of the user's prompts sends the prompt; the thread shows what was typed.
        let prompt = crate::library::expand_prompt(&text, &self.root);
        let sent = prompt.as_ref().map(|(_, expanded)| expanded.clone()).unwrap_or_else(|| text.clone());
        let mut files: Vec<(PathBuf, Option<u32>)> = Vec::new();
        let mut specials: Vec<String> = Vec::new();
        for mention in resolve_mentions(&sent, &self.root) {
            match mention {
                Mention::Path { path, line } => files.push((path, line)),
                Mention::Special(name) => specials.push(name),
            }
        }
        let mut labels: Vec<String> = files
            .iter()
            .map(|(p, line)| {
                let rel = p.strip_prefix(&self.root).unwrap_or(p).to_string_lossy().into_owned();
                match line {
                    Some(line) => format!("{rel}:{line}"),
                    None => rel,
                }
            })
            .collect();
        labels.extend(specials.iter().map(|s| format!("@{s}")));
        if let Some((prompt, _)) = &prompt {
            labels.insert(0, format!("prompt {}", prompt.path.strip_prefix(&self.root).unwrap_or(&prompt.path).display()));
        }
        let images = if images.is_empty() || self.accepts_images {
            images
        } else {
            self.system(format!("{} doesn't take images; sent without them.", self.agent_label()), Color::Warning);
            vec![]
        };
        labels.extend((1..=images.len()).map(|i| format!("image {i}")));
        if let Some(a) = &active {
            labels.push(a.label(&self.root));
        }
        let mut blocks = prompt_blocks(&sent, &files, active.as_ref(), self.embedded_context, &self.root);
        for image in &images {
            use base64::Engine as _;
            let data = base64::engine::general_purpose::STANDARD.encode(&image.bytes);
            blocks.push(serde_json::json!({ "type": "image", "mimeType": image.format.mime_type(), "data": data }));
        }
        let extras = crate::context::gather(&specials, self.workspace.clone(), self.project.clone(), self.root.clone(), cx);
        let rules = (!self.rules_sent).then(|| crate::rules::load(<dyn fs::Fs>::global(cx), self.root.clone(), crate::rules::user_file(), self.instructions_files()));
        if !self.rules_sent && self.forge_tools {
            blocks.push(json!({ "type": "text", "text": crate::forge_mcp::instructions(&self.root) }));
        }
        self.rules_sent = true;
        let retry_text = text.clone();
        self.entries.push(Entry::User(text, labels));
        self.begin_checkpoint();
        if self.verify_enabled(cx) {
            self.diagnostics_before = self.project.upgrade().map(|p| crate::verify::diagnostic_counts(&p, cx)).unwrap_or_default();
        }
        self.status = Status::Busy;
        let rt = self.runtime.clone();
        cx.spawn(async move |this, cx| {
            blocks.extend(extras.await);
            if let Some(rules) = match rules {
                Some(rules) => rules.await,
                None => None,
            } {
                blocks.push(rules.block());
                this.update(cx, |this, cx| {
                    this.system(format!("Sent your instructions from {}.", rules.label()), Color::Muted);
                    this.changed(cx);
                })
                .ok();
            }
            let result = Tokio::spawn_result(cx, async move { Ok(rt.prompt_content(&agent, &session, blocks).await?) }).await;
            this.update(cx, |this, cx| {
                if this.status == Status::Busy {
                    this.status = Status::Ready;
                }
                if let Some(turn) = this.checkpoints.last_mut() {
                    turn.duration.get_or_insert_with(|| turn.started.elapsed());
                }
                if let Some(usage) = result.as_ref().ok().and_then(|r| r.get("usage")) {
                    this.usage.add_turn(usage);
                }
                this.refresh_plan_usage(cx);
                match result {
                    Ok(r) => match r.get("stopReason").and_then(Value::as_str) {
                        Some("end_turn") | None => {}
                        Some("cancelled") => this.system("Cancelled.", Color::Muted),
                        Some(other) => this.system(format!("Turn ended: {other}"), Color::Warning),
                    },
                    Err(e) => {
                        let msg = format!("{e:#}");
                        if is_auth_error(&msg) {
                            this.retry_prompt = Some(retry_text);
                            this.show_auth("The agent needs you to sign in before it can answer.", cx);
                        } else {
                            this.system(format!("Prompt failed: {msg}"), Color::Error);
                        }
                    }
                }
                this.persist(cx);
                this.check_changed_files(cx);
                this.send_next_queued(cx);
                this.changed(cx);
            })
            .ok();
        })
        .detach();
        self.changed(cx);
        true
    }

    fn verify_enabled(&self, cx: &gpui::App) -> bool {
        crate::settings::try_global(cx).is_none_or(|s| s.read(cx).config().verify_changes)
    }

    /// After a turn that wrote files: their errors and warnings, once the language
    /// servers have caught up.
    pub(crate) fn check_changed_files(&mut self, cx: &mut Context<Self>) {
        let files: Vec<PathBuf> = self.checkpoints.last().map(|c| c.files.keys().cloned().collect()).unwrap_or_default();
        if files.is_empty() || !self.verify_enabled(cx) {
            return;
        }
        let ix = self.entries.len();
        self.entries.push(Entry::Check(None));
        let before = std::mem::take(&mut self.diagnostics_before);
        let task = crate::verify::check(self.project.clone(), files.clone(), before.clone(), cx);
        self.check_watch = None;
        cx.spawn(async move |this, cx| {
            let checks = task.await;
            this.update(cx, |this, cx| {
                if let Some(entry @ Entry::Check(None)) = this.entries.get_mut(ix) {
                    *entry = Entry::Check(Some(checks));
                }
                this.watch_check(ix, files, before, cx);
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Language servers may report on the files later (a slow first build): the check at
    /// `ix` follows their diagnostics until the next one starts.
    fn watch_check(&mut self, ix: usize, files: Vec<PathBuf>, before: HashMap<PathBuf, (usize, usize)>, cx: &mut Context<Self>) {
        let Some(project) = self.project.upgrade() else { return };
        self.check_watch = Some(cx.subscribe(&project, move |_, project, event: &project::Event, cx| {
            let project::Event::DiagnosticsUpdated { paths, .. } = event else { return };
            let touched = paths.iter().filter_map(|p| project.read(cx).absolute_path(p, cx)).any(|abs| files.contains(&abs));
            if !touched {
                return;
            }
            let (weak, files, before) = (project.downgrade(), files.clone(), before.clone());
            cx.spawn(async move |this, cx| {
                let checks = crate::verify::read_problems(weak, files, before, cx).await;
                this.update(cx, |this, cx| {
                    if let Some(entry @ Entry::Check(Some(_))) = this.entries.get_mut(ix) {
                        *entry = Entry::Check(Some(checks));
                        this.changed(cx);
                    }
                })
                .ok();
            })
            .detach();
        }));
    }

    /// Sends the problems found by the check at `ix` back to the agent (queued if it is working).
    pub(crate) fn fix_problems(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Entry::Check(Some(checks))) = self.entries.get(ix) else { return };
        let prompt = crate::verify::fix_prompt(checks, &self.root);
        self.ask(prompt, window, cx);
    }

    /// Opens `path` at `line` (zero-based).
    pub(crate) fn open_at(&self, path: PathBuf, line: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        let open = workspace.update(cx, |ws, cx| ws.open_abs_path(path, workspace::OpenOptions::default(), window, cx));
        cx.spawn_in(window, async move |_, cx| {
            let item = open.await?;
            if let Some(editor) = item.downcast::<editor::Editor>() {
                editor.update_in(cx, |editor, window, cx| {
                    let point = language::Point::new(line, 0);
                    editor.change_selections(editor::SelectionEffects::scroll(editor::scroll::Autoscroll::center()), window, cx, |s| s.select_ranges([point..point]));
                })?;
            }
            anyhow::Ok(())
        })
        .detach();
    }

    /// Adds a message for after the current turn.
    pub(crate) fn enqueue(&mut self, text: String, active: Option<ActiveContext>, images: Vec<Arc<gpui::Image>>, cx: &mut Context<Self>) {
        self.queue.push(Queued { text, active, images });
        self.changed(cx);
    }

    pub(crate) fn unqueue(&mut self, ix: usize, cx: &mut Context<Self>) -> Option<String> {
        let removed = (ix < self.queue.len()).then(|| self.queue.remove(ix).text);
        self.changed(cx);
        removed
    }

    /// Sends a queued message now: it goes first and the current turn stops.
    pub(crate) fn send_queued_now(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.queue.len() {
            let item = self.queue.remove(ix);
            self.queue.insert(0, item);
        }
        if self.status == Status::Busy {
            self.cancel(cx);
        } else {
            self.send_next_queued(cx);
        }
        self.changed(cx);
    }

    fn send_next_queued(&mut self, cx: &mut Context<Self>) {
        if self.status != Status::Ready || self.queue.is_empty() || self.auth_pending.is_some() {
            return;
        }
        let Queued { text, active, images } = self.queue.remove(0);
        self.send_with_images(text, active, images, cx);
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.status != Status::Busy {
            return;
        }
        if let Some((agent, session)) = self.connected.clone() {
            let rt = self.runtime.clone();
            Tokio::spawn(cx, async move { rt.cancel(&agent, &session).await }).detach();
        }
    }

    /// Saves the conversation (in the background) if there is one worth keeping.
    pub(crate) fn persist(&self, cx: &mut Context<Self>) {
        let Some((agent_id, session_id)) = self.connected.clone() else { return };
        let entries: Vec<RecordEntry> = self
            .entries
            .iter()
            .filter_map(|e| {
                Some(match e {
                    Entry::User(text, context) => RecordEntry::User { text: text.clone(), context: context.clone() },
                    Entry::Agent(md) => RecordEntry::Agent { text: md.read(cx).source().to_string() },
                    Entry::Thought(md) => RecordEntry::Thought { text: md.read(cx).source().to_string() },
                    Entry::Tool { title, kind, status, .. } => RecordEntry::Tool { title: title.clone(), kind: kind.clone(), status: status.clone() },
                    Entry::Plan(items) => RecordEntry::Plan { items: items.clone() },
                    Entry::Permission { title, resolved, .. } => RecordEntry::System { text: format!("Permission: {title} → {}", resolved.clone().unwrap_or_else(|| "pending".into())) },
                    Entry::Review { diff, outcome, .. } => RecordEntry::System { text: format!("Edit {}: {}", diff.edit.path, outcome.unwrap_or("pending")) },
                    Entry::Commit { message, state, .. } => RecordEntry::System { text: format!("Commit \"{}\": {}", message.lines().next().unwrap_or_default(), crate::commit_proposal::outcome(state)) },
                    Entry::Push { plan, state, .. } => RecordEntry::System { text: format!("Push of {} to {}: {}", plan.branch, plan.target(), crate::push_proposal::outcome(plan, state)) },
                    Entry::Question { question, answer, .. } => RecordEntry::System { text: format!("Asked: {question} → {}", answer.as_deref().map(|a| if a.is_empty() { "skipped" } else { a }).unwrap_or("not answered")) },
                    Entry::Remember { note, state, .. } => RecordEntry::System { text: format!("Note \"{}\": {}", note.read(cx).text(cx).trim(), state.outcome()) },
                    Entry::ExtensionTool { tool, state, .. } => RecordEntry::System { text: format!("{} ({}): {}", tool.title, tool.extension, state.outcome()) },
                    Entry::LspEdit { plan, state, .. } => RecordEntry::System { text: format!("Asked to {}: {}", plan.summary(), state.outcome()) },
                    Entry::System(..) | Entry::Auth { .. } | Entry::Check(_) => return None,
                })
            })
            .collect();
        if !entries.iter().any(|e| matches!(e, RecordEntry::User { .. })) {
            return;
        }
        let record = SessionRecord {
            session_id,
            agent_id,
            agent_name: self.agent_name.clone(),
            project_root: self.root.clone(),
            title: history::title_for(&entries),
            started_at: self.started_at,
            updated_at: history::now(),
            entries,
        };
        let dir = self.history_dir.clone();
        cx.background_spawn(async move {
            if let Err(e) = history::save(&dir, &record) {
                log::error!("failed to save agent session: {e:#}");
            }
        })
        .detach();
    }

    /// Saved conversations of this project, newest first.
    pub fn history(&self) -> Vec<SessionSummary> {
        history::list(&self.history_dir, &self.root)
    }

    /// Shows a saved conversation; connecting resumes it.
    pub fn open_session(&mut self, summary: &SessionSummary, cx: &mut Context<Self>) {
        let record = match history::load(&summary.file) {
            Ok(r) => r,
            Err(e) => {
                self.system(format!("{e:#}"), Color::Error);
                self.changed(cx);
                return;
            }
        };
        if self.status != Status::Disconnected {
            self.disconnect(cx);
        }
        if let Some(i) = self.agents.iter().position(|a| a.id == record.agent_id) {
            self.selected = i;
        }
        self.entries = record
            .entries
            .iter()
            .map(|e| match e {
                RecordEntry::User { text, context } => Entry::User(text.clone(), context.clone()),
                RecordEntry::Agent { text } => Entry::Agent(cx.new(|cx| Markdown::new(text.clone().into(), Some(self.languages.clone()), None, cx))),
                RecordEntry::Thought { text } => Entry::Thought(cx.new(|cx| Markdown::new(text.clone().into(), Some(self.languages.clone()), None, cx))),
                RecordEntry::Tool { title, kind, status } => {
                    Entry::Tool { id: String::new(), title: title.clone(), kind: kind.clone(), status: status.clone(), detail: None, terminal: None, diffs: vec![], command: None }
                }
                RecordEntry::Plan { items } => Entry::Plan(items.clone()),
                RecordEntry::System { text } => Entry::System(text.clone(), Color::Muted),
            })
            .collect();
        self.system(format!("Saved conversation from {}. Connect to continue it.", history::relative_time(record.updated_at, history::now())), Color::Muted);
        self.started_at = record.started_at;
        self.resume = Some(record);
        self.changed(cx);
    }

    /// Stops the agent and deletes the saved conversation for good, unlike `disconnect`
    /// (which saves it).
    pub fn delete_conversation(&mut self, cx: &mut Context<Self>) {
        let file = self.session_id().map(|id| history::session_file(&self.history_dir, &self.root, id));
        if let Some((id, _)) = self.connected.take() {
            let rt = self.runtime.clone();
            Tokio::spawn(cx, async move { rt.stop(&id).await }).detach();
        }
        self.resume = None;
        self.status = Status::Disconnected;
        if let Some(file) = file.filter(|f| f.exists()) {
            if let Err(e) = history::delete(&file) {
                log::error!("{e:#}");
            }
        }
        self.changed(cx);
    }

    pub fn delete_session(&mut self, summary: &SessionSummary, cx: &mut Context<Self>) {
        if let Err(e) = history::delete(&summary.file) {
            log::error!("{e:#}");
        }
        self.changed(cx);
    }

    /// Adds a sign-in card (or, if the agent offers no login methods, instructions).
    fn show_auth(&mut self, reason: &str, cx: &mut Context<Self>) {
        let pending = self.entries.iter().any(|e| matches!(e, Entry::Auth { state: AuthState::Waiting | AuthState::Running(_), .. }));
        if pending {
            return;
        }
        if self.status == Status::Connecting {
            // Login methods arrive with the connection result; decide then.
            self.auth_pending = Some(reason.to_string());
            return;
        }
        if self.auth_methods.is_empty() {
            let hint = self.login_hint();
            self.system(format!("{reason} {hint}"), Color::Warning);
        } else {
            self.system(reason.to_string(), Color::Warning);
            self.entries.push(Entry::Auth { methods: self.auth_methods.clone(), terminal: None, state: AuthState::Waiting });
        }
        self.changed(cx);
    }

    fn set_auth_state(&mut self, ix: usize, new_state: AuthState, cx: &mut Context<Self>) {
        if let Some(Entry::Auth { state, .. }) = self.entries.get_mut(ix) {
            *state = new_state;
        }
        self.changed(cx);
    }

    /// Runs login method `m` of the sign-in card at `ix`; on success restarts the agent.
    pub(crate) fn run_auth(&mut self, ix: usize, m: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Entry::Auth { methods, .. }) = self.entries.get(ix) else { return };
        let Some(method) = methods.get(m).cloned() else { return };
        self.set_auth_state(ix, AuthState::Running(method.name.clone()), cx);
        match method.action {
            AuthAction::Terminal { command, args, env } => {
                let shell_env = self.project.upgrade().map(|project| {
                    let root: Arc<std::path::Path> = self.root.clone().into();
                    project.update(cx, |p, cx| p.environment().update(cx, |env, cx| env.directory_environment(root, cx)))
                });
                let (project, workspace, root) = (self.project.clone(), self.workspace.clone(), self.root.clone());
                cx.spawn_in(window, async move |this, cx| {
                    let mut full_env: Vec<(String, String)> = match shell_env {
                        Some(task) => task.await.unwrap_or_default().into_iter().collect(),
                        None => vec![],
                    };
                    full_env.extend(env);
                    let request = TerminalRequest { command, args, env: full_env, cwd: Some(root), output_byte_limit: None };
                    let terminal = match spawn_interactive(&project, request, cx).await {
                        Ok(t) => t,
                        Err(e) => {
                            this.update(cx, |this, cx| this.set_auth_state(ix, AuthState::Failed(format!("{e:#}")), cx)).ok();
                            return;
                        }
                    };
                    let exit = this.update_in(cx, |this, window, cx| {
                        let view = cx.new(|cx| {
                            let mut view = TerminalView::new(terminal.clone(), workspace.clone(), None, project.clone(), window, cx);
                            view.set_embedded_mode(Some(14), cx);
                            view
                        });
                        // Focus it so the user can type straight away (codes, confirmations).
                        window.focus(&view.focus_handle(cx), cx);
                        if let Some(Entry::Auth { terminal: slot, .. }) = this.entries.get_mut(ix) {
                            *slot = Some(view);
                        }
                        this.changed(cx);
                        terminal.read(cx).wait_for_completed_task(cx)
                    });
                    let Ok(exit) = exit else { return };
                    let status = exit.await;
                    this.update_in(cx, |this, window, cx| match status {
                        Some(s) if s.success() => this.finish_auth(ix, window, cx),
                        Some(s) => this.set_auth_state(ix, AuthState::Failed(format!("login exited with {s}")), cx),
                        None => this.set_auth_state(ix, AuthState::Failed("login was interrupted".into()), cx),
                    })
                    .ok();
                })
                .detach();
            }
            AuthAction::Authenticate => {
                let Some((agent, _)) = self.connected.clone() else { return };
                let rt = self.runtime.clone();
                let id = method.id.clone();
                let task = Tokio::spawn_result(cx, async move { Ok(rt.rpc(&agent, "authenticate", json!({ "methodId": id })).await?) });
                cx.spawn_in(window, async move |this, cx| {
                    let result = task.await;
                    this.update_in(cx, |this, window, cx| match result {
                        Ok(_) => this.finish_auth(ix, window, cx),
                        Err(e) => this.set_auth_state(ix, AuthState::Failed(format!("{e:#}")), cx),
                    })
                    .ok();
                })
                .detach();
            }
        }
    }

    fn finish_auth(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.set_auth_state(ix, AuthState::Done, cx);
        self.system("Signed in. Restarting the agent…", Color::Success);
        self.restart_keeping_transcript = true;
        self.connect(window, cx);
    }

    /// How to sign in to the selected agent (agents authenticate through their own CLIs).
    fn login_hint(&self) -> String {
        let spec = self.agents.get(self.selected);
        let launches = |needle: &str| spec.is_some_and(|a| a.command.contains(needle) || a.args.iter().any(|x| x.contains(needle)));
        if launches("claude") {
            "Run `claude` in a terminal and type `/login` (it shares credentials with the ACP adapter), then press + for a new session.".into()
        } else if launches("gemini") {
            "Run `gemini` in a terminal and complete the login, then press + for a new session.".into()
        } else if launches("codex") {
            "Run `codex login` in a terminal, then press + for a new session.".into()
        } else if launches("copilot") {
            "Run `npx @github/copilot login` in a terminal (or `copilot login`), then press + for a new session.".into()
        } else {
            "Sign in with the agent's own CLI, then press + for a new session.".into()
        }
    }

    fn diff_views(&self, edits: Vec<Edit>, window: &mut Window, cx: &mut Context<Self>) -> Vec<DiffView> {
        edits.into_iter().map(|e| DiffView::new(e, self.languages.clone(), window, cx)).collect()
    }

    fn add_review(&mut self, review: WriteReview, window: &mut Window, cx: &mut Context<Self>) {
        let edit = Edit { path: review.path.to_string_lossy().into_owned(), old_text: review.old_text, new_text: review.new_text };
        let hunks: Vec<(Hunk, bool)> = hunks(edit.old_text.as_deref().unwrap_or_default(), &edit.new_text).into_iter().map(|h| (h, true)).collect();
        let first_change = hunks.first().map(|(h, _): &(Hunk, bool)| h.new.start);
        self.location = Some(AgentLocation { path: review.path.clone(), line: first_change, kind: Some("edit".into()) });
        // Editable: the user can adjust the proposal before accepting it.
        let diff = DiffView::editable(edit, self.languages.clone(), window, cx);
        self.entries.push(Entry::Review { diff, hunks, reply: Some(review.reply), outcome: None });
        self.changed(cx);
    }

    pub(crate) fn answer_review(&mut self, ix: usize, accept: bool, cx: &mut Context<Self>) {
        let edited = match self.entries.get(ix) {
            Some(Entry::Review { diff, .. }) => diff.edited_text(cx),
            _ => None,
        };
        if let Some(Entry::Review { diff, hunks, reply, outcome }) = self.entries.get_mut(ix) {
            if let Some(reply) = reply.take() {
                let selected = hunks.iter().filter(|(_, on)| *on).count();
                let (decision, label) = match (accept, selected, edited) {
                    // The user rewrote the proposal: write what they left.
                    (true, _, Some(text)) => (ReviewDecision::Partial(text), "Applied with your edits"),
                    (false, _, _) | (true, 0, None) => (ReviewDecision::Reject, "Rejected"),
                    (true, n, None) if n == hunks.len() => (ReviewDecision::Accept, "Applied"),
                    (true, _, None) => {
                        let (h, on): (Vec<Hunk>, Vec<bool>) = hunks.iter().cloned().unzip();
                        (ReviewDecision::Partial(apply_hunks(diff.edit.old_text.as_deref().unwrap_or_default(), &diff.edit.new_text, &h, &on)), "Partially applied")
                    }
                };
                let _ = reply.send(decision);
                *outcome = Some(label);
                diff.set_read_only(cx);
            }
        }
        self.changed(cx);
    }

    /// Accepts or rejects every write waiting for review.
    pub fn answer_all_reviews(&mut self, accept: bool, cx: &mut Context<Self>) {
        let pending: Vec<usize> = self.entries.iter().enumerate().filter(|(_, e)| matches!(e, Entry::Review { reply: Some(_), .. })).map(|(i, _)| i).collect();
        for ix in pending {
            self.answer_review(ix, accept, cx);
        }
    }

    pub(crate) fn toggle_hunk(&mut self, ix: usize, hunk: usize, cx: &mut Context<Self>) {
        if let Some(Entry::Review { hunks, reply: Some(_), .. }) = self.entries.get_mut(ix) {
            if let Some((_, on)) = hunks.get_mut(hunk) {
                *on = !*on;
            }
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

    /// The checkpoint taken when the message at `entry` was sent, if it has files to restore.
    pub(crate) fn checkpoint_at(&self, entry: usize) -> Option<&Checkpoint> {
        self.checkpoints.iter().find(|c| c.entry == entry).filter(|c| !c.files.is_empty())
    }

    /// The message at `entry` goes back to the input to be edited and sent again. When the
    /// agent changed files after it, the user chooses whether they go back as they were
    /// then too (the agent remembers the conversation either way).
    pub(crate) fn edit_message(&mut self, entry: usize, text: String, files: usize, window: &mut Window, cx: &mut Context<Self>) {
        if files == 0 {
            cx.emit(ThreadEvent::RestoreInput(text));
            return;
        }
        let detail = format!(
            "The agent changed {files} file{} after this message. Put {} back as {} then, before you send the new version?",
            if files == 1 { "" } else { "s" },
            if files == 1 { "it" } else { "them" },
            if files == 1 { "it was" } else { "they were" },
        );
        let answer = window.prompt(gpui::PromptLevel::Info, "Edit this message", Some(&detail), &["Restore files and edit", "Just edit", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            let choice = answer.await.ok();
            this.update(cx, |this, cx| {
                if choice == Some(0) {
                    this.restore_checkpoint(entry, cx);
                }
                if matches!(choice, Some(0 | 1)) {
                    cx.emit(ThreadEvent::RestoreInput(text));
                }
            })
            .ok();
        })
        .detach();
    }

    /// Puts every file the agent wrote since the message at `entry` back as it was then.
    /// The conversation stays; later checkpoints go (they describe a state that is gone).
    pub(crate) fn restore_checkpoint(&mut self, entry: usize, cx: &mut Context<Self>) {
        if self.status == Status::Busy {
            self.system("Stop the agent before restoring a checkpoint.", Color::Warning);
            return;
        }
        let Some(ix) = self.checkpoints.iter().position(|c| c.entry == entry) else { return };
        let files: Vec<(PathBuf, Option<String>)> = self.checkpoints[ix].files.clone().into_iter().collect();
        let message = match self.entries.get(entry) {
            Some(Entry::User(text, _)) => text.lines().next().unwrap_or_default().chars().take(60).collect::<String>(),
            _ => String::new(),
        };
        self.checkpoints.truncate(ix + 1);
        self.checkpoints[ix].files.clear();
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            let mut failed = Vec::new();
            for (path, content) in &files {
                if let Err(e) = crate::project_fs::restore(&project, path, content.clone(), cx).await {
                    failed.push(format!("{}: {e:#}", path.display()));
                }
            }
            this.update(cx, |this, cx| {
                for (path, content) in &files {
                    if let Some(change) = this.changes.iter_mut().find(|c| &c.path == path) {
                        change.current = content.clone().unwrap_or_default();
                    }
                }
                this.changes.retain(|c| c.original.as_deref() != Some(c.current.as_str()) && !(c.original.is_none() && c.current.is_empty()));
                let n = files.len() - failed.len();
                this.system(format!("Restored {n} file{} to how {} before “{message}”.", if n == 1 { "" } else { "s" }, if n == 1 { "it was" } else { "they were" }), Color::Muted);
                for f in failed {
                    this.system(format!("Could not restore {f}"), Color::Error);
                }
                this.changes_updated(cx);
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn record_write(&mut self, record: WriteRecord, cx: &mut Context<Self>) {
        self.warn_about_conflict(&record.path, cx);
        // Right after the write the language servers haven't looked at it yet: the problems
        // the file shows are still the ones it had before.
        if !self.problem_baseline.contains_key(&record.path) {
            let before = match (&record.problems_before, &record.old_text) {
                (Some(problems), _) => problems.clone(),
                (None, None) => Vec::new(),
                // Written without Forge's file system (language-server edits, agents that
                // write files themselves): the open buffer's, if any.
                (None, Some(_)) => self.project.upgrade().and_then(|p| crate::verify::open_file_problems(p.read(cx), &record.path, cx)).unwrap_or_default(),
            };
            self.problem_baseline.insert(record.path.clone(), before);
        }
        for checkpoint in &mut self.checkpoints {
            checkpoint.files.entry(record.path.clone()).or_insert_with(|| record.old_text.clone());
        }
        if let Some(turn) = self.checkpoints.last_mut().filter(|t| t.duration.is_none()) {
            let file = turn.turn_files.entry(record.path.clone()).or_insert_with(|| (record.old_text.clone(), String::new()));
            file.1 = record.new_text.clone();
            // Written back as it was before the turn: nothing changed in it.
            if file.0.as_deref() == Some(file.1.as_str()) {
                turn.turn_files.remove(&record.path);
            }
        }
        match self.changes.iter().position(|c| c.path == record.path) {
            Some(ix) => self.changes[ix].current = record.new_text,
            None => self.changes.push(ChangedFile { path: record.path, original: record.old_text, current: record.new_text }),
        }
        // Back to how it was: nothing left to review.
        self.changes.retain(|c| c.original.as_deref() != Some(c.current.as_str()));
        self.changes_updated(cx);
        self.changed(cx);
    }

    /// The changed files changed: views and editors showing them follow.
    pub(crate) fn changes_updated(&mut self, cx: &mut Context<Self>) {
        cx.emit(ThreadEvent::ChangesUpdated);
        let this = cx.entity();
        cx.defer(move |cx| crate::agent_review::thread_changed(&this, cx));
    }

    /// Part of a change was kept or undone in an editor: `baseline` is what the file is
    /// now compared against, `current` its content.
    pub(crate) fn set_change_baseline(&mut self, path: &std::path::Path, baseline: String, current: String, cx: &mut Context<Self>) {
        if let Some(change) = self.changes.iter_mut().find(|c| c.path == path) {
            change.original = Some(baseline);
            change.current = current;
        }
        self.changes.retain(|c| c.original.as_deref() != Some(c.current.as_str()));
        self.changes_updated(cx);
        self.changed(cx);
    }

    /// Accepts the agent's changes to `path` (all files when `None`): they stop being
    /// listed and can no longer be undone from the thread.
    pub(crate) fn keep_changes(&mut self, path: Option<&std::path::Path>, cx: &mut Context<Self>) {
        self.changes.retain(|c| path.is_some_and(|p| p != c.path));
        self.changes_updated(cx);
        self.changed(cx);
    }

    /// The files of the turn started by the message at `entry` (`path` alone if given, all
    /// of them otherwise) that a later turn changed again: undoing this turn loses those
    /// changes too.
    pub(crate) fn later_turns_touching(&self, entry: usize, path: Option<&std::path::Path>) -> Vec<PathBuf> {
        let Some(ix) = self.checkpoints.iter().position(|c| c.entry == entry) else { return vec![] };
        let turn = &self.checkpoints[ix];
        let paths: Vec<&PathBuf> = turn.turn_files.keys().filter(|p| !turn.undone.contains(*p) && path.is_none_or(|only| only == p.as_path())).collect();
        paths.into_iter().filter(|p| self.checkpoints[ix + 1..].iter().any(|later| later.turn_files.contains_key(*p) && !later.undone.contains(*p))).cloned().collect()
    }

    /// Rolls back the turn started by the message at `entry`: its files (`path` alone if
    /// given) go back as they were before the turn. The conversation stays.
    pub(crate) fn undo_turn(&mut self, entry: usize, path: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.status == Status::Busy {
            self.system("Stop the agent before undoing a turn.", Color::Warning);
            self.changed(cx);
            return;
        }
        let Some(turn) = self.checkpoints.iter().find(|c| c.entry == entry) else { return };
        let files: Vec<(PathBuf, Option<String>)> =
            turn.turn_files.iter().filter(|(p, _)| !turn.undone.contains(*p) && path.as_ref().is_none_or(|only| only == *p)).map(|(p, (before, _))| (p.clone(), before.clone())).collect();
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            let mut restored = Vec::new();
            let mut failed = Vec::new();
            for (path, before) in &files {
                match crate::project_fs::restore(&project, path, before.clone(), cx).await {
                    Ok(()) => restored.push((path.clone(), before.clone())),
                    Err(e) => failed.push(format!("{}: {e:#}", path.display())),
                }
            }
            this.update(cx, |this, cx| {
                let from = this.checkpoints.iter().position(|c| c.entry == entry).unwrap_or(this.checkpoints.len());
                for (path, before) in &restored {
                    // This turn and the later ones that changed it again are undone for it.
                    for turn in &mut this.checkpoints[from..] {
                        if turn.turn_files.contains_key(path) {
                            turn.undone.insert(path.clone());
                        }
                    }
                    if let Some(change) = this.changes.iter_mut().find(|c| &c.path == path) {
                        change.current = before.clone().unwrap_or_default();
                    }
                }
                this.changes.retain(|c| c.original.as_deref() != Some(c.current.as_str()) && !(c.original.is_none() && c.current.is_empty()));
                let n = restored.len();
                if n > 0 {
                    let what = match &restored[..] {
                        [(path, _)] => path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                        _ => format!("{n} files"),
                    };
                    this.system(format!("Undid this turn's changes to {what}."), Color::Muted);
                }
                for f in failed {
                    this.system(format!("Could not undo {f}"), Color::Error);
                }
                this.changes_updated(cx);
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Puts `path` (all changed files when `None`) back as it was before the agent.
    pub(crate) fn undo_changes(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let undo: Vec<ChangedFile> = self.changes.iter().filter(|c| path.as_ref().is_none_or(|p| *p == c.path)).cloned().collect();
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            for change in undo {
                let result = crate::project_fs::restore(&project, &change.path, change.original.clone(), cx).await;
                this.update(cx, |this, cx| {
                    match result {
                        Ok(()) => {
                            this.changes.retain(|c| c.path != change.path);
                            let name = change.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            this.system(format!("Undid the agent's changes to {name}."), Color::Muted);
                        }
                        Err(e) => this.system(format!("Could not undo the changes to {}: {e:#}", change.path.display()), Color::Error),
                    }
                    this.changes_updated(cx);
                    this.changed(cx);
                })
                .ok();
            }
        })
        .detach();
    }

    /// ACP `modes` from `session/new`: `{currentModeId, availableModes: [{id, name, description}]}`.
    fn read_modes(&mut self, modes: &Value) {
        self.modes = modes
            .get("availableModes")
            .and_then(Value::as_array)
            .map(|xs| {
                xs.iter()
                    .filter_map(|m| {
                        Some(SessionMode {
                            id: m.get("id")?.as_str()?.to_string(),
                            name: m.get("name")?.as_str()?.to_string(),
                            description: m.get("description").and_then(Value::as_str).map(str::to_string),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.current_mode = modes.get("currentModeId").and_then(Value::as_str).map(str::to_string);
    }

    /// Starts the agent in the mode that matches Forge's permissions, when it has one:
    /// fewer questions at the source (Claude stops asking about edits in Accept edits).
    fn apply_policy_mode(&mut self, cx: &mut Context<Self>) {
        use crate::permissions::PermissionMode;
        // The agent's own session modes that match the policy, best first (Claude Code:
        // `auto`, `acceptEdits`, `bypassPermissions`; Codex: `auto`; Gemini: `autoEdit`).
        let wanted: &[&str] = match self.permissions.get().mode {
            PermissionMode::Ask => &[],
            PermissionMode::AllowEdits | PermissionMode::AllowWorkspace => &["acceptEdits", "autoEdit"],
            PermissionMode::Auto => &["auto", "acceptEdits", "autoEdit"],
            PermissionMode::SuperUser => &["bypassPermissions", "acceptEdits", "autoEdit"],
        };
        if let Some(mode) = wanted.iter().find(|id| self.modes.iter().any(|m| m.id == **id)) {
            if self.current_mode.as_deref() != Some(mode) {
                self.set_mode(mode.to_string(), cx);
            }
        }
    }

    /// Changes one of the agent's session settings (`session/set_config_option`); the
    /// agent answers with all of them (a new model can change the efforts on offer).
    pub(crate) fn set_config_option(&mut self, id: String, value: Value, cx: &mut Context<Self>) {
        let Some((agent, session)) = self.connected.clone() else { return };
        // Show the choice right away; the answer replaces it.
        if let Some(option) = self.config_options.iter_mut().find(|o| o.id == id) {
            match (&mut option.value, &value) {
                (ConfigValue::Select { current, .. }, Value::String(v)) => *current = v.clone(),
                (ConfigValue::Boolean(on), Value::Bool(v)) => *on = *v,
                _ => {}
            }
            if option.category.as_deref() == Some("mode") {
                self.current_mode = value.as_str().map(str::to_string);
            }
        }
        let rt = self.runtime.clone();
        let task = Tokio::spawn_result(cx, async move {
            Ok(rt.rpc(&agent, "session/set_config_option", json!({"sessionId": session, "configId": id, "value": value})).await?)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(answer) => {
                        let options = parse_config_options(answer.get("configOptions"));
                        if !options.is_empty() {
                            this.config_options = options;
                        }
                    }
                    Err(e) => this.system(format!("Could not change that setting: {e:#}"), Color::Warning),
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
        self.changed(cx);
    }

    /// Switches the agent's session mode (`session/set_mode`).
    pub(crate) fn set_mode(&mut self, mode_id: String, cx: &mut Context<Self>) {
        let Some((agent, session)) = self.connected.clone() else { return };
        self.current_mode = Some(mode_id.clone());
        let rt = self.runtime.clone();
        let task = Tokio::spawn_result(cx, async move { Ok(rt.rpc(&agent, "session/set_mode", json!({"sessionId": session, "modeId": mode_id})).await?) });
        cx.spawn(async move |this, cx| {
            if let Err(e) = task.await {
                this.update(cx, |this, cx| {
                    this.system(format!("Could not change the mode: {e:#}"), Color::Warning);
                    this.changed(cx);
                })
                .ok();
            }
        })
        .detach();
        self.changed(cx);
    }

    /// Asks the connected agent `prompt` in a hidden session of its own and returns its
    /// answer (commit messages and other one-off questions; the thread shows nothing).
    pub fn ask_aside(&mut self, prompt: String, cx: &mut Context<Self>) -> gpui::Task<anyhow::Result<String>> {
        let Some((agent, _)) = self.connected.clone() else { return gpui::Task::ready(Err(anyhow::anyhow!("the agent is not connected"))) };
        let (rt, root) = (self.runtime.clone(), self.root.clone());
        let agent_for_session = agent.clone();
        let rt_for_session = rt.clone();
        let session = Tokio::spawn_result(cx, async move {
            rt_for_session
                .new_session(&agent_for_session, &root, &[])
                .await?
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("no sessionId"))
        });
        cx.spawn(async move |this, cx| {
            let session = session.await?;
            this.update(cx, |this, _| this.side_sessions.insert(session.clone(), String::new()))?;
            let prompt_session = session.clone();
            let result = Tokio::spawn_result(cx, async move { Ok(rt.prompt_content(&agent, &prompt_session, vec![json!({ "type": "text", "text": prompt })]).await?) }).await;
            let reply = this.update(cx, |this, _| this.side_sessions.remove(&session).unwrap_or_default())?;
            result?;
            Ok(reply)
        })
    }

    /// Asks the agent for its account's usage limits, when it offers `/usage` (Claude),
    /// in a hidden session; at most once a minute per agent.
    pub(crate) fn refresh_plan_usage(&mut self, cx: &mut Context<Self>) {
        let Some((agent, _)) = self.connected.clone() else { return };
        if !self.agent_commands.iter().any(|c| c.name == "usage") || self.usage_reply.is_some() || !crate::plan_usage::should_ask(&agent, cx) {
            return;
        }
        self.usage_reply = Some(String::new());
        let (rt, root, existing) = (self.runtime.clone(), self.root.clone(), self.usage_session.clone());
        let agent_for_task = agent.clone();
        let task = Tokio::spawn_result(cx, async move {
            let session = match existing {
                Some(session) => session,
                None => rt
                    .new_session(&agent_for_task, &root, &[])
                    .await?
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("no sessionId"))?,
            };
            anyhow::Ok(session)
        });
        cx.spawn(async move |this, cx| {
            let session = task.await;
            let prompt = this.update(cx, |this, cx| match session {
                Ok(session) => {
                    this.usage_session = Some(session.clone());
                    let rt = this.runtime.clone();
                    let agent = agent.clone();
                    Some(Tokio::spawn_result(cx, async move { Ok(rt.prompt_content(&agent, &session, vec![json!({"type": "text", "text": "/usage"})]).await?) }))
                }
                Err(e) => {
                    log::info!("could not open a session for /usage: {e:#}");
                    this.usage_reply = None;
                    None
                }
            });
            let Ok(Some(prompt)) = prompt else { return };
            let result = prompt.await;
            this.update(cx, |this, cx| {
                let reply = this.usage_reply.take().unwrap_or_default();
                match result {
                    Ok(_) => match crate::plan_usage::parse(&reply) {
                        Some(usage) => crate::plan_usage::set(&agent, usage, cx),
                        None => log::info!("/usage answered without limits"),
                    },
                    Err(e) => log::info!("/usage failed: {e:#}"),
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// The account usage of this thread's agent, if known.
    pub fn plan_usage(&self, cx: &gpui::App) -> Option<crate::plan_usage::PlanUsage> {
        let agent = self.connected.as_ref().map(|(a, _)| a.clone()).or_else(|| self.agents.get(self.selected).map(|a| a.id.clone()))?;
        crate::plan_usage::get(&agent, cx)
    }

    /// The workspace's folders (the thread's root first).
    fn roots(&self, cx: &gpui::App) -> Vec<PathBuf> {
        let mut roots = vec![self.root.clone()];
        if let Some(project) = self.project.upgrade() {
            roots.extend(project.read(cx).visible_worktrees(cx).map(|wt| wt.read(cx).abs_path().to_path_buf()));
        }
        roots
    }

    /// What Forge's tools work on, for this thread.
    fn ide(&self) -> crate::forge_tools::Ide {
        crate::forge_tools::Ide { project: self.project.clone(), workspace: self.workspace.clone(), root: self.root.clone() }
    }

    /// A call to a tool of Forge's MCP server (see `forge_mcp`).
    pub(crate) fn handle_tool_request(&mut self, request: crate::forge_mcp::ToolRequest, window: &mut Window, cx: &mut Context<Self>) {
        use crate::commit_proposal::Origin;
        use crate::forge_tools::{self as tools, str_arg};
        let crate::forge_mcp::ToolRequest { name, args, reply } = request;
        match name.as_str() {
            "propose_commit" => {
                let Some(message) = str_arg(&args, "message") else {
                    let _ = reply.send(Err("`message` is empty.".into()));
                    return;
                };
                let files = args.get("files").and_then(Value::as_array).map(|fs| fs.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
                self.propose_commit(message, files, Origin::Tool, Some(reply), cx);
            }
            "propose_push" => self.propose_push(str_arg(&args, "remote"), Origin::Tool, Some(reply), cx),
            "rename_symbol" | "apply_code_action" | "format_file" => self.propose_edit(name, args, reply, window, cx),
            "ask_user" => {
                let Some(question) = str_arg(&args, "question") else {
                    let _ = reply.send(Err("`question` is empty.".into()));
                    return;
                };
                let options: Vec<String> = args.get("options").and_then(Value::as_array).map(|xs| xs.iter().filter_map(Value::as_str).map(str::trim).filter(|o| !o.is_empty()).take(6).map(str::to_string).collect()).unwrap_or_default();
                let input = cx.new(|cx| {
                    let mut editor = editor::Editor::single_line(window, cx);
                    editor.set_placeholder_text(if options.is_empty() { "Your answer…" } else { "Or write another answer…" }, window, cx);
                    editor
                });
                self.entries.push(Entry::Question { question, options, input, answer: None, reply: Some(reply) });
                self.changed(cx);
            }
            "user_context" => {
                let ide = self.ide();
                cx.spawn_in(window, async move |_, cx| {
                    let _ = reply.send(tools::user_context(ide, cx).await);
                })
                .detach();
            }
            "remember" => {
                let Some(note) = str_arg(&args, "note") else {
                    let _ = reply.send(Err("`note` is empty.".into()));
                    return;
                };
                let file = crate::rules::project_file(&self.root, &self.instructions_files());
                let editor = cx.new(|cx| {
                    let mut editor = editor::Editor::auto_height(1, 8, window, cx);
                    editor.set_text(note, window, cx);
                    editor
                });
                self.entries.push(Entry::Remember { note: editor, file, state: crate::forge_tools::EditState::Waiting, reply: Some(reply) });
                self.changed(cx);
            }
            "notify" => {
                let Some(message) = str_arg(&args, "message") else {
                    let _ = reply.send(Err("`message` is empty.".into()));
                    return;
                };
                self.notify_user(message.clone(), cx);
                let _ = reply.send(Ok("The user was notified.".into()));
            }
            "show_changes" => {
                let answer = if self.changes.is_empty() {
                    Ok("You haven't changed any file in this conversation.".to_string())
                } else {
                    cx.emit(ThreadEvent::ShowChanges(str_arg(&args, "path").map(|p| self.root.join(p))));
                    Ok(format!("Opened the review of your changes ({} files) for the user.", self.changes.len()))
                };
                let _ = reply.send(answer);
            }
            _ if forge_ui::agent_tools::agent_tools().get(&name).is_some() => self.extension_tool(name, args, reply, window, cx),
            _ => {
                let ide = self.ide();
                // The files this thread changed (and still has changed), and their problems
                // before it did.
                let changed: Vec<PathBuf> = self.changes.iter().map(|c| c.path.clone()).collect();
                let baseline = self.problem_baseline.clone();
                cx.spawn_in(window, async move |_, cx| {
                    let answer = match name.as_str() {
                        "run_tests" => tools::run_tests(ide, args, cx).await,
                        "diagnostics" => tools::diagnostics(ide, args, &changed, cx).await,
                        "check_file" => tools::check_file(ide, args, &changed, &baseline, cx).await,
                        "go_to_definition" => tools::definition(ide, args, cx).await,
                        "find_references" => tools::references(ide, args, cx).await,
                        "hover" => tools::hover(ide, args, cx).await,
                        "workspace_symbols" => tools::workspace_symbols(ide, args, cx).await,
                        "code_actions" => tools::code_actions(ide, args, cx).await,
                        "run_app" => tools::run_app(ide, args, cx).await,
                        "app_output" => tools::app_output(ide, args, cx).await,
                        "stop_app" => tools::stop_app(ide, cx).await,
                        "show_file" => tools::show_file(ide, args, cx).await,
                        "http_request" => tools::http_request(ide, args, cx).await,
                        "set_breakpoint" => crate::forge_debug::set_breakpoint(ide, args, cx).await,
                        "remove_breakpoint" => crate::forge_debug::remove_breakpoint(ide, args, cx).await,
                        "start_debugging" => crate::forge_debug::start_debugging(ide, args, cx).await,
                        "debug_step" => crate::forge_debug::debug_step(ide, args, cx).await,
                        "debug_evaluate" => crate::forge_debug::debug_evaluate(ide, args, cx).await,
                        "stop_debugging" => crate::forge_debug::stop_debugging(ide, cx).await,
                        other => Err(format!("Unknown tool: {other}")),
                    };
                    let _ = reply.send(answer);
                })
                .detach();
            }
        }
    }

    /// Answers the question at `ix`: with `answer`, or with what the user wrote when `None`.
    /// An empty answer skips it.
    pub(crate) fn answer_question(&mut self, ix: usize, answer: Option<String>, cx: &mut Context<Self>) {
        let Some(Entry::Question { input, answer: given, reply, .. }) = self.entries.get_mut(ix) else { return };
        if given.is_some() {
            return;
        }
        let text = answer.unwrap_or_else(|| input.read(cx).text(cx).trim().to_string());
        if let Some(reply) = reply.take() {
            let _ = reply.send(if text.is_empty() { Ok("The user skipped the question: decide yourself, and say what you chose.".into()) } else { Ok(format!("The user answered: {text}")) });
        }
        *given = Some(text);
        self.changed(cx);
    }

    /// `notify`: a toast in the workspace, and the message in the thread.
    fn notify_user(&mut self, message: String, cx: &mut Context<Self>) {
        struct AgentNotification;
        let name = self.agent_label();
        if let Some(workspace) = self.workspace.upgrade() {
            let text = format!("{name}: {message}");
            workspace.update(cx, |ws, cx| ws.show_toast(workspace::Toast::new(workspace::notifications::NotificationId::unique::<AgentNotification>(), text), cx));
        }
        self.system(format!("Notified you: {message}"), Color::Accent);
        self.changed(cx);
    }

    /// A call to an extension's tool: run now when it only reads (or the user lets
    /// everything run), else a card the user answers first.
    fn extension_tool(&mut self, name: String, args: Value, reply: oneshot::Sender<crate::forge_mcp::ToolReply>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tool) = forge_ui::agent_tools::agent_tools().get(&name) else {
            let _ = reply.send(Err(format!("Unknown tool: {name}")));
            return;
        };
        if tool.read_only || self.permissions.get().mode == crate::permissions::PermissionMode::SuperUser {
            let root = self.root.clone();
            cx.spawn_in(window, async move |_, _| {
                let _ = reply.send(forge_ui::agent_tools::agent_tools().run(&name, args, root).await);
            })
            .detach();
            return;
        }
        self.entries.push(Entry::ExtensionTool { tool, args, state: crate::forge_tools::EditState::Waiting, reply: Some(reply) });
        self.changed(cx);
    }

    /// Answers the extension tool card at `ix`: runs the tool, or tells the agent no.
    pub(crate) fn answer_extension_tool(&mut self, ix: usize, allow: bool, window: &mut Window, cx: &mut Context<Self>) {
        use crate::forge_tools::EditState;
        let Some(Entry::ExtensionTool { tool, args, state, reply }) = self.entries.get_mut(ix) else { return };
        if !state.is_open() {
            return;
        }
        let Some(reply) = reply.take() else { return };
        if !allow {
            *state = EditState::Declined;
            let _ = reply.send(Err(format!("The user didn't let {} run. Nothing was done.", tool.title)));
            self.changed(cx);
            return;
        }
        *state = EditState::Applying;
        let (name, args, root) = (tool.name.clone(), args.clone(), self.root.clone());
        self.changed(cx);
        cx.spawn_in(window, async move |this, cx| {
            let answer = forge_ui::agent_tools::agent_tools().run(&name, args, root).await;
            this.update(cx, |this, cx| {
                if let Some(Entry::ExtensionTool { state, .. }) = this.entries.get_mut(ix) {
                    *state = match &answer {
                        Ok(_) => EditState::Applied,
                        Err(e) => EditState::Failed(e.clone()),
                    };
                }
                this.changed(cx);
            })
            .ok();
            let _ = reply.send(answer);
        })
        .detach();
    }

    /// Answers the note card at `ix`: adds the note (as the user left it) to the project's
    /// instructions file, or tells the agent no.
    pub(crate) fn answer_remember(&mut self, ix: usize, keep: bool, cx: &mut Context<Self>) {
        use crate::forge_tools::EditState;
        let Some(Entry::Remember { note, file, state, reply }) = self.entries.get_mut(ix) else { return };
        if !state.is_open() {
            return;
        }
        let Some(reply) = reply.take() else { return };
        let text = note.read(cx).text(cx).trim().to_string();
        if !keep || text.is_empty() {
            *state = EditState::Declined;
            let _ = reply.send(Err("The user didn't keep the note.".into()));
            self.changed(cx);
            return;
        }
        *state = EditState::Applying;
        note.update(cx, |e, _| e.set_read_only(true));
        let (file, fs) = (file.clone(), <dyn fs::Fs>::global(cx));
        let shown = file.strip_prefix(&self.root).unwrap_or(&file).to_string_lossy().into_owned();
        self.changed(cx);
        cx.spawn(async move |this, cx| {
            let before = fs.load(&file).await.unwrap_or_default();
            let after = crate::rules::with_note(&before, &text);
            let written = async {
                if let Some(dir) = file.parent() {
                    fs.create_dir(dir).await?;
                }
                fs.atomic_write(file.clone(), after).await
            }
            .await;
            this.update(cx, |this, cx| {
                if let Some(Entry::Remember { state, .. }) = this.entries.get_mut(ix) {
                    *state = match &written {
                        Ok(()) => EditState::Applied,
                        Err(e) => EditState::Failed(format!("{e:#}")),
                    };
                }
                this.changed(cx);
            })
            .ok();
            let _ = reply.send(match written {
                Ok(()) => Ok(format!("Kept in {shown}: agents get it at the start of every new session.")),
                Err(e) => Err(format!("Couldn't write {shown}: {e:#}")),
            });
        })
        .detach();
    }

    /// A language-server edit: a card when the user reviews writes, else made right away.
    fn propose_edit(&mut self, name: String, args: Value, reply: oneshot::Sender<crate::forge_mcp::ToolReply>, window: &mut Window, cx: &mut Context<Self>) {
        let review = self.config.as_ref().is_none_or(|c| c.review_writes) && !self.permissions.get().skips_review();
        let ide = self.ide();
        cx.spawn_in(window, async move |this, cx| {
            let plan = match crate::forge_tools::edit_plan(&name, &ide, &args, cx).await {
                Ok(plan) => plan,
                Err(e) => {
                    let _ = reply.send(Err(e));
                    return;
                }
            };
            if review {
                this.update(cx, |this, cx| {
                    this.entries.push(Entry::LspEdit { plan, state: crate::forge_tools::EditState::Waiting, reply: Some(reply) });
                    this.changed(cx);
                })
                .ok();
                return;
            }
            let applied = crate::forge_tools::apply_edit(&ide, &plan, cx).await;
            let answer = this.update(cx, |this, cx| this.edited(&plan, applied, cx)).unwrap_or_else(|_| Err("The conversation was closed.".into()));
            let _ = reply.send(answer);
        })
        .detach();
    }

    /// Records the files an edit changed (they join the conversation's changes) and says
    /// what happened.
    fn edited(&mut self, plan: &crate::forge_tools::EditPlan, applied: Result<Vec<WriteRecord>, String>, cx: &mut Context<Self>) -> crate::forge_mcp::ToolReply {
        let records = applied?;
        if records.is_empty() {
            return Ok(format!("Nothing to change: {} changed no file.", plan.summary()));
        }
        let files: Vec<String> = records.iter().map(|r| self.ide().show(&r.path)).collect();
        for record in records {
            self.record_write(record, cx);
        }
        Ok(format!("Done ({}), in {}: {}", plan.summary(), crate::push_proposal::plural(files.len(), "file"), files.join(", ")))
    }

    pub(crate) fn apply_edit_proposal(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        use crate::forge_tools::EditState;
        let plan = match self.entries.get_mut(ix) {
            Some(Entry::LspEdit { plan, state, .. }) if state.is_open() => {
                *state = EditState::Applying;
                plan.clone()
            }
            _ => return,
        };
        self.changed(cx);
        let ide = self.ide();
        cx.spawn_in(window, async move |this, cx| {
            let applied = crate::forge_tools::apply_edit(&ide, &plan, cx).await;
            this.update(cx, |this, cx| {
                let answer = this.edited(&plan, applied, cx);
                if let Some(Entry::LspEdit { state, reply, .. }) = this.entries.get_mut(ix) {
                    match &answer {
                        Ok(_) => {
                            *state = EditState::Applied;
                            if let Some(reply) = reply.take() {
                                let _ = reply.send(answer);
                            }
                        }
                        // Stays open: the user can try again or decline.
                        Err(e) => *state = EditState::Failed(e.clone()),
                    }
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn decline_edit(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(Entry::LspEdit { plan, state, reply }) = self.entries.get_mut(ix) {
            if state.is_open() {
                *state = crate::forge_tools::EditState::Declined;
                if let Some(reply) = reply.take() {
                    let _ = reply.send(Err(format!("The user declined: they don't want to {}. Nothing changed.", plan.summary())));
                }
            }
        }
        self.changed(cx);
    }

    /// Shows a commit card for `files` (relative to the thread's folder; every change when
    /// empty).
    fn propose_commit(&mut self, message: String, files: Vec<String>, origin: crate::commit_proposal::Origin, reply: Option<oneshot::Sender<crate::forge_mcp::ToolReply>>, cx: &mut Context<Self>) {
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let resolved = cx.background_spawn(async move { crate::commit_proposal::resolve_files(&root, &files) }).await;
            this.update(cx, |this, cx| {
                match resolved {
                    Ok(files) => {
                        let state = crate::commit_proposal::CommitState::Waiting;
                        this.entries.push(Entry::Commit { message, files, origin, state, reply });
                    }
                    Err(e) => {
                        let e = format!("{e:#}");
                        this.system(format!("The agent proposed a commit, but {e}."), Color::Warning);
                        if let Some(reply) = reply {
                            let _ = reply.send(Err(format!("Nothing to commit: {e}")));
                        }
                    }
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// The commit card at `ix`, while the user can still act on it: its message and files.
    fn open_commit(&self, ix: usize) -> Option<(String, Vec<PathBuf>)> {
        match self.entries.get(ix) {
            Some(Entry::Commit { message, files, state, .. }) if state.is_open() => Some((message.clone(), files.clone())),
            _ => None,
        }
    }

    /// Commits the proposal at `ix` as it is.
    pub(crate) fn commit_proposal(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some((message, files)) = self.open_commit(ix) else { return };
        self.set_commit_state(ix, crate::commit_proposal::CommitState::Committing, cx);
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { crate::commit_proposal::commit(&root, &files, &message) }).await;
            let state = match result {
                Ok((sha, message)) => crate::commit_proposal::CommitState::Committed { sha, message },
                Err(e) => crate::commit_proposal::CommitState::Failed(format!("{e:#}")),
            };
            this.update(cx, |this, cx| this.set_commit_state(ix, state, cx)).ok();
        })
        .detach();
    }

    /// Stages the proposal's files and puts its message in the Git panel, where the user
    /// edits and commits it; the card follows HEAD to see the commit made.
    pub(crate) fn commit_proposal_in_panel(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        use crate::commit_proposal::{CommitState, head, last_commit, stage_only};
        let Some((message, files)) = self.open_commit(ix) else { return };
        let (root, workspace) = (self.root.clone(), self.workspace.clone());
        cx.spawn_in(window, async move |this, cx| {
            let staged = cx.background_spawn({
                let root = root.clone();
                async move { stage_only(&root, &files).map(|()| head(&root)) }
            });
            let before = match staged.await {
                Ok(before) => before,
                Err(e) => {
                    this.update(cx, |this, cx| this.set_commit_state(ix, CommitState::Failed(format!("{e:#}")), cx)).ok();
                    return;
                }
            };
            this.update(cx, |this, cx| this.set_commit_state(ix, CommitState::InPanel { head: before.clone() }, cx)).ok();
            workspace
                .update_in(cx, |ws, window, cx| {
                    let Some(panel) = ws.panel::<git_ui::git_panel::GitPanel>(cx) else { return };
                    panel.read(cx).commit_message_buffer(cx).update(cx, |b, cx| b.set_text(message, cx));
                    ws.focus_panel::<git_ui::git_panel::GitPanel>(window, cx);
                })
                .ok();
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(800)).await;
                let waiting = this.read_with(cx, |t, _| matches!(t.entries.get(ix), Some(Entry::Commit { state: CommitState::InPanel { .. }, .. }))).unwrap_or(false);
                if !waiting {
                    break;
                }
                let root = root.clone();
                let before = before.clone();
                let made = cx.background_spawn(async move { (head(&root) != before).then(|| last_commit(&root).ok()).flatten() }).await;
                if let Some((sha, message)) = made {
                    this.update(cx, |this, cx| this.set_commit_state(ix, CommitState::Committed { sha, message }, cx)).ok();
                    break;
                }
            }
        })
        .detach();
    }

    pub(crate) fn decline_commit_proposal(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.open_commit(ix).is_some() {
            self.set_commit_state(ix, crate::commit_proposal::CommitState::Declined, cx);
        }
    }

    /// Moves the card at `ix` on; a decision (commit made or declined) answers the agent.
    fn set_commit_state(&mut self, ix: usize, new: crate::commit_proposal::CommitState, cx: &mut Context<Self>) {
        use crate::commit_proposal::CommitState;
        if let Some(Entry::Commit { state, reply, .. }) = self.entries.get_mut(ix) {
            *state = new;
            if matches!(state, CommitState::Committed { .. } | CommitState::Declined) {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(crate::commit_proposal::report(state));
                }
            }
        }
        self.changed(cx);
    }

    /// Shows a push card for the thread's current branch (to `remote`, or its upstream).
    fn propose_push(&mut self, remote: Option<String>, origin: crate::commit_proposal::Origin, reply: Option<oneshot::Sender<crate::forge_mcp::ToolReply>>, cx: &mut Context<Self>) {
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let plan = cx.background_spawn(async move { crate::push_proposal::plan(&root, remote.as_deref()) }).await;
            this.update(cx, |this, cx| {
                match plan {
                    Ok(plan) => {
                        let state = crate::push_proposal::PushState::Waiting;
                        this.entries.push(Entry::Push { plan, origin, state, reply });
                    }
                    Err(e) => {
                        let e = format!("{e:#}");
                        this.system(format!("The agent proposed a push, but {e}."), Color::Warning);
                        if let Some(reply) = reply {
                            let _ = reply.send(Err(format!("Nothing to push: {e}")));
                        }
                    }
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Pushes as the card at `ix` says, through Zed's repository: credentials are asked as
    /// in the Git panel.
    pub(crate) fn push_proposal(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        use crate::push_proposal::PushState;
        let plan = match self.entries.get(ix) {
            Some(Entry::Push { plan, state, .. }) if state.is_open() => plan.clone(),
            _ => return,
        };
        // The repository the thread's folder is in (a worktree's commits are in it too).
        let repo = self.project.upgrade().and_then(|project| {
            let store = project.read(cx).git_store().read(cx);
            store.repositories().values().filter(|r| self.root.starts_with(&r.read(cx).work_directory_abs_path)).max_by_key(|r| r.read(cx).work_directory_abs_path.as_os_str().len()).cloned()
        });
        let Some(repo) = repo else {
            self.set_push_state(ix, PushState::Failed("Forge doesn't know this folder's repository.".into()), cx);
            return;
        };
        self.set_push_state(ix, PushState::Pushing, cx);
        let (workspace, window_handle) = (self.workspace.clone(), window.window_handle());
        cx.spawn_in(window, async move |this, cx| {
            let operation: gpui::SharedString = format!("git push {} {}", plan.remote, plan.branch).into();
            let askpass = askpass::AskPassDelegate::new_with_cancellation(cx, move |prompt, tx, cancellation, cx| {
                window_handle
                    .update(cx, |_, window, cx| {
                        workspace
                            .update(cx, |ws, cx| ws.toggle_modal(window, cx, |window, cx| git_ui_core::askpass_modal::AskPassModal::new(operation.clone(), prompt.into(), tx, cancellation, window, cx)))
                            .ok();
                    })
                    .ok();
            });
            let options = plan.set_upstream.then_some(git::repository::PushOptions::SetUpstream);
            let pushed = repo.update(cx, |repo, cx| repo.push(plan.branch.clone().into(), plan.remote_branch.clone().into(), plan.remote.clone().into(), options, askpass, cx));
            let state = match pushed.await {
                Ok(Ok(_)) => PushState::Pushed,
                Ok(Err(e)) => PushState::Failed(format!("{e:#}")),
                Err(_) => PushState::Failed("The push was canceled.".into()),
            };
            this.update(cx, |this, cx| this.set_push_state(ix, state, cx)).ok();
        })
        .detach();
    }

    pub(crate) fn decline_push_proposal(&mut self, ix: usize, cx: &mut Context<Self>) {
        if matches!(self.entries.get(ix), Some(Entry::Push { state, .. }) if state.is_open()) {
            self.set_push_state(ix, crate::push_proposal::PushState::Declined, cx);
        }
    }

    /// Moves the card at `ix` on; a decision (pushed or declined) answers the agent.
    fn set_push_state(&mut self, ix: usize, new: crate::push_proposal::PushState, cx: &mut Context<Self>) {
        use crate::push_proposal::PushState;
        if let Some(Entry::Push { plan, state, reply, .. }) = self.entries.get_mut(ix) {
            *state = new;
            if matches!(state, PushState::Pushed | PushState::Declined) {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(crate::push_proposal::report(plan, state));
                }
            }
        }
        self.changed(cx);
    }

    /// Records the files a finished edit tool call changed on disk (see `apply_update`).
    fn record_tool_edits(&mut self, tool_call_id: &str, cx: &mut Context<Self>) {
        // Each file's diff: before and after, when the tool call carried whole files.
        let edits: Vec<(PathBuf, Option<String>, String)> = self
            .entries
            .iter()
            .find_map(|e| match e {
                Entry::Tool { id, diffs, .. } if id == tool_call_id => Some(diffs.iter().map(|d| (PathBuf::from(&d.edit.path), d.edit.old_text.clone(), d.edit.new_text.clone())).collect()),
                _ => None,
            })
            .unwrap_or_default();
        if edits.is_empty() {
            return;
        }
        let fs = <dyn fs::Fs>::global(cx);
        cx.spawn(async move |this, cx| {
            let mut now = Vec::new();
            for (path, old_text, new_text) in edits {
                now.push((fs.load(&path).await.ok(), path, old_text, new_text));
            }
            this.update(cx, |this, cx| {
                for (on_disk, path, old_text, new_text) in now {
                    // Deleted or unreadable: nothing to show.
                    let Some(on_disk) = on_disk else { continue };
                    // Only whole-file diffs say what the file was: a diff of the edited
                    // snippet doesn't end as the file on disk does.
                    if !same_text(&new_text, &on_disk) || old_text.as_deref() == Some(on_disk.as_str()) {
                        continue;
                    }
                    // Already recorded with this content (it went through `ProjectFs`).
                    if this.changes.iter().any(|c| c.path == path && c.current == on_disk) {
                        continue;
                    }
                    this.record_write(WriteRecord { path, old_text, new_text: on_disk, problems_before: None }, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// The project's instructions files (agents.json `instructions_files`).
    pub(crate) fn instructions_files(&self) -> Vec<String> {
        self.config.as_ref().map(|c| c.instructions_files.clone()).unwrap_or_else(crate::config::default_instructions_files)
    }

    /// It works in its own git worktree.
    pub(crate) fn in_worktree(&self) -> bool {
        self.worktree.is_some()
    }

    pub(crate) fn answer_permission(&mut self, request_id: String, option: Option<(String, String)>, cx: &mut Context<Self>) {
        let Some((agent, _)) = self.connected.clone() else { return };
        // The agent's `git commit` or `git push` becomes a proposal: it is told no, the user
        // makes it here.
        if option.as_ref().is_some_and(|(id, _)| id == COMMIT_IN_FORGE || id == PUSH_IN_FORGE) {
            let reject = self.entries.iter().find_map(|e| match e {
                Entry::Permission { request_id: r, options, .. } if *r == request_id => options.iter().find(|o| o.kind.starts_with("reject")).map(|o| o.id.clone()),
                _ => None,
            });
            let origin = crate::commit_proposal::Origin::Command;
            match self.git_redirects.remove(&request_id) {
                Some(GitRedirect::Commit(message)) => {
                    self.answer_permission(request_id, reject.map(|id| (id, "Moved to a commit you make from Forge".to_string())), cx);
                    self.propose_commit(message, vec![], origin, None, cx);
                }
                Some(GitRedirect::Push) => {
                    self.answer_permission(request_id, reject.map(|id| (id, "Moved to a push you make from Forge".to_string())), cx);
                    self.propose_push(None, origin, None, cx);
                }
                None => {}
            }
            return;
        }
        self.git_redirects.remove(&request_id);
        let allowed = option.as_ref().is_some_and(|(id, _)| {
            self.entries.iter().any(|e| matches!(e, Entry::Permission { request_id: r, options, .. } if *r == request_id && options.iter().any(|o| &o.id == id && o.allow)))
        });
        for entry in &mut self.entries {
            if let Entry::Permission { request_id: r, resolved, diffs, .. } = entry {
                if *r == request_id {
                    *resolved = Some(option.as_ref().map(|(_, name)| name.clone()).unwrap_or_else(|| "Cancelled".into()));
                    // The user has seen and approved these exact diffs: don't review the writes again.
                    if allowed {
                        for d in diffs.iter() {
                            self.approved_edits.approve(PathBuf::from(&d.edit.path), d.edit.new_text.clone());
                        }
                    }
                }
            }
        }
        let outcome = match option {
            Some((option_id, _)) => PermissionOutcome::Selected { option_id },
            None => PermissionOutcome::Cancelled,
        };
        let rt = self.runtime.clone();
        Tokio::spawn(cx, async move { rt.respond_permission(&agent, &request_id, outcome).await }).detach();
        self.changed(cx);
    }

    fn handle_event(&mut self, ev: IdeEvent, window: &mut Window, cx: &mut Context<Self>) {
        let IdeEvent::Agent(ev) = ev;
        let ours = |id: &str| self.connected.as_ref().is_some_and(|(a, _)| a == id) || self.status == Status::Connecting;
        match ev {
            AgentEvent::Stderr { agent_id, line } if ours(&agent_id) => {
                self.stderr.push_back(line);
                if self.stderr.len() > 200 {
                    self.stderr.pop_front();
                }
                return;
            }
            AgentEvent::Exited { agent_id, code } if ours(&agent_id) => {
                self.persist(cx);
                self.connected = None;
                self.status = Status::Disconnected;
                self.system(format!("Agent exited{}.", code.map(|c| format!(" with code {c}")).unwrap_or_default()), Color::Muted);
            }
            AgentEvent::TerminalCreated { agent_id, terminal_id } if ours(&agent_id) => {
                if let Some(terminal) = self.terminals.get(&terminal_id) {
                    let (workspace, project) = (self.workspace.clone(), self.project.clone());
                    let view = cx.new(|cx| {
                        let mut view = TerminalView::new(terminal, workspace, None, project, window, cx);
                        view.set_embedded_mode(Some(15), cx);
                        view
                    });
                    self.terminal_views.insert(terminal_id, view);
                }
            }
            AgentEvent::PermissionRequested { agent_id, request_id, params } if ours(&agent_id) => {
                let title = params.pointer("/toolCall/title").and_then(Value::as_str).unwrap_or("The agent wants to run a tool").to_string();
                let mut options: Vec<PermissionOption> = params
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|xs| {
                        xs.iter()
                            .filter_map(|o| {
                                let kind = o.get("kind").and_then(Value::as_str).unwrap_or_default().to_string();
                                Some(PermissionOption {
                                    id: o.get("optionId")?.as_str()?.to_string(),
                                    name: o.get("name")?.as_str()?.to_string(),
                                    allow: kind.starts_with("allow"),
                                    kind,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let diffs = params.get("toolCall").map(Edit::all_from_tool_call).unwrap_or_default();
                let diffs = self.diff_views(diffs, window, cx);
                let decision = crate::permissions::decide(&self.permissions.get(), params.get("toolCall").unwrap_or(&Value::Null), &self.roots(cx));
                let auto = match decision {
                    crate::permissions::Decision::Allow(reason) => params.get("options").and_then(crate::permissions::allow_option).map(|id| (id, reason)),
                    crate::permissions::Decision::Ask => None,
                };
                let tool_call_id = params.pointer("/toolCall/toolCallId").and_then(Value::as_str).map(str::to_string);
                // A `git commit` or `git push`: offer to make it from Forge instead (the commit
                // first when the command does both).
                let command = params.get("toolCall").and_then(crate::permissions::tool_command).unwrap_or_default();
                let redirect = match crate::permissions::git_commit(&command) {
                    Some(message) => Some((GitRedirect::Commit(message), COMMIT_IN_FORGE, "Commit from Forge instead")),
                    None if crate::permissions::git_push(&command) => Some((GitRedirect::Push, PUSH_IN_FORGE, "Push from Forge instead")),
                    None => None,
                };
                if let Some((redirect, id, name)) = redirect {
                    options.insert(0, PermissionOption { id: id.into(), name: name.into(), allow: false, kind: id.into() });
                    self.git_redirects.insert(request_id.clone(), redirect);
                }
                self.entries.push(Entry::Permission { request_id: request_id.clone(), tool_call_id, title, options, resolved: None, diffs });
                // The policy allows it: answer as the user would, and say so in the thread.
                if let Some((option_id, reason)) = auto {
                    self.answer_permission(request_id, Some((option_id, format!("Allowed automatically: {reason}"))), cx);
                }
            }
            AgentEvent::Notification { agent_id, method, params } if ours(&agent_id) && method == "_auth/status_update" => {
                // Claude's adapter reports its login state; tell the user before a prompt fails.
                if params.pointer("/authStatus/kind").and_then(Value::as_str) == Some("none") && !self.warned_auth {
                    self.warned_auth = true;
                    self.show_auth("The agent is not signed in.", cx);
                }
            }
            AgentEvent::Notification { agent_id, method, params }
                if ours(&agent_id) && method == "session/update" && params.get("sessionId").and_then(Value::as_str).is_some_and(|s| self.side_sessions.contains_key(s)) =>
            {
                if params.pointer("/update/sessionUpdate").and_then(Value::as_str) == Some("agent_message_chunk") {
                    let session = params.get("sessionId").and_then(Value::as_str).unwrap_or_default();
                    let text = params.pointer("/update/content/text").and_then(Value::as_str).unwrap_or_default();
                    if let Some(reply) = self.side_sessions.get_mut(session) {
                        reply.push_str(text);
                    }
                }
                return;
            }
            AgentEvent::Notification { agent_id, method, params } if ours(&agent_id) && method == "session/update" && self.usage_session.is_some() && params.get("sessionId").and_then(Value::as_str) == self.usage_session.as_deref() => {
                // The hidden `/usage` session: collect its answer, show nothing.
                if params.pointer("/update/sessionUpdate").and_then(Value::as_str) == Some("agent_message_chunk") {
                    let text = params.pointer("/update/content/text").and_then(Value::as_str).unwrap_or_default();
                    self.usage_reply.get_or_insert_default().push_str(text);
                }
                return;
            }
            AgentEvent::Notification { agent_id, method, params } if ours(&agent_id) && method == "session/update" => {
                let session_matches = self.connected.as_ref().is_none_or(|(_, s)| params.get("sessionId").and_then(Value::as_str) == Some(s.as_str()));
                if session_matches && !self.replaying {
                    self.apply_update(params.get("update").cloned().unwrap_or_default(), window, cx);
                }
            }
            _ => return,
        }
        self.changed(cx);
    }

    pub(crate) fn apply_update(&mut self, u: Value, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(mut location) = tool_location(&u) {
            // Updates often leave the kind out: it is the tool call's.
            if location.kind.is_none() {
                let id = u.get("toolCallId").and_then(Value::as_str).unwrap_or_default();
                location.kind = self.entries.iter().find_map(|e| match e {
                    Entry::Tool { id: tid, kind, .. } if tid == id && !kind.is_empty() => Some(kind.clone()),
                    _ => None,
                });
            }
            self.location = Some(location);
        }
        let text = u.pointer("/content/text").and_then(Value::as_str).unwrap_or_default().to_string();
        let str_of = |k: &str| u.get(k).and_then(Value::as_str).map(str::to_string);
        match u.get("sessionUpdate").and_then(Value::as_str).unwrap_or_default() {
            "user_message_chunk" => match self.entries.last_mut() {
                Some(Entry::User(t, _)) => t.push_str(&text),
                _ => self.entries.push(Entry::User(text, vec![])),
            },
            kind @ ("agent_message_chunk" | "agent_thought_chunk") => {
                let thought = kind == "agent_thought_chunk";
                match self.entries.last() {
                    Some(Entry::Agent(md)) if !thought => md.update(cx, |m, cx| m.append(&text, cx)),
                    Some(Entry::Thought(md)) if thought => md.update(cx, |m, cx| m.append(&text, cx)),
                    _ => {
                        let languages = self.languages.clone();
                        let md = cx.new(|cx| Markdown::new(text.into(), Some(languages), None, cx));
                        self.entries.push(if thought { Entry::Thought(md) } else { Entry::Agent(md) });
                    }
                }
            }
            "tool_call" => {
                let diffs = self.diff_views(Edit::all_from_tool_call(&u), window, cx);
                // A command the agent runs itself: its output goes to a terminal shown in the
                // tool call, as for the commands it runs in Forge's terminals.
                let display = u.pointer("/_meta/terminal_info/terminal_id").and_then(Value::as_str).map(str::to_string);
                if let Some(id) = &display {
                    self.add_display_terminal(id, window, cx);
                }
                let (title, command) = tool_title(&u, &self.root);
                self.entries.push(Entry::Tool {
                    id: str_of("toolCallId").unwrap_or_default(),
                    title: title.unwrap_or_else(|| "Tool call".into()),
                    command,
                    kind: str_of("kind").unwrap_or_default(),
                    status: str_of("status").unwrap_or_else(|| "pending".into()),
                    detail: tool_detail(&u),
                    terminal: tool_terminal(&u).or(display),
                    diffs,
                })
            }
            "tool_call_update" => {
                let id = str_of("toolCallId").unwrap_or_default();
                if let Some(output) = u.pointer("/_meta/terminal_output") {
                    let terminal = output.get("terminal_id").and_then(Value::as_str).and_then(|t| self.display_terminals.get(t));
                    if let (Some(terminal), Some(data)) = (terminal, output.get("data").and_then(Value::as_str)) {
                        terminal.update(cx, |t, cx| t.write_output(data.as_bytes(), cx));
                    }
                }
                let edits = Edit::all_from_tool_call(&u);
                let mut new_diffs = (!edits.is_empty()).then(|| self.diff_views(edits, window, cx));
                let (new_title, new_command) = tool_title(&u, &self.root);
                let completed = str_of("status").as_deref() == Some("completed");
                for entry in &mut self.entries {
                    if let Entry::Tool { id: tid, title, status, detail, terminal, diffs, command, .. } = entry {
                        if *tid == id {
                            if new_command.is_some() {
                                *command = new_command.clone();
                            }
                            if let Some(t) = tool_terminal(&u) {
                                *terminal = Some(t);
                            }
                            if let Some(d) = new_diffs.take() {
                                *diffs = d;
                            }
                            if let Some(s) = str_of("status") {
                                *status = s;
                            }
                            if let Some(t) = new_title.clone() {
                                *title = t;
                            }
                            if let Some(d) = tool_detail(&u) {
                                *detail = Some(d);
                            }
                        }
                    }
                }
                // Agents that write files themselves (Claude Code's adapter does) bypass
                // `ProjectFs`: their finished edits join the changes here, from the file
                // before (the tool call's diff) and as it is now on disk.
                if completed {
                    self.record_tool_edits(&id, cx);
                }
            }
            "usage_update" => self.usage.apply_update(&u),
            "current_mode_update" => {
                self.current_mode = str_of("currentModeId");
                // Keep the mode setting (if the agent also lists it there) in step.
                if let Some(mode) = self.current_mode.clone() {
                    for option in self.config_options.iter_mut().filter(|o| o.category.as_deref() == Some("mode")) {
                        if let ConfigValue::Select { current, .. } = &mut option.value {
                            *current = mode.clone();
                        }
                    }
                }
            }
            "config_option_update" => self.config_options = parse_config_options(u.get("configOptions")),
            "available_commands_update" => {
                self.agent_commands = u
                    .get("availableCommands")
                    .and_then(Value::as_array)
                    .map(|xs| {
                        xs.iter()
                            .filter_map(|c| {
                                Some(AgentCommand {
                                    name: c.get("name")?.as_str()?.trim_start_matches('/').to_string(),
                                    description: c.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
                                    hint: c.get("input").and_then(|i| i.get("hint")).and_then(Value::as_str).map(str::to_string),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if self.status == Status::Ready {
                    self.refresh_plan_usage(cx);
                }
            }
            "plan" => {
                let items = u
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|xs| {
                        xs.iter()
                            .map(|e| (e.get("content").and_then(Value::as_str).unwrap_or_default().to_string(), e.get("status").and_then(Value::as_str).unwrap_or("pending").to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                self.entries.retain(|e| !matches!(e, Entry::Plan(_)));
                self.entries.push(Entry::Plan(items));
            }
            _ => {}
        }
    }
}

/// The last of a tool call's `locations`; lines are zero-based, as Zed reads them.
fn tool_location(u: &Value) -> Option<AgentLocation> {
    let location = u.get("locations")?.as_array()?.last()?;
    Some(AgentLocation {
        path: PathBuf::from(location.get("path")?.as_str()?),
        line: location.get("line").and_then(Value::as_u64).map(|l| l as u32),
        kind: u.get("kind").and_then(Value::as_str).map(str::to_string),
    })
}

/// A tool call's title, and for a command, its command line when the title is the agent's
/// description of it (Claude Code's `rawInput.description`). Command lines lose the
/// `cd <folder> &&` agents put first, and say `.` for the thread's folder.
fn tool_title(u: &Value, root: &std::path::Path) -> (Option<String>, Option<String>) {
    let title = u.get("title").and_then(Value::as_str).map(str::to_string);
    let input = u.get("rawInput");
    // Forge's own tools: their title, and what they were asked.
    let name = u.pointer("/_meta/claudeCode/toolName").or_else(|| u.get("name")).and_then(Value::as_str).or(title.as_deref());
    if let Some(forge_title) = name.and_then(crate::forge_mcp::tool_title).filter(|_| name.is_some_and(|n| n.starts_with("mcp__"))) {
        return (Some(forge_title), input.and_then(crate::forge_mcp::tool_summary));
    }
    let command = input.and_then(|i| i.get("command")).and_then(Value::as_str).map(str::to_string).or_else(|| (u.get("kind").and_then(Value::as_str) == Some("execute")).then(|| title.clone()).flatten());
    let Some(command) = command.filter(|c| !c.trim().is_empty()) else { return (title, None) };
    let command = tidy_command(&command, root);
    match input.and_then(|i| i.get("description")).and_then(Value::as_str).map(str::trim).filter(|d| !d.is_empty()) {
        Some(description) => (Some(description.to_string()), Some(command)),
        None => (Some(command), None),
    }
}

/// `command` as the user reads it: see [`tool_title`].
fn tidy_command(command: &str, root: &std::path::Path) -> String {
    let mut rest = command.trim();
    // `cd <dir> && …` / `cd <dir>; …`, maybe more than one.
    while let Some(after_cd) = rest.strip_prefix("cd ") {
        let cut = [after_cd.find("&&").map(|i| (i, 2)), after_cd.find(';').map(|i| (i, 1))].into_iter().flatten().min_by_key(|(i, _)| *i);
        let Some((i, len)) = cut else { break };
        rest = after_cd[i + len..].trim_start();
    }
    let root = root.to_string_lossy();
    if root.len() <= 1 {
        return rest.to_string();
    }
    rest.replace(&format!("{root}/"), "").replace(root.as_ref(), ".")
}

/// The same text but for trailing whitespace (agents may trim it as they write).
fn same_text(a: &str, b: &str) -> bool {
    a.lines().map(str::trim_end).eq(b.lines().map(str::trim_end))
}

/// The terminal a tool call embeds (`{"type": "terminal", "terminalId"}` content).
fn tool_terminal(u: &Value) -> Option<String> {
    u.get("content")?.as_array()?.iter().find_map(|c| (c.get("type")?.as_str()? == "terminal").then(|| c.get("terminalId")?.as_str().map(str::to_string)).flatten())
}

/// Lines of a tool call's text shown at most (the last ones: a command's ending matters).
const DETAIL_LINES: usize = 200;

/// What a tool call's content says: the diff target, or its first text block (a command's
/// output, often in a code fence, shown without it).
fn tool_detail(u: &Value) -> Option<String> {
    let content = u.get("content")?.as_array()?;
    content.iter().find_map(|c| match c.get("type").and_then(Value::as_str)? {
        "diff" => Some(format!("edit {}", c.get("path")?.as_str()?)),
        "content" => {
            let text = unfence(c.pointer("/content/text")?.as_str()?);
            let lines: Vec<&str> = text.lines().collect();
            let shown = match lines.len().checked_sub(DETAIL_LINES) {
                Some(cut) if cut > 0 => format!("… {cut} more lines\n{}", lines[cut..].join("\n")),
                _ => text.to_string(),
            };
            (!shown.trim().is_empty()).then_some(shown)
        }
        _ => None,
    })
}

/// `text` without the code fence around it (```` ```console ```` … ```` ``` ````), if it has one.
fn unfence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else { return text };
    let Some(body_start) = rest.find('\n') else { return "" };
    let body = &rest[body_start + 1..];
    body.trim_end().strip_suffix("```").unwrap_or(body).trim_end_matches('\n')
}


#[cfg(test)]
mod detail_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn titles_commands_by_their_description() {
        let root = std::path::Path::new("/work/app");
        let call = json!({ "kind": "execute", "title": "cd /work/app && git status", "rawInput": { "command": "cd /work/app && git status", "description": "Show working tree status" } });
        assert_eq!(tool_title(&call, root), (Some("Show working tree status".into()), Some("git status".into())));
        let bare = json!({ "kind": "execute", "title": "cd \"/work/app\"; ls /work/app/src /tmp" });
        assert_eq!(tool_title(&bare, root), (Some("ls src /tmp".into()), None));
        let read = json!({ "kind": "read", "title": "Read main.rs" });
        assert_eq!(tool_title(&read, root), (Some("Read main.rs".into()), None));
        let forge = json!({ "kind": "other", "title": "mcp__forge__find_references", "rawInput": { "path": "src/a.rs", "line": 12, "symbol": "parse" } });
        assert_eq!(tool_title(&forge, root), (Some("Find references".into()), Some("src/a.rs:12 parse".into())));
        assert_eq!(tidy_command("cd sub", root), "cd sub", "a lone cd stays");
        assert_eq!(tidy_command("cd /work/app && cd src && cargo test", root), "cargo test");
    }

    fn text(t: &str) -> Value {
        json!({ "content": [{ "type": "content", "content": { "type": "text", "text": t } }] })
    }

    #[test]
    fn shows_command_output_without_its_fence() {
        assert_eq!(tool_detail(&text("```console\nline 1\nline 2\n```")).as_deref(), Some("line 1\nline 2"));
        assert_eq!(tool_detail(&text("```\nok\n```\n")).as_deref(), Some("ok"));
        assert_eq!(tool_detail(&text("plain\ntext")).as_deref(), Some("plain\ntext"));
        assert_eq!(tool_detail(&text("```console\n```")), None, "nothing to show");
        let long: String = (1..=250).map(|i| format!("{i}\n")).collect();
        let shown = tool_detail(&text(&format!("```console\n{long}```"))).unwrap();
        assert!(shown.starts_with("… 50 more lines\n51\n") && shown.ends_with("\n250"), "{shown}");
        assert_eq!(tool_detail(&json!({ "content": [{ "type": "diff", "path": "/a.rs" }] })).as_deref(), Some("edit /a.rs"));
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
