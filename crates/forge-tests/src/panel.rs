//! The Tests dock panel: a project → class → test tree (classes are .NET classes, Go
//! packages, Rust modules or test files), with run buttons at every level, pass/fail state
//! from the last run and failure details.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use editor::Editor;
use gpui::TaskExt as _;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Action, AnyElement, App, AppContext as _, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, WeakEntity, Window, actions, div, px,
};
use project::Project;
use regex::Regex;
use theme::ActiveTheme as _;
use ui::{
    Button, ButtonCommon as _, ButtonSize, ButtonStyle, Clickable as _, Color, Disableable as _, Icon, IconButton, IconName,
    IconSize, CommonAnimationExt as _, Label, LabelCommon as _, LabelSize, Toggleable as _, Tooltip, VisibleOnHover as _, h_flex, v_flex,
};
use workspace::{
    OpenOptions, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::discovery::{self, TestProject};
use crate::runner::{self, Scope};
use crate::trx::{CaseResult, Outcome};

actions!(forge_tests, [ToggleFocus, RunAllTests, RunFailedTests, RefreshTests]);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<TestPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &RunAllTests, window, cx| {
            if let Some(panel) = workspace.focus_panel::<TestPanel>(window, cx) {
                panel.update(cx, |panel, cx| panel.run_all(window, cx));
            }
        });
        workspace.register_action(|workspace, _: &RunFailedTests, window, cx| {
            if let Some(panel) = workspace.focus_panel::<TestPanel>(window, cx) {
                panel.update(cx, |panel, cx| panel.run_failed(window, cx));
            }
        });
        workspace.register_action(|workspace, _: &RefreshTests, window, cx| {
            if let Some(panel) = workspace.panel::<TestPanel>(cx) {
                panel.update(cx, |panel, cx| panel.discover(window, cx));
            }
        });
    })
    .detach();
}

/// The state of a test, class or project in the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    NotRun,
    Running,
    Passed,
    Skipped,
    Failed,
}

impl Status {
    fn icon(self) -> (IconName, Color) {
        match self {
            Status::NotRun => (IconName::Circle, Color::Muted),
            Status::Running => (IconName::ArrowCircle, Color::Accent),
            Status::Passed => (IconName::Check, Color::Success),
            Status::Skipped => (IconName::Dash, Color::Muted),
            Status::Failed => (IconName::XCircle, Color::Error),
        }
    }

    /// Folds the states of children into their parent's.
    fn combine(statuses: impl IntoIterator<Item = Status>) -> Status {
        let mut combined = None;
        for status in statuses {
            combined = Some(match (combined, status) {
                (_, Status::Running) | (Some(Status::Running), _) => Status::Running,
                (_, Status::Failed) | (Some(Status::Failed), _) => Status::Failed,
                (_, Status::NotRun) | (Some(Status::NotRun), _) => Status::NotRun,
                (_, Status::Passed) | (Some(Status::Passed), _) => Status::Passed,
                _ => Status::Skipped,
            });
        }
        combined.unwrap_or(Status::NotRun)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum NodeKey {
    Project(PathBuf),
    Class(String),
    Method(String),
}

struct Run {
    /// Methods (by FQN) this run will report on.
    methods: HashSet<String>,
    /// The project `dotnet test` is running now, and since when.
    current: Option<(String, std::time::Instant)>,
    /// What it has printed so far.
    live: runner::LiveLog,
    _task: Task<()>,
    /// Repaints the elapsed time and the live output.
    _ticker: Task<()>,
}

pub struct TestPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    position: DockPosition,
    projects: Vec<TestProject>,
    discovering: bool,
    /// Rediscovers shortly after source files change.
    discovery_task: Option<Task<()>>,
    collapsed: HashSet<NodeKey>,
    /// Last results per method FQN; one entry per theory row and target framework.
    results: HashMap<String, Vec<CaseResult>>,
    run: Option<Run>,
    selected: Option<NodeKey>,
    /// Output of the last `dotnet test`, shown on demand and after build failures.
    last_log: Option<String>,
    show_log: bool,
    error: Option<String>,
    list_scroll: ScrollHandle,
    details_scroll: ScrollHandle,
    /// Files this panel last colored in the gutter, so stale entries can be cleared.
    published_files: HashSet<PathBuf>,
    _subscriptions: Vec<Subscription>,
}

impl TestPanel {
    pub fn new(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let subscription = cx.subscribe_in(&project, window, |this, _, event, window, cx| match event {
            project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_) => this.discover(window, cx),
            project::Event::WorktreeUpdatedEntries(_, entries) => {
                let touches_tests = entries.iter().any(|(path, _, _)| {
                    matches!(path.extension(), Some("cs" | "csproj" | "go" | "rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" | "py"))
                        || matches!(path.file_name(), Some("go.mod" | "Cargo.toml" | "package.json" | "pyproject.toml" | "pytest.ini" | "setup.cfg" | "tox.ini" | "setup.py" | "requirements.txt"))
                });
                if touches_tests {
                    this.discover(window, cx);
                }
            }
            _ => {}
        });
        let mut panel = Self {
            workspace: workspace.weak_handle(),
            project,
            focus_handle: cx.focus_handle(),
            position: forge_ui::dock_position::load(<Self as Panel>::panel_key(), cx).unwrap_or(DockPosition::Left),
            projects: Vec::new(),
            discovering: false,
            discovery_task: None,
            collapsed: HashSet::new(),
            results: HashMap::new(),
            run: None,
            selected: None,
            last_log: None,
            show_log: false,
            error: None,
            list_scroll: ScrollHandle::new(),
            details_scroll: ScrollHandle::new(),
            published_files: HashSet::new(),
            _subscriptions: vec![subscription],
        };
        panel.discover(window, cx);
        panel
    }

