//! The Threads dock panel: the workspace's threads and where each stands, the files the
//! active one touched, the project's saved conversations, and "New thread" with the default
//! agent or any other.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::TaskExt as _;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Render, ScrollHandle, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, actions, div, px,
};
use project::Project;
use theme::ActiveTheme as _;
use ui::{
    ButtonCommon as _, ButtonLike, ButtonStyle, Clickable as _, Color, ContextMenu, Icon, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize,
    PopoverMenu, Tooltip, VisibleOnHover as _, h_flex, v_flex,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    history::{self, SessionSummary},
    settings::{self, AgentSettings},
    thread::Thread,
    threads::{self, NewThread, ThreadStore, ThreadView},
};

actions!(forge_agent, [ToggleThreadsPanel, OpenAgentSettings]);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleThreadsPanel, window, cx| {
            workspace.toggle_panel_focus::<ThreadsPanel>(window, cx);
        });
    })
    .detach();
}

pub struct ThreadsPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    store: Entity<ThreadStore>,
    settings: Entity<AgentSettings>,
    focus_handle: FocusHandle,
    position: DockPosition,
    scroll: ScrollHandle,
    saved: Vec<SessionSummary>,
    _subscriptions: Vec<Subscription>,
}

impl ThreadsPanel {
    pub fn new(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = threads::store_for(workspace.weak_handle().entity_id(), cx).expect("threads::init runs before panels are created");
        let settings = settings::global(cx);
        let mut subscriptions = vec![
            cx.observe(&store, |this: &mut Self, _, cx| {
                this.refresh_saved(cx);
                cx.notify();
            }),
            cx.observe(&settings, |this: &mut Self, _, cx| {
                this.refresh_saved(cx);
                cx.notify();
            }),
        ];
        if let Some(ws) = workspace.weak_handle().upgrade() {
            // The active tab decides which thread is highlighted.
            subscriptions.push(cx.observe_in(&ws, window, |_, _, _, cx| cx.notify()));
        }
        let mut this = Self {
            workspace: workspace.weak_handle(),
            project: workspace.project().clone(),
            store,
            settings,
            focus_handle: cx.focus_handle(),
            position: forge_ui::dock_position::load(<Self as Panel>::panel_key(), cx).unwrap_or(DockPosition::Right),
            scroll: ScrollHandle::new(),
            saved: Vec::new(),
            _subscriptions: subscriptions,
        };
        this.refresh_saved(cx);
        this
    }

    /// The project root threads save under; like `Thread`, the current directory when no
    /// folder is open.
    fn root(&self, cx: &App) -> Option<PathBuf> {
        self.project.read(cx).visible_worktrees(cx).next().map(|t| t.read(cx).abs_path().to_path_buf()).or_else(|| std::env::current_dir().ok())
    }

    /// The project's saved conversations that aren't open as threads, newest first.
    pub fn refresh_saved(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root(cx) else {
            self.saved.clear();
            return;
        };
        let open: Vec<String> = self.store.read(cx).threads().iter().filter_map(|t| t.read(cx).session_id().map(str::to_string)).collect();
        let dir = self.settings.read(cx).history_dir().clone();
        self.saved = history::list(&dir, &root).into_iter().filter(|s| !open.contains(&s.session_id)).take(30).collect();
    }

    pub fn saved(&self) -> &[SessionSummary] {
        &self.saved
    }

    /// Opens a saved conversation in a thread tab of its own; sending resumes it.
    pub fn open_saved(&mut self, summary: &SessionSummary, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        workspace.update(cx, |ws, cx| {
            let thread = cx.new(|cx| Thread::new(ws, None, window, cx));
            thread.update(cx, |t, cx| t.open_session(summary, cx));
            threads::add_thread(ws, thread, window, cx);
        });
        self.refresh_saved(cx);
    }

    /// Closes `thread`'s tab, stops its agent and drops it from the list; the conversation
    /// stays under Earlier.
    pub fn close_thread(&mut self, thread: &Entity<Thread>, window: &mut Window, cx: &mut Context<Self>) {
        thread.update(cx, |t, cx| t.disconnect(cx));
        if let Some(workspace) = self.workspace.upgrade() {
            let views: Vec<_> = workspace.read(cx).items_of_type::<ThreadView>(cx).filter(|v| v.read(cx).thread() == thread).collect();
            let panes = workspace.read(cx).panes().to_vec();
            for view in views {
                let id = view.entity_id();
                for pane in &panes {
                    pane.update(cx, |pane, cx| pane.close_item_by_id(id, workspace::SaveIntent::Skip, window, cx)).detach_and_log_err(cx);
                }
            }
        }
        self.store.update(cx, |store, cx| store.remove(thread, cx));
        self.refresh_saved(cx);
        cx.notify();
    }

