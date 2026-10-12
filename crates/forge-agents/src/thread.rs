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
use ide_api::{AgentEvent, AgentService as _, AgentSpec, IdeEvent, PermissionOutcome, TerminalRequest, TerminalHost as _};
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

mod checks;
mod handoff;
mod permission_requests;
mod persistence;
mod proposals;
mod review;
mod session_config;
mod side_sessions;
mod sign_in;
mod tool_dispatch;
mod tool_display;
mod worktree_ops;

use tool_display::{tool_detail, tool_location, tool_terminal, tool_title};

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
    /// `signoff`: whether it is signed off, as `signer` ("Name <email>", when known).
    Commit { message: String, files: Vec<PathBuf>, signoff: bool, signer: Option<String>, origin: crate::commit_proposal::Origin, state: crate::commit_proposal::CommitState, reply: Option<oneshot::Sender<crate::forge_mcp::ToolReply>> },
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
    /// Settings or key bindings the agent wants to change (`change_settings`,
    /// `change_keybinding`), for the user to apply or discard.
    Config { changes: Vec<crate::configure::ConfigChange>, state: crate::forge_tools::EditState, reply: Option<oneshot::Sender<crate::forge_mcp::ToolReply>> },
    /// A command the agent runs with `run_command`: in a terminal of Forge's, embedded here,
    /// whose output the user watches as it comes. `terminal` once it started; `exit` once done.
    Command { command: String, cwd: PathBuf, timeout: std::time::Duration, state: crate::forge_tools::EditState, terminal: Option<String>, exit: Option<String>, reply: Option<oneshot::Sender<crate::forge_mcp::ToolReply>> },
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

