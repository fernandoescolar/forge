//! Forge's title bar, its command center. The window uses a transparent macOS title bar
//! (like Zed), so the workspace draws one: `PlatformTitleBar` (from Zed) reserves room for
//! the traffic lights and handles dragging / double-click-to-zoom. Forge puts in it:
//! project, branch and pending git changes on the left; the solution, the run controls
//! (target, run, debug, stop) and the result of the last test run on the right.

use forge_run::{RunController, State};
use forge_tests::TestPanel;
use gpui::{
    Action, AnyElement, AppContext as _, Context, Entity, FontWeight, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, WeakEntity, Window, div, px,
};
use platform_title_bar::PlatformTitleBar;
use project::Project;
use theme::ActiveTheme as _;
use ui::{
    ButtonCommon as _, ButtonLike, ButtonStyle, Clickable as _, Color, ContextMenu, Disableable as _, Icon, IconButton, IconName, IconSize, Label,
    LabelCommon as _, LabelSize, PopoverMenu, Tooltip, h_flex,
};
use workspace::Workspace;


pub fn init(cx: &mut gpui::App) {
    PlatformTitleBar::init(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let workspace_entity = cx.entity();
        let bar = cx.new(|cx| ForgeTitleBar::new(workspace, &workspace_entity, cx));
        workspace.set_titlebar_item(bar.into(), window, cx);
    })
    .detach();
}

pub struct ForgeTitleBar {
    platform: Entity<PlatformTitleBar>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    /// Set up lazily: the run controller and Tests panel appear after the title bar.
    watched_run: bool,
    watched_tests: bool,
    watched_solution: bool,
    _subscriptions: Vec<Subscription>,
}

impl ForgeTitleBar {
    fn new(workspace: &Workspace, workspace_entity: &Entity<Workspace>, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let multi_workspace = workspace.multi_workspace().cloned();
        let platform = cx.new(|cx| {
            let bar = PlatformTitleBar::new("forge-title-bar", cx);
            match multi_workspace {
                Some(mw) => bar.with_multi_workspace(mw),
                None => bar,
            }
        });
        // Worktrees, git state and debug sessions change the title bar.
        let subscriptions = vec![
            cx.observe(&project, |_, _, cx| cx.notify()),
            cx.observe(&project.read(cx).git_store().clone(), |_, _, cx| cx.notify()),
            // Statuses and ahead/behind change without the store notifying.
            cx.subscribe(&project.read(cx).git_store().clone(), |_, _, _: &project::git_store::GitStoreEvent, cx| cx.notify()),
            cx.observe(&project.read(cx).dap_store(), |_, _, cx| cx.notify()),
            // Panels and the active item change what the bar shows.
            cx.observe(workspace_entity, |_, _, cx| cx.notify()),
        ];
        Self { platform, workspace: workspace.weak_handle(), project, watched_run: false, watched_tests: false, watched_solution: false, _subscriptions: subscriptions }
    }

    fn project_name(&self, cx: &gpui::App) -> Option<SharedString> {
        let names: Vec<String> = self.project.read(cx).visible_worktrees(cx).map(|wt| wt.read(cx).root_name().as_unix_str().to_string()).collect();
        (!names.is_empty()).then(|| names.join(", ").into())
    }

    fn branch(&self, cx: &gpui::App) -> Option<SharedString> {
        let repo = self.project.read(cx).active_repository(cx)?;
        let repo = repo.read(cx);
        let name = repo.branch.as_ref().map(|b| b.name().to_string()).or_else(|| repo.head_commit.as_ref().map(|c| c.sha.chars().take(7).collect()))?;
        Some(name.into())
    }

