//! Per-workspace run state behind the title bar's run controls: the selected target, and
//! whether it is running (in a terminal, or as tests) or being debugged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dap::adapters::DebugAdapterName;
use db::kvp::KeyValueStore;
use forge_tests::TestPanel;
use gpui::TaskExt as _;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, EntityId, Global, SharedString, Subscription, Task, WeakEntity, Window, actions};
use project::Project;
use project::debugger::dap_store::{DapStore, DapStoreEvent};
use dap::client::SessionId;
use project::debugger::session::{OutputToken, Session};
use task::{SaveStrategy, TaskContext, TaskId};
use terminal::{TaskStatus, Terminal};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use util::ResultExt as _;
use workspace::Workspace;

use crate::aspire::{self, Notification};
use crate::targets::{self, Kind, RunTarget};

/// How long Stop waits after Ctrl+C before it kills what still runs. An Aspire app host
/// takes a while to stop its resources (some 20 s for a few containers); a second Stop
/// doesn't wait.
const STOP_GRACE: Duration = Duration::from_secs(30);

/// How long a project Aspire asked for may take to reach the debugger (it builds first).
const ASPIRE_START_TIMEOUT: Duration = Duration::from_secs(180);

actions!(forge_run, [
    Run,
    Debug,
    Stop,
    /// Runs the selected .NET app with hot reload (`dotnet watch`).
    Watch,
    /// Opens the dashboard of the running Aspire app host.
    OpenDashboard
]);

/// The controller of each workspace, so actions, title bar and status bar share one.
#[derive(Default)]
struct Controllers(HashMap<EntityId, WeakEntity<RunController>>);
impl Global for Controllers {}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        let controller = cx.new(|cx| RunController::new(workspace, cx));
        let id = cx.entity_id();
        cx.default_global::<Controllers>().0.insert(id, controller.downgrade());
        let (run, debug, stop, watch, dashboard) = (controller.clone(), controller.clone(), controller.clone(), controller.clone(), controller);
        workspace.register_action(move |_, _: &OpenDashboard, _, cx| {
            if let Some(url) = dashboard.read(cx).dashboard_url() {
                cx.open_url(url);
            }
        });
        workspace.register_action(move |_, _: &Watch, window, cx| {
            let watch = watch.clone();
            window.defer(cx, move |window, cx| watch.update(cx, |c, cx| c.watch(window, cx)));
        });
        // Actions run while the workspace is being updated, and running a target updates it
        // (to schedule the task, focus a panel or start a debug session): run right after.
        workspace.register_action(move |_, _: &Run, window, cx| {
            let run = run.clone();
            window.defer(cx, move |window, cx| run.update(cx, |c, cx| c.run(window, cx)));
        });
        workspace.register_action(move |_, _: &Debug, window, cx| {
            let debug = debug.clone();
            window.defer(cx, move |window, cx| debug.update(cx, |c, cx| c.debug(window, cx)));
        });
        workspace.register_action(move |_, _: &Stop, _, cx| stop.update(cx, |c, cx| c.stop(cx)));
    })
    .detach();
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Idle,
    /// Starting or running in a terminal.
    Running,
    /// Running a test project from the Tests panel.
    Testing,
    Debugging,
}

