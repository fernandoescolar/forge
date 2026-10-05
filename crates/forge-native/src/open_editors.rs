//! Open Editors: every tab open in the workspace, grouped by pane when there are splits,
//! with a dot on the ones that have unsaved changes (like VS Code's). Clicking a row shows
//! that tab; its × closes it (asking to save if needed). Save All and Close All in the
//! header. A dock panel like the others: it can live on any side, or be hidden.

use std::sync::Arc;

use gpui::TaskExt as _;
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, actions, div, px,
};
use gpui::prelude::FluentBuilder as _;
use theme::ActiveTheme as _;
use ui::{
    ButtonCommon as _, Clickable as _, Color, Disableable as _, Icon, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip, VisibleOnHover as _,
    h_flex, v_flex,
};
use workspace::{
    Pane, SaveIntent, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(forge_open_editors, [ToggleFocus]);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<OpenEditorsPanel>(window, cx);
        });
    })
    .detach();
}

pub struct OpenEditorsPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    /// One per pane, kept in step with the workspace's panes.
    pane_subscriptions: Vec<(Entity<Pane>, Subscription)>,
    _workspace_subscription: Subscription,
}

/// One row: a tab of a pane.
struct Row {
    pane: Entity<Pane>,
    item_id: gpui::EntityId,
    index: usize,
    title: SharedString,
    detail: Option<SharedString>,
    icon: Option<Icon>,
    dirty: bool,
    active: bool,
}

impl OpenEditorsPanel {
    pub fn new(workspace: &mut Workspace, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace_entity = workspace.weak_handle().upgrade().expect("the workspace is alive while it adds panels");
        let workspace_subscription = cx.subscribe(&workspace_entity, |this, workspace, event: &workspace::Event, cx| {
            if matches!(event, workspace::Event::PaneAdded(_) | workspace::Event::PaneRemoved | workspace::Event::ActiveItemChanged) {
                this.watch_panes(&workspace, cx);
            }
            cx.notify();
        });
        let mut this = Self {
            workspace: workspace.weak_handle(),
            focus_handle: cx.focus_handle(),
            position: forge_ui::dock_position::load(<Self as Panel>::panel_key(), cx).unwrap_or(DockPosition::Left),
            pane_subscriptions: Vec::new(),
            _workspace_subscription: workspace_subscription,
        };
        this.watch_panes_of(workspace.panes().to_vec(), cx);
        this
    }

    fn watch_panes(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let panes = workspace.read(cx).panes().to_vec();
        self.watch_panes_of(panes, cx);
    }

    /// Panes re-render on every tab change (added, closed, renamed, edited, saved), so
    /// observing them keeps the list current.
    fn watch_panes_of(&mut self, panes: Vec<Entity<Pane>>, cx: &mut Context<Self>) {
        self.pane_subscriptions.retain(|(pane, _)| panes.contains(pane));
        for pane in panes {
            if !self.pane_subscriptions.iter().any(|(p, _)| *p == pane) {
                let subscription = cx.observe(&pane, |_, _, cx| cx.notify());
                self.pane_subscriptions.push((pane, subscription));
            }
        }
    }