    fn roots(&self, cx: &App) -> Vec<PathBuf> {
        self.project.read(cx).visible_worktrees(cx).map(|tree| tree.read(cx).abs_path().to_path_buf()).collect()
    }

    /// Rescans the project for test projects and tests, debounced.
    pub fn discover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let roots = self.roots(cx);
        self.discovering = true;
        cx.notify();
        self.discovery_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(300)).await;
            let projects = cx
                .background_spawn(async move { roots.iter().flat_map(|root| discovery::discover(root)).collect::<Vec<_>>() })
                .await;
            this.update(cx, |this, cx| {
                // Freshly found classes start collapsed when there are many of them.
                let class_count: usize = projects.iter().map(|p| p.classes.len()).sum();
                if this.projects.is_empty() && class_count > 12 {
                    this.collapsed.extend(projects.iter().flat_map(|p| p.classes.iter().map(|c| NodeKey::Class(c.fqn.clone()))));
                }
                this.projects = projects;
                this.discovering = false;
                this.publish_gutter(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Shows the current state of every test and class in the editors' gutters.
    fn publish_gutter(&mut self, cx: &mut Context<Self>) {
        let mut files = HashSet::new();
        let mut statuses = Vec::new();
        for (p, project) in self.projects.iter().enumerate() {
            for (c, class) in project.classes.iter().enumerate() {
                files.insert(class.file.clone());
                if class.row != discovery::NO_ROW {
                    statuses.push((class.file.clone(), class.row, self.class_status(p, c)));
                }
                for test in &class.tests {
                    files.insert(test.file.clone());
                    statuses.push((test.file.clone(), test.row, self.method_status(&test.fqn)));
                }
            }
        }
        files.extend(self.published_files.drain());
        self.published_files = statuses.iter().map(|(file, _, _)| file.clone()).collect();
        crate::gutter::publish(files, statuses, self.workspace.upgrade().as_ref(), cx);
    }

    fn method_status(&self, fqn: &str) -> Status {
        if self.run.as_ref().is_some_and(|run| run.methods.contains(fqn)) {
            return Status::Running;
        }
        match self.results.get(fqn) {
            None => Status::NotRun,
            Some(cases) => match cases.iter().map(|c| c.outcome).max() {
                Some(Outcome::Failed) => Status::Failed,
                Some(Outcome::Passed) => Status::Passed,
                Some(Outcome::Skipped) => Status::Skipped,
                None => Status::NotRun,
            },
        }
    }

    fn class_status(&self, project: usize, class: usize) -> Status {
        Status::combine(self.projects[project].classes[class].tests.iter().map(|t| self.method_status(&t.fqn)))
    }

    fn project_status(&self, project: usize) -> Status {
        Status::combine((0..self.projects[project].classes.len()).map(|c| self.class_status(project, c)))
    }

    fn methods_in(&self, project: usize, scope: &Scope) -> HashSet<String> {
        let classes = self.projects[project].classes.iter();
        match scope {
            Scope::Project => classes.flat_map(|c| c.tests.iter().map(|t| t.fqn.clone())).collect(),
            Scope::Class(fqn) => classes.filter(|c| &c.fqn == fqn).flat_map(|c| c.tests.iter().map(|t| t.fqn.clone())).collect(),
            Scope::Methods(methods) => methods.iter().cloned().collect(),
        }
    }

    pub fn run_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let jobs = (0..self.projects.len()).map(|p| (p, Scope::Project)).collect();
        self.start(jobs, window, cx);
    }

    pub fn run_failed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let jobs = (0..self.projects.len())
            .filter_map(|p| {
                let failed: Vec<_> = self.projects[p]
                    .classes
                    .iter()
                    .flat_map(|c| &c.tests)
                    .filter(|t| self.method_status(&t.fqn) == Status::Failed)
                    .map(|t| t.fqn.clone())
                    .collect();
                (!failed.is_empty()).then_some((p, Scope::Methods(failed)))
            })
            .collect();
        self.start(jobs, window, cx);
    }

    fn run_node(&mut self, key: &NodeKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project_of(key) else {
            return;
        };
        let scope = match key {
            NodeKey::Project(_) => Scope::Project,
            NodeKey::Class(fqn) => Scope::Class(fqn.clone()),
            NodeKey::Method(fqn) => Scope::Methods(vec![fqn.clone()]),
        };
        self.start(vec![(project, scope)], window, cx);
    }

    fn scope_of(key: &NodeKey) -> Scope {
        match key {
            NodeKey::Project(_) => Scope::Project,
            NodeKey::Class(fqn) => Scope::Class(fqn.clone()),
            NodeKey::Method(fqn) => Scope::Methods(vec![fqn.clone()]),
        }
    }

    /// Whether the debugger can run the tests under `key` (not Jest or Vitest).
    fn can_debug(&self, key: &NodeKey) -> bool {
        self.project_of(key).and_then(|p| runner::debug_plan(&self.projects[p], &Self::scope_of(key))).is_some()
    }

    /// Runs the tests under `key` in the debugger, stopping at breakpoints.
    fn debug_node(&mut self, key: &NodeKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project_of(key).map(|p| self.projects[p].clone()) else { return };
        let label = match key {
            NodeKey::Method(fqn) => format!("Debug {}", fqn.rsplit(['.', ':', '>']).next().unwrap_or(fqn).trim()),
            _ => format!("Debug {}", project.name),
        };
        self.debug_scope(&project.path, Self::scope_of(key), label.into(), window, cx);
    }

    /// Runs `scope` of the project whose manifest is `manifest` in the debugger (as a test's
    /// Debug button does). False when Forge can't debug those tests (Jest, Vitest).
    pub fn debug_scope(&mut self, manifest: &std::path::Path, scope: Scope, label: gpui::SharedString, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(project) = self.projects.iter().find(|p| p.path == manifest).cloned() else { return false };
        let Some(plan) = runner::debug_plan(&project, &scope) else { return false };
        let Some(workspace) = self.workspace.upgrade() else { return false };
        let worktree_id = self.project.read(cx).visible_worktrees(cx).next().map(|t| t.read(cx).id());
        let scenario = match plan {
            runner::DebugPlan::Build(template, adapter) => {
                self.project.read(cx).dap_store().update(cx, |store, cx| store.debug_scenario_for_build_task(template, dap::adapters::DebugAdapterName(adapter.into()), label, cx))
            }
            runner::DebugPlan::Scenario(scenario) => Task::ready(Some(task::DebugScenario { label, ..scenario })),
        };
        // The debug locator runs `dotnet` from Forge itself, so it needs the user's shell
        // environment rather than the GUI app's minimal one.
        let dir = project.dir().to_path_buf();
        let environment = self.project.read(cx).environment().clone();
        let env = environment.update(cx, |env, cx| env.directory_environment(dir.as_path().into(), cx));
        cx.spawn_in(window, async move |_, cx| {
            let context = task::TaskContext { cwd: Some(dir), project_env: env.await.unwrap_or_default(), ..task::TaskContext::default() };
            let Some(scenario) = scenario.await else {
                anyhow::bail!("Forge does not know how to debug the tests of {}", project.name);
            };
            workspace.update_in(cx, |ws, window, cx| ws.start_debug_session(scenario, context.into(), None, worktree_id, window, cx))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
        true
    }

    fn project_of(&self, key: &NodeKey) -> Option<usize> {
        self.projects.iter().position(|project| match key {
            NodeKey::Project(path) => &project.path == path,
            NodeKey::Class(fqn) => project.classes.iter().any(|c| &c.fqn == fqn),
            NodeKey::Method(fqn) => project.classes.iter().any(|c| c.tests.iter().any(|t| &t.fqn == fqn)),
        })
    }

    /// Runs each `(project, scope)` in turn; one run at a time.
    fn start(&mut self, jobs: Vec<(usize, Scope)>, window: &mut Window, cx: &mut Context<Self>) {
        if self.run.is_some() || jobs.is_empty() {
            return;
        }
        let jobs: Vec<_> = jobs
            .into_iter()
            .map(|(p, scope)| (self.projects[p].clone(), self.methods_in(p, &scope), scope))
            .collect();
        let methods = jobs.iter().flat_map(|(_, methods, _)| methods.iter().cloned()).collect();
        self.error = None;
        let live = runner::LiveLog::default();
        let task_live = live.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let mut log = String::new();
            for (project, methods, scope) in jobs {
                let name = project.name.clone();
                this.update(cx, |this, cx| {
                    if let Some(run) = &mut this.run {
                        run.current = Some((name, std::time::Instant::now()));
                    }
                    cx.notify();
                })
                .ok();
                let job_live = task_live.clone();
                // The project folder's shell environment: tools on the user's PATH.
                let env_task = this
                    .update(cx, |this, cx| {
                        let dir: Arc<std::path::Path> = project.dir().into();
                        this.project.update(cx, |p, cx| p.environment().update(cx, |env, cx| env.directory_environment(dir, cx)))
                    })
                    .ok();
                let env: runner::Env = match env_task {
                    Some(task) => task.await.map(|env| env.into_iter().collect()).unwrap_or_default(),
                    None => Default::default(),
                };
                let output = cx.background_spawn(async move { runner::run(&project, &scope, job_live, &env).await }).await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        let keep_going = this.finish_job(methods, output, &mut log);
                        this.publish_gutter(cx);
                        cx.notify();
                        keep_going
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
            this.update(cx, |this, cx| {
                this.run = None;
                this.last_log = Some(log);
                this.publish_gutter(cx);
                cx.notify();
            })
            .ok();
        });
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(500)).await;
                let running = this.update(cx, |this, cx| {
                    cx.notify();
                    this.run.is_some()
                });
                if !matches!(running, Ok(true)) {
                    break;
                }
            }
        });
        self.run = Some(Run { methods, current: None, live, _task: task, _ticker: ticker });
        self.publish_gutter(cx);
        cx.notify();
    }

    /// Records one project's results. Returns whether to run the next project.
    fn finish_job(&mut self, methods: HashSet<String>, output: anyhow::Result<runner::RunOutput>, log: &mut String) -> bool {
        if let Some(run) = &mut self.run {
            run.methods.retain(|m| !methods.contains(m));
        }
        let output = match output {
            Ok(output) => output,
            Err(err) => {
                self.error = Some(format!("{err:#}"));
                return false;
            }
        };
        log.push_str(&output.log);
        if output.results.is_empty() && !output.success {
            self.error = Some("The tests didn't run (a build error?); see the output".into());
            self.show_log = true;
            return true;
        }
        for method in &methods {
            self.results.remove(method);
        }
        for case in output.results {
            self.results.entry(case.method_fqn.clone()).or_default().push(case);
        }
        true
    }

    /// Stops the current run, if any.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.run.is_some() {
            self.stop(cx);
        }
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        // Dropping the task drops the `dotnet test` future, which kills the process.
        self.run = None;
        self.error = Some("Run cancelled".into());
        self.publish_gutter(cx);
        cx.notify();
    }

    fn open(&self, file: PathBuf, row: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let open = workspace.update(cx, |workspace, cx| workspace.open_abs_path(file, OpenOptions::default(), window, cx));
        cx.spawn_in(window, async move |_, cx: &mut AsyncWindowContext| {
            let item = open.await?;
            if let Some(editor) = item.downcast::<Editor>() {
                editor.update_in(cx, |editor, window, cx| {
                    editor.go_to_singleton_buffer_point(language::Point::new(row, 0), window, cx);
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn location(&self, key: &NodeKey) -> Option<(PathBuf, u32)> {
        self.projects.iter().find_map(|project| match key {
            NodeKey::Project(path) => (&project.path == path).then(|| (path.clone(), 0)),
            NodeKey::Class(fqn) => project.classes.iter().find(|c| &c.fqn == fqn).map(|c| (c.file.clone(), if c.row == discovery::NO_ROW { c.tests.first().map_or(0, |t| t.row) } else { c.row })),
            NodeKey::Method(fqn) => project
                .classes
                .iter()
                .flat_map(|c| &c.tests)
                .find(|t| &t.fqn == fqn)
                .map(|t| (t.file.clone(), t.row)),
        })
    }

    /// Runs every test of the project whose `.csproj` is `manifest`.
    pub fn run_project(&mut self, manifest: &std::path::Path, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(project) = self.projects.iter().position(|p| p.path == manifest) {
            self.start(vec![(project, Scope::Project)], window, cx);
        }
    }

    /// Runs `scope` in each project whose manifest is given, as the panel's own buttons do
    /// (progress here, results in the gutter). Nothing happens while another run is going.
    pub fn run_scopes(&mut self, jobs: Vec<(PathBuf, Scope)>, window: &mut Window, cx: &mut Context<Self>) {
        let jobs = jobs.into_iter().filter_map(|(manifest, scope)| Some((self.projects.iter().position(|p| p.path == manifest)?, scope))).collect();
        self.start(jobs, window, cx);
    }

    /// The test projects found, with their tests.
    pub fn projects(&self) -> &[TestProject] {
        &self.projects
    }

    /// The last results of a test method, by its FQN (one per theory row and framework).
    pub fn results_for(&self, method_fqn: &str) -> &[CaseResult] {
        self.results.get(method_fqn).map(Vec::as_slice).unwrap_or_default()
    }

    /// The output of the last run, and why it failed when it couldn't run.
    pub fn last_log(&self) -> Option<&str> {
        self.last_log.as_deref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn is_running(&self) -> bool {
        self.run.is_some()
    }

    /// `(passed, failed, not run)` over every discovered test.
    pub fn summary(&self) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        for project in &self.projects {
            for test in project.classes.iter().flat_map(|c| &c.tests) {
                match self.method_status(&test.fqn) {
                    Status::Passed => counts.0 += 1,
                    Status::Failed => counts.1 += 1,
                    Status::NotRun => counts.2 += 1,
                    _ => {}
                }
            }
        }
        counts
    }

    fn render_row(
        &self,
        key: NodeKey,
        depth: usize,
        label: String,
        status: Status,
        expandable: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let collapsed = self.collapsed.contains(&key);
        let selected = self.selected.as_ref() == Some(&key);
        let running = self.run.is_some();
        let (icon, color) = status.icon();
        let id = format!("{key:?}");
        let group = SharedGroup::name(&id);
        let duration = match &key {
            NodeKey::Method(fqn) => self.results.get(fqn).map(|cases| cases.iter().map(|c| c.duration).sum::<Duration>()).filter(|d| !d.is_zero()),
            _ => None,
        };
        let kind = match &key {
            NodeKey::Project(path) => self.projects.iter().find(|p| &p.path == path).map(|p| p.kind.label()),
            _ => None,
        };

        let toggle_key = key.clone();
        let run_key = key.clone();
        let debug_key = key.clone();
        let can_debug = self.can_debug(&key);
        let click_key = key.clone();
        h_flex()
            .id(gpui::ElementId::Name(id.into()))
            .group(group.clone())
            .w_full()
            .h(px(24.))
            .pl(px(8. + depth as f32 * 14.))
            .pr_1()
            .gap_1()
            .cursor_pointer()
            .when(selected, |row| row.bg(colors.element_selected))
            .hover(|row| row.bg(colors.element_hover))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.selected = Some(click_key.clone());
                this.show_log = false;
                if event.click_count() >= 2 {
                    if let Some((file, row)) = this.location(&click_key).filter(|_| !matches!(click_key, NodeKey::Project(_))) {
                        this.open(file, row, window, cx);
                    }
                }
                cx.notify();
            }))
            .child(
                div().w(px(14.)).when(expandable, |slot| {
                    slot.child(
                        IconButton::new("toggle", if collapsed { IconName::ChevronRight } else { IconName::ChevronDown })
                            .icon_size(IconSize::XSmall)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.collapsed.remove(&toggle_key) {
                                    this.collapsed.insert(toggle_key.clone());
                                }
                                cx.notify();
                            })),
                    )
                }),
            )
            .child(status_icon(status, icon, color))
            .child(div().flex_1().min_w_0().overflow_hidden().child(Label::new(label).size(LabelSize::Small).single_line()))
            .children(kind.map(|kind| Label::new(kind).size(LabelSize::XSmall).color(Color::Muted)))
            .children(duration.map(|d| Label::new(format_duration(d)).size(LabelSize::XSmall).color(Color::Muted)))
            .child(
                IconButton::new("run", IconName::PlayFilled)
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Success)
                    .disabled(running)
                    .tooltip(Tooltip::text("Run"))
                    .visible_on_hover(group.clone())
                    .on_click(cx.listener(move |this, _, window, cx| this.run_node(&run_key, window, cx))),
            )
            .when(can_debug, |row| {
                row.child(
                    IconButton::new("debug", IconName::Debug)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Warning)
                        .disabled(running)
                        .tooltip(Tooltip::text("Debug"))
                        .visible_on_hover(group)
                        .on_click(cx.listener(move |this, _, window, cx| this.debug_node(&debug_key, window, cx))),
                )
            })
            .into_any_element()
    }

    fn render_tree(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        for (p, project) in self.projects.iter().enumerate() {
            let key = NodeKey::Project(project.path.clone());
            let collapsed = self.collapsed.contains(&key);
            rows.push(self.render_row(key, 0, project.name.clone(), self.project_status(p), true, cx));
            if collapsed {
                continue;
            }
            for (c, class) in project.classes.iter().enumerate() {
                let key = NodeKey::Class(class.fqn.clone());
                let collapsed = self.collapsed.contains(&key);
                rows.push(self.render_row(key, 1, class.name.clone(), self.class_status(p, c), true, cx));
                if collapsed {
                    continue;
                }
                for test in &class.tests {
                    rows.push(self.render_row(NodeKey::Method(test.fqn.clone()), 2, test.name.clone(), self.method_status(&test.fqn), false, cx));
                }
            }
        }
        rows
    }

    fn render_details(&self, _window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let border = cx.theme().colors().border;
        let body: AnyElement = if self.show_log {
            let log = match &self.run {
                Some(run) => run.live.lock().map(|l| l.clone()).unwrap_or_default(),
                None => self.last_log.clone().unwrap_or_default(),
            };
            // Long build logs: the end is what matters.
            let lines: Vec<&str> = log.lines().collect();
            let log = lines[lines.len().saturating_sub(400)..].join("\n");
            v_flex().children(log.lines().map(|line| Label::new(line.to_string()).size(LabelSize::XSmall).buffer_font(cx))).into_any_element()
        } else {
            let Some(NodeKey::Method(fqn)) = &self.selected else {
                return None;
            };
            let cases = self.results.get(fqn)?;
            let mut column = v_flex().gap_2();
            if let Some(prompt) = self.fix_prompt(fqn, cx) {
                column = column.child(
                    h_flex().child(
                        Button::new("fix-with-agent", "Fix with agent")
                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                            .size(ButtonSize::Compact)
                            .start_icon(Icon::from_path("icons/forge_agents.svg").size(IconSize::XSmall))
                            .tooltip(Tooltip::text("Send the failure and the test to the agent"))
                            .on_click(move |_, window, cx| window.dispatch_action(Box::new(forge_ui::AskAgent { prompt: prompt.clone() }), cx)),
                    ),
                );
            }
            for case in cases {
                let (icon, color) = match case.outcome {
                    Outcome::Passed => Status::Passed.icon(),
                    Outcome::Failed => Status::Failed.icon(),
                    Outcome::Skipped => Status::Skipped.icon(),
                };
                let title = match &case.framework {
                    Some(framework) => format!("{} [{framework}] · {}", short_name(&case.display_name), format_duration(case.duration)),
                    None => format!("{} · {}", short_name(&case.display_name), format_duration(case.duration)),
                };
                let mut entry = v_flex().gap_1().child(
                    h_flex().gap_1().child(Icon::new(icon).size(IconSize::Small).color(color)).child(Label::new(title).size(LabelSize::Small)),
                );
                let root = self.project_of(&NodeKey::Method(fqn.clone())).map(|p| self.projects[p].dir().to_path_buf());
                if let Some(message) = &case.message {
                    let mut lines = Vec::new();
                    for (i, line) in message.lines().enumerate() {
                        lines.push(match source_location(line, root.as_deref()) {
                            Some(_) => self.render_stack_line(10_000 + i, line, root.as_deref(), cx),
                            None => Label::new(line.to_string()).size(LabelSize::XSmall).buffer_font(cx).color(color).into_any_element(),
                        });
                    }
                    entry = entry.child(v_flex().pl_5().children(lines));
                }
                if let Some(stack) = &case.stack_trace {
                    let mut frames = Vec::new();
                    for (i, line) in stack.lines().enumerate() {
                        frames.push(self.render_stack_line(i, line, root.as_deref(), cx));
                    }
                    entry = entry.child(v_flex().pl_5().children(frames));
                }
                column = column.child(entry);
            }
            column.into_any_element()
        };
        Some(
            div()
                .id("test-details")
                .flex_shrink_0()
                .max_h(px(260.))
                .border_t_1()
                .border_color(border)
                .p_2()
                .overflow_y_scroll()
                .track_scroll(&self.details_scroll)
                .child(body)
                .into_any_element(),
        )
    }

    /// What "Fix with agent" sends for a failing test: its failures and the test file.
    fn fix_prompt(&self, fqn: &str, cx: &App) -> Option<String> {
        let failures: Vec<String> = self
            .results
            .get(fqn)?
            .iter()
            .filter(|case| case.outcome == Outcome::Failed)
            .map(|case| {
                let mut text = case.display_name.clone();
                if let Some(framework) = &case.framework {
                    text.push_str(&format!(" [{framework}]"));
                }
                for part in [&case.message, &case.stack_trace].into_iter().flatten() {
                    text.push('\n');
                    text.push_str(part);
                }
                text
            })
            .collect();
        if failures.is_empty() {
            return None;
        }
        let (file, row) = self.location(&NodeKey::Method(fqn.to_string()))?;
        let root = self.roots(cx).into_iter().find(|root| file.starts_with(root));
        let path = root.as_ref().and_then(|root| file.strip_prefix(root).ok()).unwrap_or(&file).to_string_lossy().into_owned();
        let name = self
            .projects
            .iter()
            .flat_map(|p| &p.classes)
            .find_map(|c| c.tests.iter().find(|t| t.fqn == fqn).map(|t| if matches!(c.row, discovery::NO_ROW) || c.name.is_empty() { format!("{} › {}", c.name, t.name) } else { format!("{}.{}", c.name, t.name) }))
            .unwrap_or_else(|| fqn.to_string());
        Some(format!(
            "The test `{name}` fails. Find out why and fix it (the code under test, or the test if the test is wrong). The test is in @{path} at line {}.\n\n```\n{}\n```",
            row + 1,
            failures.join("\n\n")
        ))
    }

    /// A line of a failure; one naming a source line links to it: `in /path/File.cs:line
    /// 12` (.NET), `calc_test.go:12` (Go), `src/lib.rs:12:9` (Rust), `(/path/a.test.ts:4:7)`
    /// (Node). Relative paths are resolved against the test's project.
    fn render_stack_line(&self, index: usize, line: &str, root: Option<&std::path::Path>, cx: &mut Context<Self>) -> AnyElement {
        let label = Label::new(line.trim_end().to_string()).size(LabelSize::XSmall).buffer_font(cx);
        let Some((file, row)) = source_location(line, root) else {
            return label.color(Color::Muted).into_any_element();
        };
        div()
            .id(("frame", index))
            .cursor_pointer()
            .hover(|style| style.underline())
            .child(label.color(Color::Accent))
            .on_click(cx.listener(move |this, _, window, cx| this.open(file.clone(), row, window, cx)))
            .into_any_element()
    }
}