    /// Files changed in the working tree or index, and commits ahead of / behind upstream.
    fn git_changes(&self, cx: &gpui::App) -> Option<AnyElement> {
        let repo = self.project.read(cx).active_repository(cx)?;
        let repo = repo.read(cx);
        let changed = repo.status_summary().count;
        let tracking = repo.branch.as_ref().and_then(|b| b.tracking_status());
        let (ahead, behind) = tracking.map(|t| (t.ahead, t.behind)).unwrap_or_default();
        let mut chip = h_flex().gap_1p5();
        let mut tooltip = Vec::new();
        if changed > 0 {
            chip = chip.child(
                h_flex()
                    .gap_0p5()
                    .child(Icon::new(IconName::Circle).size(IconSize::XSmall).color(Color::Modified))
                    .child(Label::new(changed.to_string()).size(LabelSize::Small).color(Color::Modified)),
            );
            tooltip.push(format!("{changed} changed file{}", if changed == 1 { "" } else { "s" }));
        }
        if ahead > 0 {
            chip = chip.child(Label::new(format!("↑{ahead}")).size(LabelSize::Small).color(Color::Muted));
            tooltip.push(format!("{ahead} commit{} to push", if ahead == 1 { "" } else { "s" }));
        }
        if behind > 0 {
            chip = chip.child(Label::new(format!("↓{behind}")).size(LabelSize::Small).color(Color::Accent));
            tooltip.push(format!("{behind} commit{} on the server to pull", if behind == 1 { "" } else { "s" }));
        }
        if tooltip.is_empty() {
            return None;
        }
        let tooltip: SharedString = format!("{} — open Git Changes", tooltip.join(", ")).into();
        Some(
            ButtonLike::new("tb-git-changes")
                .style(ButtonStyle::Subtle)
                .child(chip)
                .tooltip(Tooltip::text(tooltip))
                .on_click(Self::dispatch(Box::new(zed_actions::git_panel::ToggleFocus)))
                .into_any_element(),
        )
    }

    fn controller(&mut self, cx: &mut Context<Self>) -> Option<Entity<RunController>> {
        let workspace = self.workspace.upgrade()?;
        let controller = RunController::for_workspace(&workspace, cx)?;
        if !self.watched_run {
            self.watched_run = true;
            self._subscriptions.push(cx.observe(&controller, |_, _, cx| cx.notify()));
        }
        Some(controller)
    }

    fn test_panel(&mut self, cx: &mut Context<Self>) -> Option<Entity<TestPanel>> {
        let panel = self.workspace.upgrade()?.read(cx).panel::<TestPanel>(cx)?;
        if !self.watched_tests {
            self.watched_tests = true;
            self._subscriptions.push(cx.observe(&panel, |_, _, cx| cx.notify()));
        }
        Some(panel)
    }

    fn dotnet_model(&mut self, cx: &mut Context<Self>) -> Option<Entity<forge_dotnet::model::DotnetModel>> {
        let explorer = self.workspace.upgrade()?.read(cx).panel::<forge_dotnet::SolutionExplorer>(cx)?;
        let model = explorer.read(cx).model().clone();
        if !self.watched_solution {
            self.watched_solution = true;
            self._subscriptions.push(cx.observe(&model, |_, _, cx| cx.notify()));
        }
        Some(model)
    }