    fn groups(&self, window: &Window, cx: &App) -> Vec<Vec<Row>> {
        let Some(workspace) = self.workspace.upgrade() else { return Vec::new() };
        let workspace = workspace.read(cx);
        let active_pane = workspace.active_pane().clone();
        workspace
            .panes()
            .iter()
            .map(|pane| {
                let p = pane.read(cx);
                let active_index = p.active_item_index();
                p.items()
                    .enumerate()
                    .map(|(index, item)| Row {
                        pane: pane.clone(),
                        item_id: item.item_id(),
                        index,
                        title: item.tab_content_text(0, cx),
                        detail: item.tab_tooltip_text(cx).filter(|t| *t != item.tab_content_text(0, cx)),
                        // Editors have no tab icon unless enabled for tabs: use the file's.
                        icon: item.tab_icon(window, cx).or_else(|| {
                            let path = item.project_path(cx)?;
                            file_icons::FileIcons::get_icon(path.path.as_std_path(), cx).map(Icon::from_path)
                        }),
                        dirty: item.is_dirty(cx),
                        active: index == active_index && *pane == active_pane,
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|rows| !rows.is_empty())
            .collect()
    }

    fn save_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(workspace::SaveAll { save_intent: None }.boxed_clone(), cx);
    }

    fn close_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        for pane in workspace.read(cx).panes().to_vec() {
            pane.update(cx, |pane, cx| pane.close_all_items(&Default::default(), window, cx)).detach_and_log_err(cx);
        }
    }

    fn render_row(&self, ix: usize, row: Row, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let (pane, item_id, index) = (row.pane.clone(), row.item_id, row.index);
        let close_pane = row.pane.clone();
        h_flex()
            .id(("open-editor", ix))
            .group("open-editor-row")
            .w_full()
            .h(px(24.))
            .px_2()
            .gap_1p5()
            .cursor_pointer()
            .when(row.active, |el| el.bg(colors.element_selected))
            .hover(|el| el.bg(colors.element_hover))
            .when_some(row.detail.clone(), |el, detail| el.tooltip(Tooltip::text(detail)))
            .child(
                // The × takes the dot's place on hover, as in VS Code.
                div()
                    .flex_none()
                    .w(px(16.))
                    .child(
                        div()
                            .visible_on_hover("open-editor-row")
                            .child(
                                IconButton::new(("close-editor", ix), IconName::Close)
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(Tooltip::text(if row.dirty { "Close (asks to save)" } else { "Close" }))
                                    .on_click(cx.listener(move |_, _, window, cx| {
                                        close_pane.update(cx, |pane, cx| pane.close_item_by_id(item_id, SaveIntent::Close, window, cx)).detach_and_log_err(cx);
                                    })),
                            ),
                    )
                    .when(row.dirty, |el| {
                        el.child(
                            div()
                                .absolute()
                                .top(px(4.))
                                .left(px(4.))
                                .group_hover("open-editor-row", |style| style.opacity(0.))
                                .child(Icon::new(IconName::Circle).size(IconSize::XSmall).color(Color::Modified)),
                        )
                    })
                    .relative(),
            )
            .children(row.icon.map(|icon| icon.size(IconSize::Small)))
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(row.title)
                        .size(LabelSize::Small)
                        .single_line()
                        .truncate()
                        .color(if row.dirty { Color::Modified } else { Color::Default }),
                ),
            )
            .on_click(cx.listener(move |_, _, window, cx| {
                pane.update(cx, |pane, cx| pane.activate_item(index, true, true, window, cx));
            }))
    }
}

impl Render for OpenEditorsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let groups = self.groups(window, cx);
        let count: usize = groups.iter().map(Vec::len).sum();
        let dirty = groups.iter().flatten().filter(|r| r.dirty).count();
        let several = groups.len() > 1;
        let mut list = v_flex().w_full();
        let mut ix = 0;
        for (group_ix, rows) in groups.into_iter().enumerate() {
            if several {
                list = list.child(
                    div().px_2().pt_2().pb_0p5().child(Label::new(format!("Group {}", group_ix + 1)).size(LabelSize::XSmall).color(Color::Muted)),
                );
            }
            for row in rows {
                list = list.child(self.render_row(ix, row, cx));
                ix += 1;
            }
        }
        let header = h_flex()
            .justify_between()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(forge_ui::panel_grip("open-editors-grip", Arc::new(cx.entity()), "Open Editors", Some(IconName::FileTextOutlined)))
                    .child(Label::new("Open Editors").weight(FontWeight::BOLD).single_line())
                    .when(dirty > 0, |row| row.child(Label::new(format!("{dirty} unsaved")).size(LabelSize::XSmall).color(Color::Modified))),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("save-all", IconName::CheckDouble)
                            .disabled(dirty == 0)
                            .tooltip(Tooltip::for_action_title("Save all", &workspace::SaveAll { save_intent: None }))
                            .on_click(cx.listener(|this, _, window, cx| this.save_all(window, cx))),
                    )
                    .child(
                        IconButton::new("close-all", IconName::ListX)
                            .disabled(count == 0)
                            .tooltip(Tooltip::text("Close all editors"))
                            .on_click(cx.listener(|this, _, window, cx| this.close_all(window, cx))),
                    ),
            );
        v_flex()
            .key_context("ForgeOpenEditors")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.panel_background)
            .child(header)
            .child(
                div()
                    .id("open-editors-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .py_1()
                    .when(count == 0, |el| el.child(div().p_3().child(Label::new("No open editors.").size(LabelSize::Small).color(Color::Muted))))
                    .child(list),
            )
    }
}

impl Focusable for OpenEditorsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for OpenEditorsPanel {}

impl Panel for OpenEditorsPanel {
    fn persistent_name() -> &'static str {
        "ForgeOpenEditorsPanel"
    }
    fn panel_key() -> &'static str {
        "ForgeOpenEditorsPanel"
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
        px(240.)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::FileTextOutlined)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Open Editors")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }
    fn activation_priority(&self) -> u32 {
        // Forge panels use 10–19; Zed's use 1–7 and extension slots 8 and 20+.
        15
    }
}
