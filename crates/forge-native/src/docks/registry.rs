//! The panels of each window (Zed's and extensions'), View › Show or Hide Panels, and
//! installing the dock buttons in a workspace.

use super::*;

gpui::actions!(forge, [ShowOrHidePanels]);

/// Shows a built-in panel and focuses it, putting its button back if it was hidden
/// (View › Panels). `title` is its `DockEntry`'s.
#[derive(PartialEq, Clone, serde::Deserialize, Default, schemars::JsonSchema, gpui::Action)]
#[action(namespace = forge)]
#[serde(deny_unknown_fields)]
pub struct OpenPanel {
    pub title: String,
}

/// The built-in panels: their `DockEntry` titles, and their names in View › Panels.
/// Every panel Forge adds must be here, so that it can be shown again once hidden.
pub const BUILT_IN_PANELS: &[(&str, &str)] = &[
    ("Project", "Project"),
    ("Open Editors", "Open Editors"),
    ("Solution", "Solution Explorer"),
    ("Terminal", "Terminal"),
    ("Debug", "Debug"),
    ("Debug Console", "Debug Console"),
    ("Threads", "Threads"),
    ("Tests", "Tests"),
    ("Output", "Output"),
    ("Git", "Git Changes"),
    ("History", "Git History"),
    ("Extensions", "Extensions"),
];

/// View › Show or Hide Panels…: every panel, shown or hidden; picking one switches it.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ShowOrHidePanels, window, cx| show_or_hide_panels(workspace, window, cx));
        workspace.register_action(|workspace, action: &OpenPanel, window, cx| open_panel(workspace, &action.title, window, cx));
    })
    .detach();
}

pub(super) fn open_panel(workspace: &mut Workspace, title: &str, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(registry) = cx.try_global::<Registries>().and_then(|r| r.0.get(&cx.entity_id()).cloned()) else { return };
    let Some(handle) = registry.read(cx).entries.iter().find(|e| (e.title)(cx) == title).map(|e| e.handle.clone()) else { return };
    HiddenPanels::global(cx).update(cx, |h, cx| h.set_hidden(handle.persistent_name(), false, cx));
    let dock = workspace.dock_at_position(handle.position(window, cx)).clone();
    let shown = dock.read(cx).shown_panels().iter().any(|p| p.panel_id() == handle.panel_id());
    if shown {
        handle.panel_focus_handle(cx).focus(window, cx);
    } else {
        window.dispatch_action(handle.toggle_action(window, cx), cx);
    }
}

pub(super) fn show_or_hide_panels(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(registry) = cx.try_global::<Registries>().and_then(|r| r.0.get(&cx.entity_id()).cloned()) else { return };
    let hidden = HiddenPanels::global(cx);
    // Extension slots with nothing in them have no button and no name: leave them out.
    let panels: Vec<(&'static str, SharedString)> = registry
        .read(cx)
        .entries
        .iter()
        .filter(|e| e.handle.enabled(cx))
        .map(|e| (e.handle.persistent_name(), (e.title)(cx)))
        .filter(|(_, title)| !title.is_empty())
        .collect();
    let choices = panels
        .iter()
        .map(|(name, title)| forge_ui::pick::Choice::new(title.clone()).detail(if hidden.read(cx).is_hidden(name) { "hidden — pick to show it" } else { "shown — pick to hide it" }))
        .collect();
    let weak = workspace.weak_handle();
    forge_ui::pick::pick(workspace, "Show or hide which panel?", choices, window, cx, move |ix, window, cx| {
        let Some(&(name, _)) = panels.get(ix) else { return };
        let hide = !hidden.read(cx).is_hidden(name);
        set_panel_hidden(name, hide, &weak, window, cx);
    });
}

/// Hides `name` from the side bars (closing its dock if it is the one showing), or
/// shows it again.
pub(super) fn set_panel_hidden(name: &str, hide: bool, workspace: &WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    HiddenPanels::global(cx).update(cx, |h, cx| h.set_hidden(name, hide, cx));
    if !hide {
        return;
    }
    let Some(workspace) = workspace.upgrade() else { return };
    workspace.update(cx, |workspace, cx| {
        for dock in [workspace.left_dock().clone(), workspace.right_dock().clone(), workspace.bottom_dock().clone()] {
            let showing = dock.read(cx).is_open() && dock.read(cx).visible_panel().is_some_and(|p| p.persistent_name() == name);
            if showing {
                dock.update(cx, |dock, cx| dock.set_open(false, window, cx));
            }
        }
    });
}

/// Each workspace's panel registry, for View › Show or Hide Panels….
#[derive(Default)]
pub(super) struct Registries(std::collections::HashMap<gpui::EntityId, Entity<PanelRegistry>>);
impl gpui::Global for Registries {}

/// Every panel Forge added to a workspace, in button order.
pub struct PanelRegistry {
    pub entries: Vec<DockEntry>,
}

/// Swaps Zed's panel buttons for Forge's draggable ones.
pub fn install(workspace: &mut Workspace, entries: Vec<DockEntry>, extension_host: Option<Entity<ExtensionHost>>, window: &mut Window, cx: &mut Context<Workspace>) {
    debug_assert!(
        entries.iter().all(|e| SLOT_KEYS.contains(&e.handle.persistent_name()) || BUILT_IN_PANELS.iter().any(|(title, _)| *title == (e.title)(cx).as_ref())),
        "a panel missing from BUILT_IN_PANELS (View › Panels)"
    );
    let registry = cx.new(|_| PanelRegistry { entries });
    let workspace_id = cx.entity_id();
    cx.default_global::<Registries>().0.insert(workspace_id, registry.clone());
    cx.on_release(move |_, cx| {
        cx.default_global::<Registries>().0.remove(&workspace_id);
    })
    .detach();
    let weak = workspace.weak_handle();
    let docks = [workspace.left_dock().clone(), workspace.bottom_dock().clone(), workspace.right_dock().clone()];
    let make = |position: DockPosition, draws_drop_zones: bool, cx: &mut Context<Workspace>| {
        let registry = registry.clone();
        let (weak, docks, host) = (weak.clone(), docks.clone(), extension_host.clone());
        cx.new(move |cx| DockButtons::new(position, draws_drop_zones, registry, weak, &docks, host, cx))
    };
    let left = make(DockPosition::Left, true, cx);
    let bottom = make(DockPosition::Bottom, false, cx);
    let right = make(DockPosition::Right, false, cx);
    workspace.status_bar().update(cx, |bar, cx| {
        while let Some(ix) = bar.position_of_item::<PanelButtons>() {
            bar.remove_item_at(ix, cx);
        }
        bar.add_left_item(bottom, window, cx);
    });
    workspace.set_edge_item(DockPosition::Left, Some(left.into()), cx);
    workspace.set_edge_item(DockPosition::Right, Some(right.into()), cx);

    // Panel groups follow their setting and the panels' moves between docks.
    apply_groups(workspace, &registry, window, cx);
    restore_weights(workspace, &registry, cx);
    let groups = PanelGroups::global(cx);
    let registry_for_groups = registry.clone();
    cx.observe_in(&groups, window, move |workspace, _, window, cx| apply_groups(workspace, &registry_for_groups, window, cx)).detach();
    for dock in docks {
        let registry = registry.clone();
        cx.observe_in(&dock, window, move |workspace, dock, window, cx| {
            apply_groups(workspace, &registry, window, cx);
            save_weights(&dock, &registry, window, cx);
        })
        .detach();
    }
}