    /// The solution the Solution Explorer shows; with several in the workspace, a menu
    /// to show another one.
    fn solution_picker(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let model = self.dotnet_model(cx)?;
        let (solutions, current) = {
            let m = model.read(cx);
            (m.solutions.clone(), m.solution.as_ref().map(|s| (s.name(), s.path.clone())))
        };
        // Loose projects in a folder (no solution file) have nothing to pick.
        let (name, current_path) = current.filter(|_| !solutions.is_empty())?;
        let chip = |chevron: bool| {
            h_flex()
                .gap_1()
                .child(Icon::from_path("icons/forge_solution.svg").size(IconSize::XSmall).color(Color::Muted))
                .child(Label::new(name.clone()).size(LabelSize::Small))
                .when(chevron, |row| row.child(Icon::new(IconName::ChevronDown).size(IconSize::XSmall).color(Color::Muted)))
        };
        if solutions.len() < 2 {
            return Some(
                ButtonLike::new("tb-solution")
                    .style(ButtonStyle::Subtle)
                    .child(chip(false))
                    .tooltip(Tooltip::for_action_title("Solution Explorer", &forge_dotnet::explorer::ToggleFocus))
                    .on_click(Self::dispatch(Box::new(forge_dotnet::explorer::ToggleFocus)))
                    .into_any_element(),
            );
        }
        let model = model.downgrade();
        Some(
            PopoverMenu::new("tb-solution-menu")
                .trigger(ButtonLike::new("tb-solution").style(ButtonStyle::Subtle).child(chip(true)).tooltip(Tooltip::text("Show another solution")))
                .menu(move |window, cx| {
                    let (solutions, current_path, model) = (solutions.clone(), current_path.clone(), model.clone());
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        menu = menu.header("Solutions");
                        for path in solutions {
                            let label = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            let model = model.clone();
                            let checked = path == current_path;
                            menu = menu.toggleable_entry(label, checked, ui::IconPosition::Start, None, move |_, cx| {
                                model.update(cx, |m, cx| m.select_solution(path.clone(), cx)).ok();
                            });
                        }
                        menu
                    }))
                })
                .into_any_element(),
        )
    }

    fn dispatch(action: Box<dyn Action>) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static {
        move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx)
    }

    /// [target ▾] ▶ 🐞 ■
    fn run_controls(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let controller = self.controller(cx)?;
        let (target, targets, state, dashboard) = {
            let c = controller.read(cx);
            (c.selected().cloned(), c.targets().to_vec(), c.state(cx), c.dashboard_url().is_some())
        };
        if targets.is_empty() {
            return None;
        }
        let busy = state != State::Idle;
        let target_label: SharedString = target.as_ref().map(|t| t.name.clone()).unwrap_or_default().into();
        let kind_label = target.as_ref().map(|t| t.kind.label()).unwrap_or_default();
        let selected_id = target.as_ref().map(|t| t.id());
        let menu_controller = controller.downgrade();
        let picker = PopoverMenu::new("run-target-menu")
            .trigger(
                ButtonLike::new("run-target")
                    .style(ButtonStyle::Subtle)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Label::new(kind_label).size(LabelSize::XSmall).color(Color::Muted))
                            .child(Label::new(target_label).size(LabelSize::Small))
                            .child(Icon::new(IconName::ChevronDown).size(IconSize::XSmall).color(Color::Muted)),
                    )
                    .tooltip(Tooltip::text("What Run and Debug start")),
            )
            .menu(move |window, cx| {
                let (targets, selected_id, controller) = (targets.clone(), selected_id.clone(), menu_controller.clone());
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    let mut last_kind = None;
                    for target in targets {
                        if last_kind != Some(target.kind) {
                            menu = menu.header(target.kind.label());
                            last_kind = Some(target.kind);
                        }
                        let id = target.id();
                        let checked = Some(&id) == selected_id.as_ref();
                        let controller = controller.clone();
                        menu = menu.toggleable_entry(target.name.clone(), checked, ui::IconPosition::Start, None, move |_, cx| {
                            controller.update(cx, |c, cx| c.select(id.clone(), cx)).ok();
                        });
                    }
                    menu
                }))
            });

        let run_icon = if state == State::Running || state == State::Testing { IconName::ArrowCircle } else { IconName::PlayFilled };
        Some(
            h_flex()
                .gap_0p5()
                .child(picker)
                .child(
                    IconButton::new("run", run_icon)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Success)
                        .disabled(busy)
                        .tooltip(Tooltip::for_action_title("Run", &forge_run::Run))
                        .on_click(Self::dispatch(Box::new(forge_run::Run))),
                )
                .when(target.as_ref().is_some_and(|t| t.watch_task().is_some()), |el| {
                    el.child(
                        IconButton::new("watch", IconName::Flame)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Warning)
                            .disabled(busy)
                            .tooltip(Tooltip::for_action_title("Run with Hot Reload (dotnet watch)", &forge_run::Watch))
                            .on_click(Self::dispatch(Box::new(forge_run::Watch))),
                    )
                })
                .child(
                    IconButton::new("debug", IconName::Debug)
                        .icon_size(IconSize::Small)
                        .icon_color(if state == State::Debugging { Color::Accent } else { Color::Default })
                        .disabled(busy)
                        .tooltip(Tooltip::for_action_title("Debug", &forge_run::Debug))
                        .on_click(Self::dispatch(Box::new(forge_run::Debug))),
                )
                .when(dashboard, |el| {
                    el.child(
                        IconButton::new("aspire-dashboard", IconName::ArrowUpRight)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::for_action_title("Open the Aspire Dashboard", &forge_run::OpenDashboard))
                            .on_click(Self::dispatch(Box::new(forge_run::OpenDashboard))),
                    )
                })
                .child(
                    IconButton::new("stop", IconName::Stop)
                        .icon_size(IconSize::Small)
                        .icon_color(if busy { Color::Error } else { Color::Muted })
                        .disabled(!busy)
                        .tooltip(Tooltip::for_action_title("Stop", &forge_run::Stop))
                        .on_click(Self::dispatch(Box::new(forge_run::Stop))),
                )
                .into_any_element(),
        )
    }

    /// ✓ 43  ✗ 2 — opens the Tests panel.
    fn test_summary(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let panel = self.test_panel(cx)?;
        let (passed, failed, _) = panel.read(cx).summary();
        let running = panel.read(cx).is_running();
        if passed + failed == 0 && !running {
            return None;
        }
        let mut chip = h_flex().gap_1p5();
        if running {
            chip = chip.child(Icon::new(IconName::ArrowCircle).size(IconSize::XSmall).color(Color::Accent));
        }
        chip = chip
            .child(h_flex().gap_0p5().child(Icon::new(IconName::Check).size(IconSize::XSmall).color(Color::Success)).child(Label::new(passed.to_string()).size(LabelSize::Small)))
            .when(failed > 0, |chip| {
                chip.child(h_flex().gap_0p5().child(Icon::new(IconName::XCircle).size(IconSize::XSmall).color(Color::Error)).child(Label::new(failed.to_string()).size(LabelSize::Small).color(Color::Error)))
            });
        Some(
            ButtonLike::new("test-summary")
                .style(ButtonStyle::Subtle)
                .child(chip)
                .tooltip(Tooltip::text("Last test run — open the Tests panel"))
                .on_click(Self::dispatch(Box::new(forge_tests::panel::ToggleFocus)))
                .into_any_element(),
        )
    }
}