/// The source file and zero-based line a failure line points at, if it names one that
/// exists.
fn source_location(line: &str, root: Option<&std::path::Path>) -> Option<(PathBuf, u32)> {
    static DOTNET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" in (.+):line (\d+)\s*$").unwrap());
    static GENERIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"((?:[A-Za-z]:)?[\w./\\@+-]+\.(?:cs|go|rs|ts|tsx|js|jsx|mjs|cjs|mts|cts|fs|vb)):(\d+)").unwrap());
    let (path, row) = if let Some(c) = DOTNET.captures(line) {
        (c[1].to_string(), c[2].to_string())
    } else {
        let c = GENERIC.captures(line)?;
        (c[1].trim_start_matches("file://").to_string(), c[2].to_string())
    };
    let path = PathBuf::from(path);
    let path = if path.is_absolute() { path } else { root?.join(path) };
    let row = row.parse::<u32>().ok()?.saturating_sub(1);
    path.is_file().then_some((path, row))
}

/// The status glyph; a running test spins.
fn status_icon(status: Status, icon: IconName, color: Color) -> AnyElement {
    let icon = Icon::new(icon).size(IconSize::Small).color(color);
    if status == Status::Running {
        icon.with_rotate_animation(1).into_any_element()
    } else {
        icon.into_any_element()
    }
}