pub struct RunController {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    targets: Vec<RunTarget>,
    selected: Option<String>,
    terminal: Option<WeakEntity<Terminal>>,
    running: bool,
    /// Stop sent Ctrl+C and waits for the target to end; a second Stop kills it.
    stopping: bool,
    /// The login link of the running Aspire app host's dashboard, once it prints it.
    dashboard: Option<String>,
    dashboard_watch: Option<Task<()>>,
    /// The Aspire app host being debugged, whose projects run under the debugger.
    aspire: Option<AspireDebug>,
    discovery: Option<Task<()>>,
    watch: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl RunController {
    /// The terminal of the target running now (or that ran last, while it is open).
    pub fn terminal(&self) -> Option<Entity<Terminal>> {
        self.terminal.as_ref().and_then(WeakEntity::upgrade)
    }

    /// The controller of `workspace`, once it has been set up.
    pub fn for_workspace(workspace: &Entity<Workspace>, cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<Controllers>()?.0.get(&workspace.entity_id())?.upgrade()
    }

    fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let mut subscriptions = vec![cx.subscribe(&project, |this, _, event, cx| match event {
            project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_) => this.discover(cx),
            project::Event::WorktreeUpdatedEntries(_, entries) => {
                let manifests = entries.iter().any(|(path, _, _)| {
                    let file_name = path.file_name().unwrap_or_default();
                    matches!(path.extension(), Some("csproj" | "go" | "py"))
                        || matches!(file_name, "Cargo.toml" | "package.json" | "pyproject.toml" | "setup.py" | "setup.cfg" | "requirements.txt" | "launchSettings.json" | "apphost.cs" | "aspire.config.json")
                        // A file-based app's profiles, or one already found (its directives may change).
                        || file_name.ends_with(".run.json")
                        || (file_name == "settings.json" && path.parent().is_some_and(|p| p.file_name() == Some(".aspire")))
                        || this.targets.iter().any(|t| t.is_file_based() && t.manifest.ends_with(path.as_std_path()))
                });
                if manifests {
                    this.discover(cx);
                }
            }
            _ => {}
        })];
        // Debug sessions starting and ending change what the run controls show.
        let dap_store = project.read(cx).dap_store();
        subscriptions.push(cx.observe(&dap_store, |_, _, cx| cx.notify()));
        subscriptions.push(cx.subscribe(&dap_store, |this, store, event, cx| match event {
            DapStoreEvent::DebugClientStarted(id) => this.aspire_session_started(*id, &store, cx),
            DapStoreEvent::DebugClientShutdown(id) => this.aspire_session_ended(*id),
            _ => {}
        }));
        let mut this = Self {
            workspace: workspace.weak_handle(),
            project,
            targets: Vec::new(),
            selected: None,
            terminal: None,
            running: false,
            stopping: false,
            dashboard: None,
            dashboard_watch: None,
            aspire: None,
            discovery: None,
            watch: None,
            _subscriptions: subscriptions,
        };
        this.discover(cx);
        this
    }

    pub fn targets(&self) -> &[RunTarget] {
        &self.targets
    }

    pub fn selected(&self) -> Option<&RunTarget> {
        let selected = self.selected.as_deref();
        self.targets.iter().find(|t| Some(t.id().as_str()) == selected).or_else(|| self.targets.first())
    }

    /// The dashboard of the Aspire app host that is running, once it is up.
    pub fn dashboard_url(&self) -> Option<&str> {
        self.dashboard.as_deref()
    }

    pub fn select(&mut self, id: String, cx: &mut Context<Self>) {
        if let Some(key) = self.selection_key(cx) {
            let kvp = KeyValueStore::global(cx);
            let value = id.clone();
            cx.background_spawn(async move { kvp.write_kvp(key, value).await.log_err() }).detach();
        }
        self.selected = Some(id);
        cx.notify();
    }

    pub fn state(&self, cx: &App) -> State {
        let debugging = self.project.read(cx).dap_store().read(cx).sessions().any(|s| !s.read(cx).is_terminated());
        let testing = self.selected().is_some_and(|t| t.kind == Kind::DotnetTests)
            && self.test_panel(cx).is_some_and(|panel| panel.read(cx).is_running());
        if debugging {
            State::Debugging
        } else if self.running {
            State::Running
        } else if testing {
            State::Testing
        } else {
            State::Idle
        }
    }

    fn roots(&self, cx: &App) -> Vec<PathBuf> {
        self.project.read(cx).visible_worktrees(cx).map(|t| t.read(cx).abs_path().to_path_buf()).collect()
    }

    /// The selection is remembered per project (keyed by its first folder).
    fn selection_key(&self, cx: &App) -> Option<String> {
        Some(format!("forge-run-target:{}", self.roots(cx).first()?.display()))
    }

    fn discover(&mut self, cx: &mut Context<Self>) {
        let roots = self.roots(cx);
        let key = self.selection_key(cx);
        let kvp = KeyValueStore::global(cx);
        self.discovery = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(300)).await;
            let (targets, saved) = cx
                .background_spawn(async move {
                    let targets: Vec<_> = roots.iter().flat_map(|root| targets::discover(root)).collect();
                    let saved = key.and_then(|key| kvp.read_kvp(&key).ok().flatten());
                    (targets, saved)
                })
                .await;
            this.update(cx, |this, cx| {
                if this.selected.is_none() {
                    this.selected = saved;
                }
                this.targets = targets;
                cx.notify();
            })
            .ok();
        }));
    }

    fn test_panel(&self, cx: &App) -> Option<Entity<TestPanel>> {
        self.workspace.upgrade()?.read(cx).panel::<TestPanel>(cx)
    }

    fn task_context(target: &RunTarget) -> TaskContext {
        TaskContext { cwd: Some(target.dir.clone()), ..TaskContext::default() }
    }

    /// A task context for debugging in `dir`, with the user's shell environment: a debug
    /// locator runs its tools (`dotnet msbuild`) from Forge itself, whose environment is the
    /// GUI app's minimal one (no `dotnet` on `PATH` when launched from the Dock or Finder).
    fn debug_context(&self, dir: Option<PathBuf>, cx: &mut App) -> Task<TaskContext> {
        let environment = self.project.read(cx).environment().clone();
        let env = dir.clone().map(|dir| environment.update(cx, |env, cx| env.directory_environment(dir.into(), cx)));
        cx.background_spawn(async move {
            let project_env = match env {
                Some(env) => env.await.unwrap_or_default(),
                None => Default::default(),
            };
            TaskContext { cwd: dir, project_env, ..TaskContext::default() }
        })
    }

    pub fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.selected().cloned() else { return };
        if self.state(cx) != State::Idle {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else { return };
        if target.kind == Kind::DotnetTests {
            // The Tests panel reads the workspace when it starts a run: update it after.
            let panel = workspace.update(cx, |workspace, cx| workspace.focus_panel::<TestPanel>(window, cx));
            if let Some(panel) = panel {
                panel.update(cx, |panel, cx| panel.run_project(&target.manifest, window, cx));
            }
            cx.notify();
            return;
        }

        self.run_template(target.task(), &target, window, cx);
    }

    /// Runs the selected target with hot reload, if it can (`RunTarget::watch_task`).
    pub fn watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.selected().cloned() else { return };
        if self.state(cx) != State::Idle {
            return;
        }
        if let Some(template) = target.watch_task() {
            self.run_template(template, &target, window, cx);
        }
    }

    /// Runs `template` in a terminal and follows it until it ends.
    fn run_template(&mut self, mut template: task::TaskTemplate, target: &RunTarget, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        template.save = SaveStrategy::All;
        let Some(resolved) = template.resolve_task("forge-run", &Self::task_context(target)) else { return };
        let task_id = resolved.id.clone();
        // Running a task again reuses its terminal tab, which keeps showing the last run's
        // (ended) terminal until the new one replaces it: only a new terminal is this run's.
        let earlier: Vec<EntityId> = self.task_terminals(&task_id, cx).iter().map(|t| t.entity_id()).collect();
        workspace.update(cx, |workspace, cx| {
            workspace.schedule_resolved_task(project::TaskSourceKind::UserInput, resolved, false, window, cx);
        });
        self.running = true;
        self.terminal = None;
        self.dashboard = None;
        let aspire = target.kind == Kind::Aspire;
        cx.notify();
        self.watch = Some(cx.spawn_in(window, async move |this, cx| {
            // This run lasts while a terminal of its own runs the task, wherever that terminal
            // is (Terminal panel or editor area) and even if its tab gets a new terminal. It
            // shows up a little after scheduling (the task saves files first).
            let started = std::time::Instant::now();
            let mut seen = false;
            loop {
                let alive = this.update(cx, |this, cx| {
                    let current = this
                        .task_terminals(&task_id, cx)
                        .into_iter()
                        .filter(|t| !earlier.contains(&t.entity_id()))
                        .find(|t| t.read(cx).task().is_some_and(|task| task.status == TaskStatus::Running));
                    let Some(terminal) = current else { return false };
                    if this.terminal.as_ref().and_then(|t| t.upgrade()).as_ref() != Some(&terminal) {
                        this.terminal = Some(terminal.downgrade());
                        if aspire {
                            this.dashboard_watch = Some(Self::find_dashboard(terminal.downgrade(), cx));
                        }
                    }
                    true
                });
                match alive {
                    Ok(true) => seen = true,
                    Ok(false) if seen || started.elapsed() > Duration::from_secs(60) => break,
                    Ok(false) => {}
                    Err(_) => return,
                }
                cx.background_executor().timer(Duration::from_millis(250)).await;
            }
            this.update(cx, |this, cx| {
                this.running = false;
                this.stopping = false;
                this.terminal = None;
                this.dashboard = None;
                this.dashboard_watch = None;
                if let Some(aspire) = this.aspire.take() {
                    aspire.end(&this.project, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Watches an app host's output for the dashboard's login link and keeps it, so the
    /// title bar can offer it.
    fn find_dashboard(terminal: WeakEntity<Terminal>, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let content = terminal.read_with(cx, |terminal, _| terminal.task().is_some_and(|t| t.status == TaskStatus::Running).then(|| terminal.get_content()));
                let Ok(Some(content)) = content else { return };
                if let Some(url) = dashboard_url(&content) {
                    this.update(cx, |this, cx| {
                        this.dashboard = Some(url);
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                cx.background_executor().timer(Duration::from_millis(500)).await;
            }
        })
    }

    /// The terminals that run (or ran) the task `id`, in the Terminal panel or in the editor
    /// area (where a task's terminal can be moved, or open).
    fn task_terminals(&self, id: &TaskId, cx: &App) -> Vec<Entity<Terminal>> {
        let Some(workspace) = self.workspace.upgrade() else { return Vec::new() };
        let workspace = workspace.read(cx);
        let mut panes: Vec<Entity<workspace::Pane>> = workspace.panes().to_vec();
        if let Some(panel) = workspace.panel::<TerminalPanel>(cx) {
            panes.extend(panel.read(cx).panes().into_iter().cloned());
        }
        panes
            .iter()
            .flat_map(|pane| pane.read(cx).items_of_type::<TerminalView>())
            .map(|view| view.read(cx).terminal().clone())
            .filter(|terminal| terminal.read(cx).task().is_some_and(|task| &task.spawned_task.id == id))
            .collect()
    }

    pub fn debug(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.selected().cloned() else { return };
        if self.state(cx) != State::Idle {
            return;
        }
        if target.kind == Kind::Aspire {
            return self.debug_aspire(target, window, cx);
        }
        let Some(workspace) = self.workspace.upgrade() else { return };
        let worktree_id = self.project.read(cx).visible_worktrees(cx).next().map(|t| t.read(cx).id());
        let template = target.task();
        let label: gpui::SharedString = format!("Debug {}", target.name).into();
        let scenario = self.project.read(cx).dap_store().update(cx, |store, cx| {
            store.debug_scenario_for_build_task(template, DebugAdapterName(target.kind.debug_adapter().into()), label, cx)
        });
        let context = self.debug_context(Some(target.dir.clone()), cx);
        cx.spawn_in(window, async move |_, cx| {
            let context = context.await;
            let Some(scenario) = scenario.await else {
                anyhow::bail!("Forge does not know how to debug {}", target.name);
            };
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.start_debug_session(scenario, context.into(), None, worktree_id, window, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        match self.state(cx) {
            State::Debugging => {
                self.project.read(cx).dap_store().update(cx, |store, cx| store.shutdown_sessions(cx)).detach();
                // A debugged Aspire app host runs in a terminal, its projects in the debugger.
                if self.aspire.is_some() {
                    self.kill_terminal(cx);
                }
            }
            State::Running => self.kill_terminal(cx),
            State::Testing => {
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        if let Some(panel) = workspace.panel::<TestPanel>(cx) {
                            panel.update(cx, |panel, cx| panel.cancel(cx));
                        }
                    });
                }
            }
            State::Idle => {}
        }
        cx.notify();
    }
}

/// An Aspire app host being debugged. It runs in a terminal like any run, pointed at an
/// IDE endpoint (see `aspire`): its orchestrator then asks Forge for each of the app's
/// projects instead of starting them, and Forge starts each one under the debugger.
struct AspireDebug {
    endpoint: aspire::Endpoint,
    /// The app host's launch profile: projects with one of the same name use it.
    profile: Option<String>,
    /// Debug sessions asked for and not started yet, by label, with their run session.
    pending: HashMap<SharedString, String>,
    /// Run session → its debug session, and how much of its output DCP has seen.
    sessions: HashMap<String, (SessionId, OutputToken)>,
    _requests: Task<()>,
    _observers: Vec<Subscription>,
}

impl AspireDebug {
    /// The app host ended: so do its projects.
    fn end(self, project: &Entity<Project>, cx: &mut App) {
        project.read(cx).dap_store().update(cx, |store, cx| {
            for (session, _) in self.sessions.values() {
                store.shutdown_session(*session, cx).detach_and_log_err(cx);
            }
        });
    }
}

impl RunController {
    /// Stops the target running in the terminal with Ctrl+C, as you would by hand, so it
    /// can shut down cleanly: an Aspire app host stops its orchestrator and resources, which
    /// a killed one leaves running. What hasn't ended after `STOP_GRACE`, or on a second
    /// Stop, is killed.
    fn kill_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminal.as_ref().and_then(|t| t.upgrade()) else { return };
        if self.stopping {
            terminal.update(cx, |terminal, _| terminal.kill_active_task());
            return;
        }
        self.stopping = true;
        terminal.update(cx, |terminal, _| terminal.input(b"\x03".as_slice()));
        let terminal = terminal.downgrade();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(STOP_GRACE).await;
            // Only if this very run is still stopping (not one started since).
            let still = this.read_with(cx, |this, _| this.stopping && this.terminal.as_ref() == Some(&terminal)).unwrap_or(false);
            if still {
                terminal.update(cx, |terminal, _| terminal.kill_active_task()).ok();
            }
        })
        .detach();
    }

    /// Runs the app host with Forge as its IDE endpoint, so its projects get debugged.
    fn debug_aspire(&mut self, target: RunTarget, window: &mut Window, cx: &mut Context<Self>) {
        let (endpoint, mut requests) = match aspire::Endpoint::start() {
            Ok(started) => started,
            Err(error) => {
                log::error!("could not debug {}: {error:#}", target.name);
                return;
            }
        };
        let mut template = target.task();
        template.label = format!("Debug {}", target.name);
        template.env.extend(endpoint.env());
        let profile = target.entry.clone().or_else(|| forge_languages::launch_settings::profile(&target.manifest, None).map(|p| p.name));
        let requests = cx.spawn_in(window, async move |this, cx| {
            while let Some(request) = requests.next().await {
                if this.update_in(cx, |this, window, cx| this.aspire_request(request, window, cx)).is_err() {
                    break;
                }
            }
        });
        self.run_template(template, &target, window, cx);
        self.aspire = Some(AspireDebug { endpoint, profile, pending: HashMap::default(), sessions: HashMap::default(), _requests: requests, _observers: Vec::new() });
    }

    fn aspire_request(&mut self, request: aspire::Request, window: &mut Window, cx: &mut Context<Self>) {
        let Some(aspire) = self.aspire.as_mut() else { return };
        match request {
            aspire::Request::Start(session) => {
                let id = session.id.clone();
                let template = aspire_project_task(&session, aspire.profile.as_deref());
                let name = session.project.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let label: SharedString = format!("{name} · Aspire {id}").into();
                aspire.pending.insert(label.clone(), id.clone());
                let scenario = self.project.read(cx).dap_store().update(cx, |store, cx| {
                    store.debug_scenario_for_build_task(template, DebugAdapterName(Kind::Aspire.debug_adapter().into()), label.clone(), cx)
                });
                let context = self.debug_context(session.project.parent().map(Path::to_path_buf), cx);
                let worktree_id = self.project.read(cx).visible_worktrees(cx).next().map(|t| t.read(cx).id());
                let workspace = self.workspace.clone();
                cx.spawn_in(window, async move |this, cx| {
                    let context = context.await;
                    let started = match scenario.await {
                        Some(scenario) => workspace.update_in(cx, |workspace, window, cx| {
                            workspace.start_debug_session(scenario, context.into(), None, worktree_id, window, cx);
                        }),
                        None => Err(anyhow::anyhow!("Forge does not know how to debug {name}")),
                    };
                    if started.is_ok() {
                        // It reaches the debugger after building; give up on it if it never does.
                        cx.background_executor().timer(ASPIRE_START_TIMEOUT).await;
                    }
                    this.update(cx, |this, _| {
                        let Some(aspire) = this.aspire.as_mut() else { return };
                        if aspire.pending.remove(&label).is_some() {
                            let message = match started {
                                Ok(()) => format!("{name} did not start in the debugger"),
                                Err(error) => format!("{error:#}"),
                            };
                            aspire.endpoint.notify(&id, Notification::Error { message });
                            aspire.endpoint.notify(&id, Notification::Terminated { exit_code: None });
                        }
                    })
                    .ok();
                })
                .detach();
            }
            aspire::Request::Stop(id) => {
                if let Some((session, _)) = aspire.sessions.get(&id) {
                    let session = *session;
                    self.project.read(cx).dap_store().update(cx, |store, cx| store.shutdown_session(session, cx).detach_and_log_err(cx));
                } else {
                    aspire.pending.retain(|_, pending| *pending != id);
                    aspire.endpoint.notify(&id, Notification::Terminated { exit_code: None });
                }
            }
        }
    }

    /// A debug session started: if it runs a project of the app host, tie the two.
    fn aspire_session_started(&mut self, id: SessionId, store: &Entity<DapStore>, cx: &mut Context<Self>) {
        let Some(aspire) = self.aspire.as_mut() else { return };
        let Some(session) = store.read(cx).session_by_id(id) else { return };
        let Some(run_id) = session.read(cx).label().and_then(|label| aspire.pending.remove(&label)) else { return };
        aspire.sessions.insert(run_id.clone(), (id, OutputToken::default()));
        let observed = run_id.clone();
        aspire._observers.push(cx.observe(&session, move |this, session, cx| this.forward_output(&observed, &session, cx)));
        self.forward_output(&run_id, &session, cx);
    }

    /// Sends DCP what a project wrote since last time, for the dashboard's logs.
    fn forward_output(&mut self, run_id: &str, session: &Entity<Session>, cx: &mut Context<Self>) {
        let Some(aspire) = self.aspire.as_mut() else { return };
        let Some((_, token)) = aspire.sessions.get_mut(run_id) else { return };
        let (events, latest) = session.read(cx).output(*token);
        for event in events {
            let stderr = match event.category {
                Some(dap::OutputEventCategory::Stdout) => false,
                Some(dap::OutputEventCategory::Stderr) => true,
                _ => continue, // the debugger's own messages
            };
            aspire.endpoint.notify(run_id, Notification::Output { stderr, text: event.output.trim_end_matches(['\r', '\n']).to_string() });
        }
        *token = latest;
    }

    fn aspire_session_ended(&mut self, id: SessionId) {
        let Some(aspire) = self.aspire.as_mut() else { return };
        let Some(run_id) = aspire.sessions.iter().find(|(_, (s, _))| *s == id).map(|(run, _)| run.clone()) else { return };
        aspire.sessions.remove(&run_id);
        aspire.endpoint.notify(&run_id, Notification::Terminated { exit_code: None });
    }
}

/// `dotnet run` for a project Aspire asked for, which the .NET debug locator turns into a
/// debug session. The base launch profile is the one asked for, or else the app host's
/// when the project has one of that name; the request's environment and arguments win.
pub(crate) fn aspire_project_task(session: &aspire::RunSession, apphost_profile: Option<&str>) -> task::TaskTemplate {
    let file_based = session.project.extension().is_some_and(|e| e == "cs");
    let mut args = vec!["run".to_string(), if file_based { "--file" } else { "--project" }.into(), format!("\"{}\"", session.project.display())];
    let wanted = if session.disable_launch_profile { None } else { session.launch_profile.as_deref().or(apphost_profile) };
    match wanted.filter(|name| forge_languages::launch_settings::profile(&session.project, Some(name)).is_some()) {
        Some(profile) => args.extend(["--launch-profile".into(), format!("\"{profile}\"")]),
        None => args.push("--no-launch-profile".into()),
    }
    if let Some(program_args) = &session.args {
        args.push("--".into());
        args.extend(program_args.iter().cloned());
    }
    task::TaskTemplate {
        label: format!("dotnet run {}", session.project.display()),
        command: "dotnet".into(),
        args,
        env: session.env.iter().cloned().collect(),
        cwd: session.project.parent().map(|dir| dir.to_string_lossy().into_owned()),
        ..task::TaskTemplate::default()
    }
}

/// The dashboard link an Aspire app host prints when it starts: `Login to the dashboard at
/// https://localhost:17043/login?t=…`, or the `aspire run` CLI's `Dashboard: …`.
fn dashboard_url(output: &str) -> Option<String> {
    let url_after = |marker: &str| {
        let rest = &output[output.find(marker)? + marker.len()..];
        let url: String = rest.trim_start().chars().take_while(|c| !c.is_whitespace()).collect();
        url.starts_with("http").then_some(url)
    };
    url_after("Login to the dashboard at").or_else(|| url_after("Dashboard:"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    #[test]
    fn finds_the_dashboard_link() {
        let output = "info: Aspire.Hosting.DistributedApplication[0]\n      Aspire version: 9.5.0\ninfo: Aspire.Hosting.DistributedApplication[0]\n      Login to the dashboard at https://localhost:17043/login?t=3f2a  \n";
        assert_eq!(dashboard_url(output).as_deref(), Some("https://localhost:17043/login?t=3f2a"));
        assert_eq!(dashboard_url("     🔗  Dashboard:  https://localhost:17043/login?t=ab\n").as_deref(), Some("https://localhost:17043/login?t=ab"));
        assert_eq!(dashboard_url("Building...\n"), None);
    }

    /// The title bar's Run dispatches `Run` to the workspace, which is being updated while
    /// its handler runs: running a target must not update it again there.
    #[gpui::test]
    async fn run_from_the_title_bar(cx: &mut TestAppContext) {
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            init(cx);
        });
        params.fs.as_fake().insert_tree("/root", serde_json::json!({ "Cargo.toml": "[package]\nname = \"demo\"\n" })).await;
        let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let controller = cx.update(|_, cx| RunController::for_workspace(&workspace, cx)).unwrap();
        controller.update(cx, |c, _| {
            c.targets = vec![RunTarget { kind: Kind::Cargo, name: "demo".into(), manifest: "/root/Cargo.toml".into(), dir: "/root".into(), framework: None, program: None, entry: None }];
        });
        workspace.update_in(cx, |ws, window, cx| {
            window.focus(&gpui::Focusable::focus_handle(ws, cx), cx);
            window.dispatch_action(Box::new(Run), cx);
        });
        cx.run_until_parked();
        assert!(controller.read_with(cx, |c, _| c.running), "the target started");
    }
}