use gpui::prelude::FluentBuilder as _;

impl Render for ForgeTitleBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let left = h_flex()
            .gap_2()
            // Breathing room after the traffic lights.
            .pl_4()
            .child(Icon::from_path("icons/forge_mark.svg").size(IconSize::Small).color(Color::Accent))
            .child(Label::new(self.project_name(cx).unwrap_or_else(|| "Forge".into())).size(LabelSize::Small).weight(FontWeight::SEMIBOLD))
            .children(self.branch(cx).map(|b| {
                ButtonLike::new("tb-branch")
                    .style(ButtonStyle::Subtle)
                    .tooltip(Tooltip::for_action_title("Switch branch", &zed_actions::git::Branch))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall).color(Color::Muted))
                            .child(Label::new(b).size(LabelSize::Small).color(Color::Muted)),
                    )
                    .on_click(Self::dispatch(Box::new(zed_actions::git::Branch)))
                    .into_any_element()
            }))
            .children(self.git_changes(cx))
            // Not a repository yet: offer `git init` where the branch would be.
            .when(forge_git::init::uninitialized_root(&self.project, cx).is_some(), |row| {
                row.child(
                    ButtonLike::new("tb-git-init")
                        .style(ButtonStyle::Subtle)
                        .tooltip(Tooltip::text("Initialize a git repository (branch main)"))
                        .child(
                            h_flex()
                                .gap_1()
                                .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall).color(Color::Accent))
                                .child(Label::new("Init git").size(LabelSize::Small).color(Color::Accent)),
                        )
                        .on_click(Self::dispatch(Box::new(forge_git::init::InitRepository))),
                )
            });

        let separator = || div().w(px(1.)).h(px(16.)).mx_1().bg(colors.border_variant);
        let (solution, run) = (self.solution_picker(cx), self.run_controls(cx));
        let has_solution = solution.is_some();
        let right = h_flex()
            .gap_1()
            .pr_2()
            .children(solution)
            .when(has_solution && run.is_some(), |row| row.child(separator()))
            .children(run)
            .children(self.test_summary(cx));

        let children = [h_flex()
            .w_full()
            .justify_between()
            .child(div().flex_1().child(left))
            .child(h_flex().flex_1().justify_end().child(right))
            .into_any_element()];
        self.platform.update(cx, |bar, _| bar.set_children(children));
        self.platform.clone()
    }
}