/// `Ns.Class.Method(x: 1)` → `Method(x: 1)`.
fn short_name(display_name: &str) -> &str {
    let name_end = display_name.find('(').unwrap_or(display_name.len());
    let start = display_name[..name_end].rfind('.').map_or(0, |i| i + 1);
    &display_name[start..]
}

fn format_duration(duration: Duration) -> String {
    if duration < Duration::from_secs(1) {
        format!("{} ms", duration.as_millis())
    } else {
        format!("{:.1} s", duration.as_secs_f64())
    }
}

/// Group names for `visible_on_hover`, unique per row.
struct SharedGroup;
impl SharedGroup {
    fn name(id: &str) -> gpui::SharedString {
        format!("test-row-{id}").into()
    }
}

impl Render for TestPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let running = self.run.is_some();
        let (passed, failed, not_run) = self.summary();
        let has_failed = failed > 0;

        let header = h_flex()
            .justify_between()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(forge_ui::panel_grip("tests-panel-grip", Arc::new(cx.entity()), "Tests", Some(PANEL_ICON)))
                    .child(Label::new("Tests").weight(FontWeight::BOLD))
                    .when(passed + failed > 0, |row| {
                        row.child(Label::new(format!("{passed} passed")).size(LabelSize::XSmall).color(Color::Success))
                            .when(has_failed, |row| row.child(Label::new(format!("{failed} failed")).size(LabelSize::XSmall).color(Color::Error)))
                            .when(not_run > 0, |row| row.child(Label::new(format!("{not_run} not run")).size(LabelSize::XSmall).color(Color::Muted)))
                    }),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("run-all", IconName::PlayFilled)
                            .icon_color(Color::Success)
                            .disabled(running || self.projects.is_empty())
                            .tooltip(Tooltip::text("Run all tests"))
                            .on_click(cx.listener(|this, _, window, cx| this.run_all(window, cx))),
                    )
                    .child(
                        IconButton::new("run-failed", IconName::Rerun)
                            .disabled(running || !has_failed)
                            .tooltip(Tooltip::text("Run failed tests"))
                            .on_click(cx.listener(|this, _, window, cx| this.run_failed(window, cx))),
                    )
                    .when(running, |row| {
                        row.child(
                            IconButton::new("stop", IconName::Stop)
                                .icon_color(Color::Error)
                                .tooltip(Tooltip::text("Stop"))
                                .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                        )
                    })
                    .child(
                        IconButton::new("output", IconName::ListTree)
                            .toggle_state(self.show_log)
                            .disabled(self.last_log.is_none() && self.run.is_none())
                            .tooltip(Tooltip::text("Output of the last run"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_log = !this.show_log;
                                cx.notify();
                            })),
                    )
                    .child(
                        IconButton::new("refresh", IconName::RotateCw)
                            .disabled(self.discovering)
                            .tooltip(Tooltip::text("Find tests again"))
                            .on_click(cx.listener(|this, _, window, cx| this.discover(window, cx))),
                    ),
            );

        let body: AnyElement = if self.projects.is_empty() {
            let message = if self.discovering { "Looking for tests…" } else { "No tests in this workspace (.NET, Go, Rust, Jest or Vitest)." };
            v_flex()
                .p_4()
                .gap_2()
                .child(Label::new(message).color(Color::Muted))
                .when(!self.discovering, |col| {
                    col.child(
                        Button::new("rescan", "Look again")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .on_click(cx.listener(|this, _, window, cx| this.discover(window, cx))),
                    )
                })
                .into_any_element()
        } else {
            div()
                .id("test-tree")
                .flex_1()
                .min_h_0()
                .py_1()
                .overflow_y_scroll()
                .track_scroll(&self.list_scroll)
                .children(self.render_tree(cx))
                .into_any_element()
        };

        v_flex()
            .key_context("ForgeTestPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.panel_background)
            .child(header)
            .children(self.run.as_ref().and_then(|run| run.current.clone()).map(|(name, started)| {
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(Icon::new(IconName::ArrowCircle).size(IconSize::Small).color(Color::Accent).with_rotate_animation(1))
                    .child(Label::new(format!("Running {name} · {} s", started.elapsed().as_secs())).size(LabelSize::Small).color(Color::Muted))
                    .child(div().flex_1())
                    .child(
                        Button::new("live-output", if self.show_log { "Hide output" } else { "Show output" })
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_log = !this.show_log;
                                cx.notify();
                            })),
                    )
            }))
            .children(self.error.clone().map(|error| {
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .bg(colors.editor_background)
                    .child(Icon::new(IconName::Warning).size(IconSize::Small).color(Color::Warning))
                    .child(Label::new(error).size(LabelSize::Small).color(Color::Warning))
            }))
            .child(body)
            .children(self.render_details(window, cx))
    }
}

