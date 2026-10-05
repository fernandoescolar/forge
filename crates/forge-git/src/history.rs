//! The History panel: the active repository's commit graph (Zed's `git_graph`, with its
//! search and commit details) in a dock instead of a tab.

use std::sync::Arc;

use git_ui::git_graph::GitGraph;
use gpui::{
    Action, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, Styled as _, Subscription, Window, actions, div, px,
};
use project::{Project, git_store::{GitStoreEvent, RepositoryId}};
use theme::ActiveTheme as _;
use ui::{Button, ButtonCommon as _, ButtonSize, ButtonStyle, Clickable as _, Color, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, h_flex, v_flex};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::init::{DEFAULT_BRANCH, InitRepository, uninitialized_root};

actions!(forge_git, [ToggleHistory]);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleHistory, window, cx| {
            workspace.toggle_panel_focus::<HistoryPanel>(window, cx);
        });
    })
    .detach();
}

pub struct HistoryPanel {
    project: Entity<Project>,
    workspace: gpui::WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    graph: Option<(RepositoryId, Entity<GitGraph>)>,
    _subscriptions: Vec<Subscription>,
}

impl HistoryPanel {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        let subscription = cx.subscribe_in(&git_store, window, |this, _, event, window, cx| {
            if matches!(event, GitStoreEvent::ActiveRepositoryChanged(_) | GitStoreEvent::RepositoryAdded | GitStoreEvent::RepositoryRemoved(_)) {
                this.sync_graph(window, cx);
            }
        });
        let mut panel = Self {
            project,
            workspace: workspace.weak_handle(),
            focus_handle: cx.focus_handle(),
            position: forge_ui::dock_position::load(<Self as Panel>::panel_key(), cx).unwrap_or(DockPosition::Bottom),
            graph: None,
            _subscriptions: vec![subscription],
        };
        panel.sync_graph(window, cx);
        panel
    }

    pub fn has_graph(&self) -> bool {
        self.graph.is_some()
    }

    /// Shows the graph of the active repository, replacing it when that changes.
    fn sync_graph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.project.read(cx).active_repository(cx).map(|repo| repo.read(cx).id);
        if self.graph.as_ref().map(|(id, _)| *id) == active {
            return;
        }
        self.graph = active.map(|id| {
            let git_store = self.project.read(cx).git_store().clone();
            let workspace = self.workspace.clone();
            (id, cx.new(|cx| GitGraph::new(id, git_store, workspace, None, window, cx)))
        });
        cx.notify();
    }
}

impl Render for HistoryPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let branch = self.project.read(cx).active_repository(cx).and_then(|repo| repo.read(cx).branch.as_ref().map(|b| b.name().to_string()));
        let header = h_flex()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(colors.border)
            .child(forge_ui::panel_grip("history-panel-grip", Arc::new(cx.entity()), "History", Some(IconName::HistoryRerun)))
            .child(Label::new("History").weight(FontWeight::BOLD))
            .children(branch.map(|b| {
                h_flex()
                    .gap_1()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall).color(Color::Muted))
                    .child(Label::new(b).size(LabelSize::Small).color(Color::Muted))
            }));

        let body = match &self.graph {
            Some((_, graph)) => div().flex_1().min_h_0().child(graph.clone()).into_any_element(),
            None => {
                let can_init = uninitialized_root(&self.project, cx).is_some();
                v_flex()
                    .p_4()
                    .gap_2()
                    .child(Label::new("This folder is not a git repository.").color(Color::Muted))
                    .when(can_init, |col| {
                        col.child(
                            Button::new("git-init", format!("Initialize repository ({DEFAULT_BRANCH})"))
                                .style(ButtonStyle::Filled)
                                .size(ButtonSize::Compact)
                                .on_click(|_, window, cx| window.dispatch_action(Box::new(InitRepository), cx)),
                        )
                    })
                    .into_any_element()
            }
        };

        v_flex()
            .key_context("ForgeHistoryPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.panel_background)
            .child(header)
            .child(body)
    }
}

use gpui::prelude::FluentBuilder as _;

impl Focusable for HistoryPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.graph {
            Some((_, graph)) => graph.focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

impl EventEmitter<PanelEvent> for HistoryPanel {}

impl Panel for HistoryPanel {
    fn persistent_name() -> &'static str {
        "ForgeHistoryPanel"
    }
    fn panel_key() -> &'static str {
        "ForgeHistoryPanel"
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
        gpui::BorrowAppContext::update_global::<settings::SettingsStore, _>(cx, |_, _| {});
        cx.notify();
    }
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(320.)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::HistoryRerun)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("History")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleHistory)
    }
    fn activation_priority(&self) -> u32 {
        // Forge panels use 10–19; Zed's use 1–7 and extension slots 8 and 20+.
        13
    }
}