    fn new_thread(&self, agent: Option<usize>, window: &mut Window, cx: &mut App) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |ws, cx| {
                threads::new_thread(ws, agent, window, cx);
            });
        }
    }

    fn active_thread(&self, cx: &App) -> Option<Entity<Thread>> {
        let workspace = self.workspace.upgrade()?;
        let item = workspace.read(cx).active_item(cx)?;
        Some(item.downcast::<ThreadView>()?.read(cx).thread().clone())
    }

    fn render_new(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = self.settings.read(cx);
        let default_label = settings.default_agent().map(|a| a.id.clone()).unwrap_or_else(|| "no agent".into());
        let agents: Vec<(usize, String)> = settings.agents().iter().enumerate().map(|(i, a)| (i, a.id.clone())).collect();
        let default = settings.default_index();
        let this = cx.entity().downgrade();
        h_flex()
            .child(
                ButtonLike::new("threads-new")
                    .style(ButtonStyle::Filled)
                    .tooltip(Tooltip::for_action_title("New thread with the default agent", &NewThread))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Icon::new(IconName::Plus).size(IconSize::XSmall))
                            .child(Label::new("New").size(LabelSize::Small))
                            .child(Label::new(default_label).size(LabelSize::XSmall).color(Color::Muted)),
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.new_thread(None, window, cx))),
            )
            .child(
                PopoverMenu::new("threads-new-with")
                    .trigger(ButtonLike::new("threads-new-with-trigger").style(ButtonStyle::Filled).child(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)).tooltip(Tooltip::text("New thread with another agent")))
                    .menu(move |window, cx| {
                        let (agents, this) = (agents.clone(), this.clone());
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            menu = menu.header("New thread with");
                            for (i, id) in agents {
                                let this = this.clone();
                                let label = if i == default { format!("{id} (default)") } else { id };
                                menu = menu.entry(label, None, move |window, cx| {
                                    this.update(cx, |panel, cx| panel.new_thread(Some(i), window, cx)).ok();
                                });
                            }
                            menu.separator().action("Agent settings…", OpenAgentSettings.boxed_clone())
                        }))
                    }),
            )
            .into_any_element()
    }

    fn render_threads(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let active = self.active_thread(cx);
        let root = self.root(cx);
        let threads = self.store.read(cx).threads().to_vec();
        if threads.is_empty() {
            return v_flex()
                .px_2()
                .py_3()
                .gap_1()
                .child(Label::new("No threads yet.").size(LabelSize::Small).color(Color::Muted))
                .child(Label::new("Start one with New, ctrl-enter in the code, or the agent button on an error.").size(LabelSize::XSmall).color(Color::Muted))
                .into_any_element();
        }
        let mut list = v_flex().gap_0p5();
        for (i, thread) in threads.iter().enumerate() {
            let t = thread.read(cx);
            let title = t.title().unwrap_or_else(|| "New thread".into());
            let (summary, color) = t.summary();
            let agent = t.agent_label();
            let selected = active.as_ref() == Some(thread);
            let files = if selected { t.touched_files() } else { Vec::new() };
            let target = thread.clone();
            let workspace = self.workspace.clone();
            list = list.child(
                h_flex()
                    .id(("thread", i))
                    .items_start()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |el| el.bg(colors.element_selected))
                    .hover(|el| el.bg(colors.element_hover))
                    .on_click(move |_, window, cx| {
                        if let Some(workspace) = workspace.upgrade() {
                            workspace.update(cx, |ws, cx| threads::open_view(ws, target.clone(), window, cx));
                        }
                    })
                    .group(format!("thread-row-{i}"))
                    .child(div().mt(px(5.)).size(px(8.)).flex_shrink_0().rounded_full().bg(color.color(cx)))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(title).size(LabelSize::Small).truncate())
                            .child(Label::new(format!("{summary} · {agent}")).size(LabelSize::XSmall).color(Color::Muted)),
                    )
                    .child({
                        let close = thread.clone();
                        IconButton::new(("close-thread", i), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Close thread (it stays under Earlier)"))
                            .visible_on_hover(format!("thread-row-{i}"))
                            .on_click(cx.listener(move |this, _, window, cx| this.close_thread(&close, window, cx)))
                    }),
            );
            for (j, (path, added, removed)) in files.into_iter().enumerate() {
                let shown = root.as_ref().and_then(|r| path.strip_prefix(r).ok()).unwrap_or(&path).to_string_lossy().into_owned();
                let workspace = self.workspace.clone();
                list = list.child(
                    h_flex()
                        .id(("touched", i * 1000 + j))
                        .justify_between()
                        .gap_2()
                        .pl_6()
                        .pr_2()
                        .py_0p5()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|el| el.bg(colors.element_hover))
                        .tooltip(Tooltip::text(shown.clone()))
                        .on_click(move |_, window, cx| open_file(&workspace, path.clone(), window, cx))
                        .child(h_flex().gap_1().min_w_0().child(Icon::new(IconName::File).size(IconSize::XSmall).color(Color::Muted)).child(Label::new(file_name(&shown)).size(LabelSize::XSmall).truncate()))
                        .child(
                            h_flex()
                                .gap_1()
                                .child(Label::new(format!("+{added}")).size(LabelSize::XSmall).color(Color::Created))
                                .child(Label::new(format!("−{removed}")).size(LabelSize::XSmall).color(Color::Deleted)),
                        ),
                );
            }
        }
        list.into_any_element()
    }

    fn render_saved(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.saved.is_empty() {
            return None;
        }
        let colors = cx.theme().colors().clone();
        let now = history::now();
        let mut list = v_flex().gap_0p5().child(div().pt_4().px_2().pb_1().child(Label::new("EARLIER").size(LabelSize::XSmall).color(Color::Muted)));
        for (i, summary) in self.saved.iter().enumerate() {
            let open = summary.clone();
            list = list.child(
                v_flex()
                    .id(("saved", i))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|el| el.bg(colors.element_hover))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_saved(&open, window, cx)))
                    .child(Label::new(summary.title.clone()).size(LabelSize::Small).color(Color::Muted).truncate())
                    .child(Label::new(format!("{} · {}", summary.agent_id, history::relative_time(summary.updated_at, now))).size(LabelSize::XSmall).color(Color::Muted)),
            );
        }
        Some(list.into_any_element())
    }
}