const PANEL_ICON: IconName = IconName::PlayOutlined;

impl Focusable for TestPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for TestPanel {}

impl Panel for TestPanel {
    fn persistent_name() -> &'static str {
        "ForgeTestPanel"
    }
    fn panel_key() -> &'static str {
        "ForgeTestPanel"
    }
    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }
    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }
    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        forge_ui::dock_position::save(Self::panel_key(), position, cx);
        // Docks re-check panel positions when settings change; poke the store so the
        // workspace moves the panel now (see AgentPanel::set_position).
        gpui::BorrowAppContext::update_global::<settings::SettingsStore, _>(cx, |_, _| {});
        cx.notify();
    }
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(320.)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(PANEL_ICON)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Tests")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }
    fn activation_priority(&self) -> u32 {
        // Forge panels use 10–19; Zed's use 1–7 and extension slots 8 and 20+.
        11
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, point, size};
    use std::time::Instant;

    #[test]
    fn combines_statuses_like_a_test_runner() {
        use Status::*;
        assert_eq!(Status::combine([Passed, Passed]), Passed);
        assert_eq!(Status::combine([Passed, Failed, NotRun]), Failed);
        assert_eq!(Status::combine([Passed, Running]), Running);
        assert_eq!(Status::combine([Passed, NotRun]), NotRun);
        assert_eq!(Status::combine([Passed, Skipped]), Passed);
        assert_eq!(Status::combine([Skipped]), Skipped);
        assert_eq!(Status::combine([]), NotRun);
    }

    #[test]
    fn shortens_display_names() {
        assert_eq!(short_name("Ns.Class.Doubles(x: 2, expected: 5)"), "Doubles(x: 2, expected: 5)");
        assert_eq!(short_name("Ns.Class.Adds"), "Adds");
        assert_eq!(format_duration(Duration::from_millis(12)), "12 ms");
    }

    const DEMO_TESTS: &str = "namespace Demo.Tests;\n\npublic class MathTests\n{\n    [Fact]\n    public void Adds() => Assert.Equal(4, 2 + 2);\n\n    [Fact]\n    public void Fails() => Assert.Equal(5, 2 + 2);\n\n    [Theory]\n    [InlineData(1, 2)]\n    [InlineData(2, 5)]\n    public void Doubles(int x, int expected) => Assert.Equal(expected, x * 2);\n\n    [Fact(Skip = \"not today\")]\n    public void Skipped() { }\n}\n";

    /// A workspace whose only worktree is `root`, a real folder: discovery reads the disk.
    async fn panel_for(root: &std::path::Path, cx: &mut TestAppContext) -> (Entity<TestPanel>, VisualTestContext) {
        cx.executor().allow_parking();
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(root, serde_json::json!({})).await;
        cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let project = Project::test(fs, [root], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let panel = workspace.update_in(&mut cx, |ws, window, cx| cx.new(|cx| TestPanel::new(ws, window, cx)));
        (panel, cx)
    }

    async fn wait_for(cx: &mut VisualTestContext, panel: &Entity<TestPanel>, what: &str, timeout: Duration, pred: impl Fn(&TestPanel) -> bool) {
        let deadline = Instant::now() + timeout;
        loop {
            cx.executor().advance_clock(Duration::from_millis(500));
            cx.run_until_parked();
            if panel.read_with(cx, |p, _| pred(p)) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn draw(panel: &Entity<TestPanel>, cx: &mut VisualTestContext) {
        let panel = panel.clone();
        cx.draw(point(px(0.), px(0.)), size(px(400.), px(800.)), move |_, _| div().size_full().child(panel));
    }

    /// Zed's debug locators turn the Tests panel's Go and pytest tasks into sessions. (Its
    /// Cargo locator isn't public; it takes any `cargo test` command.)
    #[gpui::test]
    async fn debug_tasks_become_debug_scenarios(cx: &mut TestAppContext) {
        use crate::discovery::{Kind, TestProject};
        let params = cx.update(workspace::AppState::test);
        // What `project::init` registers in the app.
        cx.update(|cx| {
            let registry = dap::DapRegistry::global(cx);
            registry.add_locator(std::sync::Arc::new(project::debugger::locators::go::GoLocator {}));
            registry.add_locator(std::sync::Arc::new(project::debugger::locators::python::PythonLocator));
        });
        let project = Project::test(params.fs.clone(), [], cx).await;
        let test_project = |kind: Kind, path: &str| TestProject { name: "p".into(), path: path.into(), kind, classes: vec![] };
        let go = tempfile::tempdir().unwrap();
        std::fs::write(go.path().join("go.mod"), "module example.com/app\n").unwrap();
        let go_mod = go.path().join("go.mod").to_string_lossy().into_owned();
        let cases = [
            (test_project(Kind::Go, &go_mod), Scope::Methods(vec!["example.com/app/pkg.TestAdds".into()])),
            (test_project(Kind::Python, "/py/pyproject.toml"), Scope::Methods(vec!["/py/test_a.py::test_b".into()])),
        ];
        for (test_project, scope) in cases {
            let (template, adapter) = runner::debug_task(&test_project, &scope).unwrap();
            let scenario = project
                .read_with(cx, |p, _| p.dap_store())
                .update(cx, |store, cx| store.debug_scenario_for_build_task(template.clone(), dap::adapters::DebugAdapterName(adapter.into()), "t".into(), cx))
                .await;
            assert!(scenario.is_some(), "{adapter} takes {} {:?}", template.command, template.args);
        }
    }

    #[gpui::test]
    async fn discovers_renders_and_aggregates(cx: &mut TestAppContext) {
        let root = tempfile::tempdir().unwrap();
        let tests_dir = root.path().join("Demo.Tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        std::fs::write(tests_dir.join("Demo.Tests.csproj"), "<Project><ItemGroup><PackageReference Include=\"Microsoft.NET.Test.Sdk\" /></ItemGroup></Project>").unwrap();
        std::fs::write(tests_dir.join("MathTests.cs"), DEMO_TESTS).unwrap();

        let (panel, mut cx) = panel_for(root.path(), cx).await;
        wait_for(&mut cx, &panel, "discovery", Duration::from_secs(10), |p| !p.discovering && !p.projects.is_empty()).await;
        let names = panel.read_with(&cx, |p, _| p.projects[0].classes[0].tests.iter().map(|t| t.name.clone()).collect::<Vec<_>>());
        assert_eq!(names, ["Adds", "Fails", "Doubles", "Skipped"]);
        draw(&panel, &mut cx);

        // Feed the results of a real run (the fixture is this very class) and select a failure.
        let results = crate::trx::parse(include_str!("../fixtures/demo_net10.0.trx"), Some("net10.0")).unwrap();
        panel.update(&mut cx, |p, _| {
            let methods = p.methods_in(0, &Scope::Project);
            let mut log = String::new();
            p.finish_job(methods, Ok(runner::RunOutput { results, log: "build log".into(), success: false }), &mut log);
            p.selected = Some(NodeKey::Method("Demo.Tests.MathTests.Doubles".into()));
        });
        panel.read_with(&cx, |p, _| {
            assert_eq!(p.method_status("Demo.Tests.MathTests.Adds"), Status::Passed);
            assert_eq!(p.method_status("Demo.Tests.MathTests.Doubles"), Status::Failed, "one failing theory row fails the method");
            assert_eq!(p.method_status("Demo.Tests.MathTests.Skipped"), Status::Skipped);
            assert_eq!(p.class_status(0, 0), Status::Failed);
            assert_eq!(p.summary(), (1, 2, 0));
        });
        panel.update(&mut cx, |p, cx| p.publish_gutter(cx));
        let file = tests_dir.join("MathTests.cs");
        assert_eq!(crate::gutter::status_at(&file, 5), Some(Status::Passed), "Adds");
        assert_eq!(crate::gutter::status_at(&file, 8), Some(Status::Failed), "Fails");
        assert_eq!(crate::gutter::status_at(&file, 2), Some(Status::Failed), "the class");
        draw(&panel, &mut cx);
    }

    /// Runs `dotnet test` for real; see `runner::tests::runs_a_real_project`.
    #[gpui::test]
    #[ignore]
    async fn runs_all_tests_from_the_panel(cx: &mut TestAppContext) {
        let Ok(project) = std::env::var("FORGE_TESTS_DOTNET_PROJECT") else {
            return;
        };
        let root = std::path::PathBuf::from(project).parent().unwrap().to_path_buf();
        let (panel, mut cx) = panel_for(&root, cx).await;
        wait_for(&mut cx, &panel, "discovery", Duration::from_secs(10), |p| !p.projects.is_empty()).await;
        panel.update_in(&mut cx, |p, window, cx| p.run_all(window, cx));
        assert!(panel.read_with(&cx, |p, _| p.method_status("Demo.Tests.MathTests.Adds") == Status::Running));
        draw(&panel, &mut cx);
        wait_for(&mut cx, &panel, "results", Duration::from_secs(120), |p| p.run.is_none()).await;
        panel.read_with(&cx, |p, _| {
            assert_eq!(p.error, None, "{:?}", p.last_log);
            assert_eq!(p.method_status("Demo.Tests.MathTests.Adds"), Status::Passed);
            assert_eq!(p.method_status("Demo.Tests.MathTests.Fails"), Status::Failed);
            assert_eq!(p.summary(), (1, 2, 0));
        });
        draw(&panel, &mut cx);
    }
}
