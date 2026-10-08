//! UI pieces shared by Forge's crates: panel drag & drop and dock persistence.
//!
//! Dragging works with two payloads: [`DraggedPanel`] (any dock panel: from its status-bar
//! icon or from the header of Forge's own panels) and [`DraggedExtensionTab`] (an
//! extension panel inside an extension slot). Drop zones along the window edges are drawn
//! by `forge-native` while [`drag_in_progress`] is true.

pub mod agent_tools;
pub mod dock_position;
pub mod pick;
pub mod process;
pub mod settings_registry;

use gpui::{
    App, AppContext as _, Entity, Global, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    WeakEntity, Window, div,
};
use std::{sync::Arc, time::Duration};
use theme::ActiveTheme as _;
use ui::{Color, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip, h_flex};
use workspace::{
    Workspace,
    dock::{DockPosition, PanelHandle},
};

/// Sends `prompt` to the agent panel, connecting the default agent first if needed.
/// Lives here so any Forge crate can ask the agent without depending on forge-agents.
#[derive(Clone, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = forge_agent)]
#[serde(deny_unknown_fields)]
pub struct AskAgent {
    pub prompt: String,
}

/// Opens the three-pane merge editor (forge-git) on `path`, or on the active file, or lets
/// the user pick a conflicted file. Lives here so any Forge crate can open it.
#[derive(Clone, Default, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = forge_git)]
#[serde(deny_unknown_fields)]
pub struct OpenMergeEditor {
    #[serde(default)]
    pub path: Option<String>,
}

/// Opens the Settings tab on one of its pages (`ext:<extension id>` for an extension's).
/// Lives here so any Forge crate can link to its settings.
#[derive(Clone, Default, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = forge)]
#[serde(deny_unknown_fields)]
pub struct OpenSettingsPage {
    pub page: String,
}

/// Forge's own icon for one of its panels (by `Panel::persistent_name`), an SVG under
/// `icons/` served by forge-native's asset source. Zed's panels keep Zed's icons.
pub fn panel_icon(persistent_name: &str) -> Option<&'static str> {
    Some(match persistent_name {
        "ForgeTestPanel" => "icons/forge_tests.svg",
        "ForgeThreadsPanel" => "icons/forge_agents.svg",
        "ForgeExtensionsPanel" => "icons/forge_extensions.svg",
        "ForgeOutputPanel" => "icons/forge_output.svg",
        "ForgeHistoryPanel" => "icons/forge_history.svg",
        "GitPanel" => "icons/forge_git.svg",
        "ForgeSolutionExplorer" => "icons/forge_solution.svg",
        _ => return None,
    })
}

/// The icon at the start of a panel's header, which doubles as its drag handle: the
/// panel's own icon when it has one (Forge's SVG, see [`panel_icon`], or `drag_icon`),
/// three dots otherwise.
pub fn panel_grip(id: &'static str, panel: Arc<dyn PanelHandle>, title: impl Into<SharedString>, drag_icon: Option<IconName>) -> impl IntoElement {
    let glyph = match (panel_icon(panel.persistent_name()), drag_icon) {
        (Some(path), _) => Icon::from_path(path).size(IconSize::Small).color(Color::Accent),
        (None, Some(icon)) => Icon::new(icon).size(IconSize::Small).color(Color::Accent),
        (None, None) => Icon::new(IconName::Ellipsis).size(IconSize::Small).color(Color::Muted),
    };
    div()
        .id(id)
        .flex_none()
        .cursor_grab()
        .tooltip(Tooltip::text("Drag to move this panel to another side"))
        .child(glyph)
        .on_drag(DraggedPanel { panel, title: title.into(), icon: drag_icon }, |d, _, _, cx| drag_preview(d.title.clone(), d.icon, cx))
}

/// A dock panel being dragged to another dock.
#[derive(Clone)]
pub struct DraggedPanel {
    pub panel: Arc<dyn PanelHandle>,
    pub title: SharedString,
    pub icon: Option<IconName>,
}

/// An extension panel (tab) being dragged between extension slots or to a window edge.
#[derive(Clone, Debug)]
pub struct DraggedExtensionTab {
    pub tab: String,
    pub title: SharedString,
    pub from_slot: usize,
}

/// The chip that follows the cursor while dragging.
pub struct DragPreview {
    title: SharedString,
    icon: Option<IconName>,
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        h_flex()
            .gap_1()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border_focused)
            .bg(colors.elevated_surface_background)
            .shadow_md()
            .children(self.icon.map(|i| Icon::new(i).size(IconSize::Small).color(Color::Accent)))
            .child(Label::new(self.title.clone()).size(LabelSize::Small))
    }
}

#[derive(Default)]
struct DragState(bool);
impl Global for DragState {}

/// Builds the drag preview and marks a Forge drag as in progress (drop zones appear).
pub fn drag_preview(title: SharedString, icon: Option<IconName>, cx: &mut App) -> Entity<DragPreview> {
    cx.set_global(DragState(true));
    cx.new(|_| DragPreview { title, icon })
}

pub fn drag_in_progress(cx: &App) -> bool {
    cx.try_global::<DragState>().is_some_and(|s| s.0)
}

pub fn end_drag(cx: &mut App) {
    if drag_in_progress(cx) {
        cx.set_global(DragState(false));
        cx.refresh_windows();
    }
}

/// Moves `panel` to the dock at `position`, then opens that dock on it.
///
/// Zed's panels persist their dock in settings.json (written asynchronously) and Forge's
/// in the key-value store; either way the workspace re-docks the panel when it notices,
/// so this waits briefly for the panel to arrive before activating it.
pub fn move_panel(panel: Arc<dyn PanelHandle>, position: DockPosition, workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    if !panel.position_is_valid(position, cx) {
        return;
    }
    if panel.position(window, cx) != position {
        panel.set_position(position, window, cx);
    }
    let name = panel.persistent_name();
    window
        .spawn(cx, async move |cx| {
            for _ in 0..60 {
                let opened = workspace
                    .update_in(cx, |ws, window, cx| {
                        let dock = ws.dock_at_position(position).clone();
                        let Some(ix) = dock.read(cx).panel_index_for_persistent_name(name, cx) else { return false };
                        dock.update(cx, |dock, cx| {
                            dock.activate_panel(ix, window, cx);
                            dock.set_open(true, window, cx);
                        });
                        true
                    })
                    .unwrap_or(true);
                if opened {
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(50)).await;
            }
        })
        .detach();
}

/// "Left" / "Right" / "Bottom".
pub fn dock_label(position: DockPosition) -> &'static str {
    match position {
        DockPosition::Left => "Left",
        DockPosition::Right => "Right",
        DockPosition::Bottom => "Bottom",
    }
}

pub fn dock_name(position: DockPosition) -> &'static str {
    match position {
        DockPosition::Left => "left",
        DockPosition::Right => "right",
        DockPosition::Bottom => "bottom",
    }
}