pub(crate) fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

pub(crate) fn open_file(workspace: &WeakEntity<Workspace>, path: PathBuf, window: &mut Window, cx: &mut App) {
    if let Some(workspace) = workspace.upgrade() {
        workspace.update(cx, |ws, cx| ws.open_abs_path(path, workspace::OpenOptions::default(), window, cx).detach_and_log_err(cx));
    }
}

impl Render for ThreadsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let error = self.settings.read(cx).error().map(str::to_string);
        let header = h_flex()
            .justify_between()
            .gap_2()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(forge_ui::panel_grip("threads-panel-grip", Arc::new(cx.entity()), "Threads", Some(IconName::ZedAgent)))
                    .child(Label::new("Threads").weight(FontWeight::BOLD)),
            )
            .child(self.render_new(cx));
        let body = div()
            .id("threads-list")
            .flex_1()
            .min_h_0()
            .p_1()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(self.render_threads(cx))
            .children(self.render_saved(cx));
        v_flex()
            .key_context("ForgeThreadsPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.panel_background)
            .child(header)
            .children(error.map(|e| div().px_2().py_1().child(Label::new(e).size(LabelSize::XSmall).color(Color::Error))))
            .child(body)
    }
}

impl Focusable for ThreadsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ThreadsPanel {}

impl Panel for ThreadsPanel {
    fn persistent_name() -> &'static str {
        "ForgeThreadsPanel"
    }
    fn panel_key() -> &'static str {
        "ForgeThreadsPanel"
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
        // workspace moves the panel now.
        gpui::BorrowAppContext::update_global::<settings_store::SettingsStore, _>(cx, |_, _| {});
        cx.notify();
    }
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(300.)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::ZedAgent)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Threads")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleThreadsPanel)
    }
    fn activation_priority(&self) -> u32 {
        // Forge panels use 10–19; Zed's use 1–7 and extension slots 8 and 20+.
        10
    }
}

use ::settings as settings_store;