/// Bytes of a `run_command` command's output kept for the agent (its end).
const MAX_COMMAND_OUTPUT: usize = 30_000;

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
    /// The turn's files put back as they were before it (*Undo turn*).
    pub undone: std::collections::BTreeSet<PathBuf>,
    pub started: std::time::Instant,
    /// How long the turn took, once it ended.
    pub duration: Option<std::time::Duration>,
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
    /// Each changed file's (added, removed) lines, counted when the files change
    /// (`changes_updated`): views show them on every repaint, and counting reads whole files.
    change_stats: HashMap<PathBuf, (usize, usize)>,
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
    /// Runs `run_command`'s commands, in terminals the thread embeds (the agent's own
    /// `terminal/*` requests go through it too).
    terminal_host: Arc<ZedTerminals>,
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
    /// What the agent that had the thread before did, for the next message to the one that
    /// takes over (see [`Thread::hand_off`]).
    handoff_context: Option<String>,
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
        let runtime = Arc::new(AcpRuntime::new(bus, fs).with_terminals(terminal_host.clone()).with_terminal_auth().with_display_terminals().with_session_meta(crate::forge_mcp::session_meta(forge_mcp.as_ref().map(|m| m.url()))));

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
            change_stats: HashMap::new(),
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
            terminal_host,
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
            handoff_context: None,
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
        self.entries.iter().filter(|e| matches!(e, Entry::Review { reply: Some(_), .. } | Entry::Permission { resolved: None, .. } | Entry::Commit { reply: Some(_), .. } | Entry::Push { reply: Some(_), .. } | Entry::LspEdit { reply: Some(_), .. } | Entry::ExtensionTool { reply: Some(_), .. } | Entry::Remember { reply: Some(_), .. } | Entry::Config { reply: Some(_), .. } | Entry::Command { state: crate::forge_tools::EditState::Waiting, .. } | Entry::Question { answer: None, .. })).count()
    }

    /// Where the agent last read or wrote.
    pub fn location(&self) -> Option<&AgentLocation> {
        self.location.as_ref()
    }

    /// The pending write to `path`, if any: its entry index and (+, −) counts.
    pub fn pending_review_for(&self, path: &std::path::Path) -> Option<(usize, usize, usize)> {
        self.entries.iter().enumerate().find_map(|(ix, e)| match e {
            Entry::Review { diff, reply: Some(_), .. } if std::path::Path::new(&diff.edit.path) == path => {
                let (added, removed) = diff.stats;
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
            let (added, removed) = diff.stats;
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
            // What Forge adds for the agents it knows (see `agent_setup`).
            if let Ok(exe) = std::env::current_exe() {
                crate::agent_setup::prepare(&mut spec, &exe, &paths::data_dir().join("agents"));
            }
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
        self.stop_agent(cx);
        self.status = Status::Disconnected;
        self.changed(cx);
    }

    /// Stops the agent's process (it gets a moment to exit on its own).
    fn stop_agent(&mut self, cx: &mut Context<Self>) {
        if let Some((id, _)) = self.connected.take() {
            let rt = self.runtime.clone();
            Tokio::spawn(cx, async move {
                if let Err(e) = rt.stop(&id).await {
                    log::warn!("couldn't stop {id}: {e}");
                }
            })
            .detach();
        }
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
        let kept = crate::agent_ignore::AgentIgnore::load(&self.root);
        let mut withheld = Vec::new();
        for mention in resolve_mentions(&sent, &self.root) {
            match mention {
                // Kept from agents: not sent, and the thread says so.
                Mention::Path { path, .. } if kept.denies(&path) => withheld.push(path.strip_prefix(&self.root).unwrap_or(&path).display().to_string()),
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
        if !withheld.is_empty() {
            self.system(format!("Not sent to the agent: {} (kept from agents: secrets, or listed in {}).", withheld.join(", "), crate::agent_ignore::files_text()), Color::Warning);
        }
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
        if let Some(summary) = self.handoff_context.take() {
            blocks.push(json!({ "type": "text", "text": summary }));
            labels.push("what the previous agent did".into());
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
            Tokio::spawn(cx, async move {
                if let Err(e) = rt.cancel(&agent, &session).await {
                    log::warn!("couldn't cancel {agent}'s turn: {e}");
                }
            })
            .detach();
        }
        self.close_waiting_calls();
        self.changed(cx);
    }

    fn diff_views(&self, edits: Vec<Edit>, window: &mut Window, cx: &mut Context<Self>) -> Vec<DiffView> {
        edits.into_iter().map(|e| DiffView::new(e, self.languages.clone(), window, cx)).collect()
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
            Entry::Config { changes, reply: Some(_), .. } => Some(format!("change {}", crate::configure::summary(changes))),
            Entry::Command { command, state: crate::forge_tools::EditState::Waiting, .. } => Some(format!("run `{command}`")),
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

    /// The project's instructions files (agents.json `instructions_files`).
    pub(crate) fn instructions_files(&self) -> Vec<String> {
        self.config.as_ref().map(|c| c.instructions_files.clone()).unwrap_or_else(crate::config::default_instructions_files)
    }

    /// It works in its own git worktree.
    pub(crate) fn in_worktree(&self) -> bool {
        self.worktree.is_some()
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
            AgentEvent::PermissionRequested { agent_id, request_id, params }
                if ours(&agent_id) && params.get("sessionId").and_then(Value::as_str).is_some_and(|s| self.side_sessions.contains_key(s) || self.usage_session.as_deref() == Some(s)) =>
            {
                // Hidden sessions only answer questions: nothing to ask the user about.
                self.answer_permission(request_id, None, cx);
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
                let kept = crate::agent_ignore::AgentIgnore::load(&self.root);
                let decision = crate::permissions::decide(&self.permissions.get(), params.get("toolCall").unwrap_or(&Value::Null), &self.roots(cx), &kept);
                let auto = match decision {
                    crate::permissions::Decision::Allow(reason) => params.get("options").and_then(crate::permissions::allow_option).map(|id| (id, format!("Allowed automatically: {reason}"))),
                    crate::permissions::Decision::Deny(reason) => options.iter().find(|o| o.kind.starts_with("reject")).map(|o| (o.id.clone(), reason)),
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
                    self.answer_permission(request_id, Some((option_id, reason)), cx);
                }
            }
            AgentEvent::PermissionClosed { agent_id, request_id, reason } if ours(&agent_id) => {
                self.git_redirects.remove(&request_id);
                for entry in &mut self.entries {
                    if let Entry::Permission { request_id: r, resolved, .. } = entry
                        && *r == request_id
                        && resolved.is_none()
                    {
                        *resolved = Some(reason.clone());
                    }
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
                // Whole output (`terminal_output`) or the next piece of it (`terminal_output_delta`):
                // either way it is what comes next in the terminal.
                if let Some(output) = u.pointer("/_meta/terminal_output").or_else(|| u.pointer("/_meta/terminal_output_delta")) {
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
                set_plan(&mut self.entries, items);
            }
            _ => {}
        }
    }
}

/// What a file was before a tool call's `diffs` (before → after, in order) made it
/// `on_disk`: a whole-file diff says it (`None` for a new file); snippet diffs (Claude
/// Code's Edit) are undone on the file, last first. `None` when they don't add up.
pub(crate) fn file_before(on_disk: &str, diffs: &[(Option<String>, String)]) -> Option<Option<String>> {
    if let Some((old_text, _)) = diffs.last().filter(|(_, new_text)| same_text(new_text, on_disk)) {
        return Some(old_text.clone());
    }
    let mut text = on_disk.to_string();
    for (old_text, new_text) in diffs.iter().rev() {
        let old_text = old_text.as_deref()?;
        if new_text.is_empty() || !text.contains(new_text.as_str()) {
            return None;
        }
        // An edit that replaced every occurrence left the new snippet in each of their places.
        text = if text.matches(new_text.as_str()).count() == 1 { text.replacen(new_text.as_str(), old_text, 1) } else { text.replace(new_text.as_str(), old_text) };
    }
    Some(Some(text))
}

/// The same text but for trailing whitespace (agents may trim it as they write).
fn same_text(a: &str, b: &str) -> bool {
    a.lines().map(str::trim_end).eq(b.lines().map(str::trim_end))
}

/// The agent's plan for this turn: updated where it is, so the entries after it keep their
/// places (cards waiting on the user are found by them); a new turn gets a plan of its own.
fn set_plan(entries: &mut Vec<Entry>, items: Vec<(String, String)>) {
    let turn_start = entries.iter().rposition(|e| matches!(e, Entry::User(..))).map_or(0, |ix| ix + 1);
    match entries[turn_start..].iter().position(|e| matches!(e, Entry::Plan(_))) {
        Some(ix) => entries[turn_start + ix] = Entry::Plan(items),
        None => entries.push(Entry::Plan(items)),
    }
}

#[cfg(test)]
mod plan_tests {
    use super::{Entry, set_plan};

    fn plan(step: &str) -> Vec<(String, String)> {
        vec![(step.into(), "pending".into())]
    }

    #[test]
    fn updates_the_turns_plan_in_place() {
        let mut entries = vec![Entry::User("one".into(), vec![]), Entry::System("a".into(), ui::Color::Muted)];
        set_plan(&mut entries, plan("first"));
        entries.push(Entry::System("b".into(), ui::Color::Muted));
        set_plan(&mut entries, plan("second"));
        assert_eq!(entries.len(), 4, "the plan didn't move");
        assert!(matches!(&entries[2], Entry::Plan(items) if items == &plan("second")));
        assert!(matches!(&entries[3], Entry::System(text, _) if text == "b"));

        // The next turn keeps the old plan in the transcript and gets a new one.
        entries.push(Entry::User("two".into(), vec![]));
        set_plan(&mut entries, plan("third"));
        assert!(matches!(&entries[2], Entry::Plan(items) if items == &plan("second")));
        assert!(matches!(entries.last(), Some(Entry::Plan(items)) if items == &plan("third")));
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
