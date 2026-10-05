//! Per-workspace run state behind the title bar's run controls: the selected target, and
//! whether it is running (in a terminal, or as tests) or being debugged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use dap::adapters::DebugAdapterName;
use db::kvp::KeyValueStore;
use forge_tests::TestPanel;
use gpui::TaskExt as _;
use gpui::{App, AppContext as _, Context, Entity, EntityId, Global, Subscription, Task, WeakEntity, Window, actions};
use project::Project;
use task::{SaveStrategy, TaskContext, TaskId};
use terminal::Terminal;
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use util::ResultExt as _;
use workspace::Workspace;

use crate::targets::{self, Kind, RunTarget};

actions!(forge_run, [
    Run,
    Debug,
    Stop,
    /// Runs the selected .NET app with hot reload (`dotnet watch`).
    Watch
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
        let (run, debug, stop, watch) = (controller.clone(), controller.clone(), controller.clone(), controller);
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
    discovery: Option<Task<()>>,
    watch: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl RunController {
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
                    matches!(path.extension(), Some("csproj" | "go" | "py"))
                        || matches!(path.file_name(), Some("Cargo.toml" | "package.json" | "pyproject.toml" | "setup.py" | "setup.cfg" | "requirements.txt" | "launchSettings.json"))
                });
                if manifests {
                    this.discover(cx);
                }
            }
            _ => {}
        })];
        // Debug sessions starting and ending change what the run controls show.
        subscriptions.push(cx.observe(&project.read(cx).dap_store(), |_, _, cx| cx.notify()));
        let mut this = Self {
            workspace: workspace.weak_handle(),
            project,
            targets: Vec::new(),
            selected: None,
            terminal: None,
            running: false,
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
        workspace.update(cx, |workspace, cx| {
            workspace.schedule_resolved_task(project::TaskSourceKind::UserInput, resolved, false, window, cx);
        });
        self.running = true;
        self.terminal = None;
        cx.notify();
        self.watch = Some(cx.spawn_in(window, async move |this, cx| {
            // The task saves files first, so its terminal shows up a little later.
            let mut terminal = None;
            for _ in 0..100 {
                terminal = this.update(cx, |this, cx| this.find_terminal(&task_id, cx)).ok().flatten();
                if terminal.is_some() {
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(100)).await;
            }
            if let Some(terminal) = terminal {
                this.update(cx, |this, _| this.terminal = Some(terminal.downgrade())).ok();
                terminal.read_with(cx, |terminal, cx| terminal.wait_for_completed_task(cx)).await;
            }
            this.update(cx, |this, cx| {
                this.running = false;
                this.terminal = None;
                cx.notify();
            })
            .ok();
        }));
    }

    fn find_terminal(&self, id: &TaskId, cx: &App) -> Option<Entity<Terminal>> {
        let panel = self.workspace.upgrade()?.read(cx).panel::<TerminalPanel>(cx)?;
        panel.read(cx).panes().into_iter().flat_map(|pane| pane.read(cx).items_of_type::<TerminalView>()).find_map(|view| {
            let terminal = view.read(cx).terminal().clone();
            let matches = terminal.read(cx).task().is_some_and(|task| &task.spawned_task.id == id);
            matches.then_some(terminal)
        })
    }

    pub fn debug(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.selected().cloned() else { return };
        if self.state(cx) != State::Idle {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else { return };
        let worktree_id = self.project.read(cx).visible_worktrees(cx).next().map(|t| t.read(cx).id());
        let template = target.task();
        let label: gpui::SharedString = format!("Debug {}", target.name).into();
        let scenario = self.project.read(cx).dap_store().update(cx, |store, cx| {
            store.debug_scenario_for_build_task(template, DebugAdapterName(target.kind.debug_adapter().into()), label, cx)
        });
        let context = Self::task_context(&target);
        cx.spawn_in(window, async move |_, cx| {
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
            }
            State::Running => {
                if let Some(terminal) = self.terminal.as_ref().and_then(|t| t.upgrade()) {
                    terminal.update(cx, |terminal, _| terminal.kill_active_task());
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

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
