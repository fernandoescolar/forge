//! Dock buttons with drag & drop.
//!
//! Replaces Zed's status-bar panel buttons with Forge's own, each next to the dock it
//! controls: a vertical strip along the left edge of the window for the left dock, one
//! along the right edge for the right dock, and a row in the status bar for the bottom
//! dock. Clicking a button opens its panel, or hides the dock when that panel is the one
//! showing; right-click moves it to another dock, and every icon can be dragged.
//! While a panel or an extension tab is being dragged, drop zones appear along the left,
//! right and bottom edges of the window.
//!
//! Panels can also be shown together in one dock ([`PanelGroups`]): drop a panel's button
//! on the middle of another's, or use "Show with" in the button's menu. Dropped on a
//! button's edge (or past the last one), it goes there instead, so the buttons can be put
//! in any order ([`PanelOrder`]).

use forge_extension_host::{ExtensionHost, panel::SLOT_KEYS};
use forge_ui::{DraggedExtensionTab, DraggedPanel};
use gpui::{
    AnchoredPositionMode, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, MouseUpEvent, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, anchored, canvas, deferred, div, point, px,
};
use std::{rc::Rc, sync::Arc};
use theme::ActiveTheme as _;
use gpui::prelude::FluentBuilder as _;
use ui::{ButtonCommon as _, ButtonLike, ButtonSize, Clickable as _, Color, ContextMenu, Icon, IconPosition, IconSize, Label, LabelCommon as _, LabelSize, Toggleable as _, Tooltip, h_flex, right_click_menu, v_flex};
use workspace::{
    ItemHandle, StatusItemView, Workspace,
    dock::{DockPosition, PanelButtons, PanelHandle},
};

pub use forge_extension_host::panel::DockEntry;

/// Panels the user took out of the side bars (by `Panel::persistent_name`), for every
/// window, remembered across restarts. Their actions still open them.
pub struct HiddenPanels(std::collections::BTreeSet<String>);

struct GlobalHiddenPanels(Entity<HiddenPanels>);
impl gpui::Global for GlobalHiddenPanels {}

const HIDDEN_KEY: &str = "forge-hidden-panels";

impl HiddenPanels {
    pub fn global(cx: &mut App) -> Entity<HiddenPanels> {
        if let Some(g) = cx.try_global::<GlobalHiddenPanels>() {
            return g.0.clone();
        }
        let saved = db::kvp::KeyValueStore::global(cx).read_kvp(HIDDEN_KEY).ok().flatten();
        let names: std::collections::BTreeSet<String> = saved.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let entity = cx.new(|_| HiddenPanels(names));
        cx.set_global(GlobalHiddenPanels(entity.clone()));
        entity
    }

    pub fn is_hidden(&self, name: &str) -> bool {
        self.0.contains(name)
    }

    pub fn set_hidden(&mut self, name: &str, hidden: bool, cx: &mut Context<Self>) {
        let changed = if hidden { self.0.insert(name.to_string()) } else { self.0.remove(name) };
        if !changed {
            return;
        }
        let kvp = db::kvp::KeyValueStore::global(cx);
        let value = serde_json::to_string(&self.0).unwrap_or_default();
        cx.background_spawn(async move { kvp.write_kvp(HIDDEN_KEY.to_string(), value).await.ok() }).detach();
        cx.notify();
    }
}

/// The order of the panel buttons (by `Panel::persistent_name`), for every window,
/// remembered across restarts. Panels it doesn't name go last, in the order Forge adds them.
pub struct PanelOrder(Vec<String>);

struct GlobalPanelOrder(Entity<PanelOrder>);
impl gpui::Global for GlobalPanelOrder {}

const ORDER_KEY: &str = "forge-panel-order";

impl PanelOrder {
    pub fn global(cx: &mut App) -> Entity<PanelOrder> {
        if let Some(g) = cx.try_global::<GlobalPanelOrder>() {
            return g.0.clone();
        }
        let saved = db::kvp::KeyValueStore::global(cx).read_kvp(ORDER_KEY).ok().flatten();
        let names: Vec<String> = saved.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let entity = cx.new(|_| PanelOrder(names));
        cx.set_global(GlobalPanelOrder(entity.clone()));
        entity
    }

    /// Sorts `items` into button order (stably: the panels it doesn't name keep theirs).
    fn sort<T>(&self, items: &mut [T], name: impl Fn(&T) -> &str) {
        items.sort_by_key(|item| self.0.iter().position(|n| n == name(item)).unwrap_or(usize::MAX));
    }

    /// Puts `name` right before (or, `after`, right after) `target`, or last without one.
    /// `current` is every panel, in button order.
    fn place(&mut self, name: &str, target: Option<(&str, bool)>, current: Vec<String>, cx: &mut Context<Self>) {
        let mut order: Vec<String> = current.into_iter().filter(|n| n != name).collect();
        let at = target.and_then(|(t, after)| order.iter().position(|n| n == t).map(|ix| ix + after as usize)).unwrap_or(order.len());
        order.insert(at, name.to_string());
        if order == self.0 {
            return;
        }
        self.0 = order;
        let kvp = db::kvp::KeyValueStore::global(cx);
        let value = serde_json::to_string(&self.0).unwrap_or_default();
        cx.background_spawn(async move { kvp.write_kvp(ORDER_KEY.to_string(), value).await.ok() }).detach();
        cx.notify();
    }
}

/// Where a dragged panel button would land: on another button (shown together with it),
/// or before or after it.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Spot {
    Before,
    On,
    After,
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct DropHint {
    target: &'static str,
    spot: Spot,
}

/// Drops `dragged` on the button strip of the dock at `position`: shows it together with
/// the button it is on, or puts its button where it was dropped (last, `last` being the
/// strip's last button, when not over a button), moving it to that dock.
fn drop_on_strip(
    dragged: &DraggedPanel,
    hint: Option<DropHint>,
    position: DockPosition,
    last: Option<&'static str>,
    workspace: &WeakEntity<Workspace>,
    registry: &Entity<PanelRegistry>,
    window: &mut Window,
    cx: &mut App,
) {
    let name = dragged.panel.persistent_name();
    if let Some(DropHint { target, spot: Spot::On }) = hint {
        let with = registry.read(cx).entries.iter().find(|e| e.handle.persistent_name() == target).map(|e| e.handle.clone());
        if let Some(with) = with.filter(|_| target != name) {
            show_together(name, with, dragged.panel.clone(), workspace, window, cx);
        }
        return;
    }
    if !dragged.panel.position_is_valid(position, cx) {
        return;
    }
    let target = match hint {
        Some(hint) => Some((hint.target, hint.spot == Spot::After)),
        None => last.map(|last| (last, true)),
    };
    if target.is_some_and(|(t, _)| t == name) {
        return;
    }
    let order = PanelOrder::global(cx);
    let mut current: Vec<String> = registry.read(cx).entries.iter().map(|e| e.handle.persistent_name().to_string()).collect();
    order.read(cx).sort(&mut current, |n| n.as_str());
    order.update(cx, |o, cx| o.place(name, target, current, cx));
    if dragged.panel.position(window, cx) != position {
        forge_ui::move_panel(dragged.panel.clone(), position, workspace.clone(), window, cx);
    }
}

/// Panels shown together in a dock (by `Panel::persistent_name`, in order), remembered
/// across restarts. A group only applies to its members that share a dock.
pub struct PanelGroups(Vec<Vec<String>>, std::collections::BTreeMap<String, f32>);

struct GlobalPanelGroups(Entity<PanelGroups>);
impl gpui::Global for GlobalPanelGroups {}

const GROUPS_KEY: &str = "forge-panel-groups";
/// How much of its group's space each panel takes (only those resized).
const WEIGHTS_KEY: &str = "forge-panel-weights";

impl PanelGroups {
    pub fn global(cx: &mut App) -> Entity<PanelGroups> {
        if let Some(g) = cx.try_global::<GlobalPanelGroups>() {
            return g.0.clone();
        }
        let saved = db::kvp::KeyValueStore::global(cx).read_kvp(GROUPS_KEY).ok().flatten();
        let groups: Vec<Vec<String>> = saved.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let saved = db::kvp::KeyValueStore::global(cx).read_kvp(WEIGHTS_KEY).ok().flatten();
        let weights = saved.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let entity = cx.new(|_| PanelGroups(groups, weights));
        cx.set_global(GlobalPanelGroups(entity.clone()));
        entity
    }

    pub fn groups(&self) -> &[Vec<String>] {
        &self.0
    }

    pub fn weight(&self, name: &str) -> Option<f32> {
        self.1.get(name).copied()
    }

    /// Records the panels' shares of their groups (`None`: back to an even share).
    fn set_weights(&mut self, weights: Vec<(&'static str, Option<f32>)>, cx: &mut Context<Self>) {
        let mut changed = false;
        for (name, weight) in weights {
            let previous = match weight {
                Some(weight) => self.1.insert(name.to_string(), weight),
                None => self.1.remove(name),
            };
            changed |= previous != weight;
        }
        if changed {
            let kvp = db::kvp::KeyValueStore::global(cx);
            let value = serde_json::to_string(&self.1).unwrap_or_default();
            cx.background_spawn(async move { kvp.write_kvp(WEIGHTS_KEY.to_string(), value).await.ok() }).detach();
        }
    }

    pub fn group_of(&self, name: &str) -> Option<&Vec<String>> {
        self.0.iter().find(|g| g.iter().any(|n| n == name))
    }

    /// Shows `name` together with `with` (after the panels already shown with it).
    pub fn join(&mut self, name: &str, with: &str, cx: &mut Context<Self>) {
        if name == with {
            return;
        }
        self.remove(name);
        match self.0.iter_mut().find(|g| g.iter().any(|n| n == with)) {
            Some(group) => group.push(name.to_string()),
            None => self.0.push(vec![with.to_string(), name.to_string()]),
        }
        self.save(cx);
    }

    /// Shows `name` on its own again.
    pub fn leave(&mut self, name: &str, cx: &mut Context<Self>) {
        self.remove(name);
        self.save(cx);
    }

    fn remove(&mut self, name: &str) {
        for group in &mut self.0 {
            group.retain(|n| n != name);
        }
        self.0.retain(|g| g.len() > 1);
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let kvp = db::kvp::KeyValueStore::global(cx);
        let value = serde_json::to_string(&self.0).unwrap_or_default();
        cx.background_spawn(async move { kvp.write_kvp(GROUPS_KEY.to_string(), value).await.ok() }).detach();
        cx.notify();
    }
}

/// Gives each dock the groups of the panels it holds (only when they changed: docks
/// notify, and this runs when they do).
fn apply_groups(workspace: &Workspace, registry: &Entity<PanelRegistry>, window: &mut Window, cx: &mut App) {
    let groups = PanelGroups::global(cx).read(cx).groups().to_vec();
    let handles: Vec<Arc<dyn PanelHandle>> = registry.read(cx).entries.iter().map(|e| e.handle.clone()).collect();
    for dock in [workspace.left_dock().clone(), workspace.bottom_dock().clone(), workspace.right_dock().clone()] {
        let position = dock.read(cx).position();
        let wanted: Vec<Vec<gpui::EntityId>> = groups
            .iter()
            .map(|group| {
                group
                    .iter()
                    .filter_map(|name| handles.iter().find(|h| h.persistent_name() == name && h.position(window, cx) == position).map(|h| h.panel_id()))
                    .collect::<Vec<_>>()
            })
            .filter(|ids| ids.len() > 1)
            .collect();
        if dock.read(cx).panel_groups() != wanted.as_slice() {
            dock.update(cx, |dock, cx| dock.set_panel_groups(wanted, window, cx));
        }
    }
}

/// Gives the docks the panel sizes saved last time.
fn restore_weights(workspace: &Workspace, registry: &Entity<PanelRegistry>, cx: &mut App) {
    let groups = PanelGroups::global(cx);
    let handles: Vec<Arc<dyn PanelHandle>> = registry.read(cx).entries.iter().map(|e| e.handle.clone()).collect();
    let weights: std::collections::HashMap<gpui::EntityId, f32> =
        handles.iter().filter_map(|h| groups.read(cx).weight(h.persistent_name()).map(|w| (h.panel_id(), w))).collect();
    if weights.is_empty() {
        return;
    }
    for dock in [workspace.left_dock().clone(), workspace.bottom_dock().clone(), workspace.right_dock().clone()] {
        let weights = weights.clone();
        dock.update(cx, |dock, cx| dock.set_panel_weights(weights, cx));
    }
}

/// Remembers the sizes the user gave the panels of `dock`'s groups.
fn save_weights(dock: &Entity<workspace::dock::Dock>, registry: &Entity<PanelRegistry>, window: &Window, cx: &mut App) {
    let position = dock.read(cx).position();
    let current = dock.read(cx).panel_weights().clone();
    let weights: Vec<(&'static str, Option<f32>)> = registry
        .read(cx)
        .entries
        .iter()
        .filter(|e| e.handle.position(window, cx) == position)
        .map(|e| (e.handle.persistent_name(), current.get(&e.handle.panel_id()).copied()))
        .collect();
    PanelGroups::global(cx).update(cx, |g, cx| g.set_weights(weights, cx));
}

/// Shows `name` together with `with`, moving it to `with`'s dock if needed, and shows
/// the group.
fn show_together(name: &'static str, with: Arc<dyn PanelHandle>, panel: Arc<dyn PanelHandle>, workspace: &WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    let target = with.position(window, cx);
    if panel.position(window, cx) != target {
        forge_ui::move_panel(panel, target, workspace.clone(), window, cx);
    }
    PanelGroups::global(cx).update(cx, |g, cx| g.join(name, with.persistent_name(), cx));
    let Some(workspace) = workspace.upgrade() else { return };
    let action = with.toggle_action(window, cx);
    workspace.update(cx, |workspace, cx| {
        let dock = workspace.dock_at_position(target).clone();
        if !dock.read(cx).shown_panels().iter().any(|p| p.panel_id() == with.panel_id()) {
            window.dispatch_action(action, cx);
        }
    });
}

gpui::actions!(forge, [ShowOrHidePanels]);

/// View › Show or Hide Panels…: every panel, shown or hidden; picking one switches it.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ShowOrHidePanels, window, cx| show_or_hide_panels(workspace, window, cx));
    })
    .detach();
}

fn show_or_hide_panels(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
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
fn set_panel_hidden(name: &str, hide: bool, workspace: &WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
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
struct Registries(std::collections::HashMap<gpui::EntityId, Entity<PanelRegistry>>);
impl gpui::Global for Registries {}

/// Every panel Forge added to a workspace, in button order.
pub struct PanelRegistry {
    pub entries: Vec<DockEntry>,
}

/// Swaps Zed's panel buttons for Forge's draggable ones.
pub fn install(workspace: &mut Workspace, entries: Vec<DockEntry>, extension_host: Option<Entity<ExtensionHost>>, window: &mut Window, cx: &mut Context<Workspace>) {
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

pub struct DockButtons {
    position: DockPosition,
    /// Exactly one instance draws the window-wide drop zones.
    draws_drop_zones: bool,
    registry: Entity<PanelRegistry>,
    workspace: WeakEntity<Workspace>,
    extension_host: Option<Entity<ExtensionHost>>,
    /// Where the panel being dragged over this strip would land.
    drop_hint: Option<DropHint>,
    _subscriptions: Vec<Subscription>,
}

impl DockButtons {
    fn new(
        position: DockPosition,
        draws_drop_zones: bool,
        registry: Entity<PanelRegistry>,
        workspace: WeakEntity<Workspace>,
        docks: &[Entity<workspace::dock::Dock>; 3],
        extension_host: Option<Entity<ExtensionHost>>,
        cx: &mut Context<Self>,
    ) -> Self {
        // Panels move between docks, so watch all of them (and extension tabs, which
        // change slot titles/icons).
        let mut subscriptions: Vec<Subscription> = docks.iter().map(|d| cx.observe(d, |_, _, cx| cx.notify())).collect();
        // Hiding or showing a panel, from any window.
        let hidden = HiddenPanels::global(cx);
        subscriptions.push(cx.observe(&hidden, |_, _, cx| cx.notify()));
        // Reordering the buttons, from any window.
        let order = PanelOrder::global(cx);
        subscriptions.push(cx.observe(&order, |_, _, cx| cx.notify()));
        if let Some(host) = &extension_host {
            subscriptions.push(cx.observe(host, |_, _, cx| cx.notify()));
        }
        Self { position, draws_drop_zones, registry, workspace, extension_host, drop_hint: None, _subscriptions: subscriptions }
    }

    fn button(&self, ix: usize, handle: Arc<dyn PanelHandle>, title: SharedString, window: &Window, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let icon = handle.icon(window, cx)?;
        let workspace = self.workspace.upgrade()?;
        let dock = workspace.read(cx).dock_at_position(self.position).clone();
        let is_active = dock.read(cx).shown_panels().iter().any(|p| p.panel_id() == handle.panel_id());
        // The other panels on this side, for "Show with".
        let hidden = HiddenPanels::global(cx);
        let others: Vec<(Arc<dyn PanelHandle>, SharedString)> = self
            .registry
            .read(cx)
            .entries
            .iter()
            .filter(|e| e.handle.panel_id() != handle.panel_id() && e.handle.position(window, cx) == self.position && e.handle.enabled(cx))
            .filter(|e| !hidden.read(cx).is_hidden(e.handle.persistent_name()))
            .map(|e| (e.handle.clone(), (e.title)(cx)))
            .collect();
        let group: Vec<String> = PanelGroups::global(cx).read(cx).group_of(handle.persistent_name()).cloned().unwrap_or_default();
        let action = handle.toggle_action(window, cx);
        let handle_name = handle.persistent_name();
        let current = self.position;
        let (menu_panel, menu_ws) = (handle.clone(), self.workspace.clone());
        let (others, group) = (others.clone(), group.clone());
        let name: SharedString = format!("forge-dock-{}-{ix}", forge_ui::dock_name(self.position)).into();
        let tooltip = title.clone();
        let vertical = self.position != DockPosition::Bottom;
        let button = right_click_menu(name.clone())
            .menu(move |window, cx| {
                let (panel, ws) = (menu_panel.clone(), menu_ws.clone());
                let (others, group) = (others.clone(), group.clone());
                ContextMenu::build(window, cx, move |mut menu, _, cx| {
                    for position in [DockPosition::Left, DockPosition::Right, DockPosition::Bottom] {
                        if panel.position_is_valid(position, cx) {
                            let (panel, ws) = (panel.clone(), ws.clone());
                            menu = menu.toggleable_entry(format!("Dock {}", forge_ui::dock_label(position)), position == current, IconPosition::Start, None, move |window, cx| {
                                forge_ui::move_panel(panel.clone(), position, ws.clone(), window, cx)
                            });
                        }
                    }
                    if !others.is_empty() {
                        menu = menu.separator();
                        for (other, title) in &others {
                            let together = group.iter().any(|n| n == other.persistent_name());
                            let (panel, other, ws) = (panel.clone(), other.clone(), ws.clone());
                            menu = menu.toggleable_entry(format!("Show with {title}"), together, IconPosition::Start, None, move |window, cx| {
                                let name = panel.persistent_name();
                                if together {
                                    PanelGroups::global(cx).update(cx, |g, cx| g.leave(name, cx));
                                } else {
                                    show_together(name, other.clone(), panel.clone(), &ws, window, cx);
                                }
                            });
                        }
                        if !group.is_empty() {
                            let name = panel.persistent_name();
                            menu = menu.entry("Show Alone", None, move |_, cx| PanelGroups::global(cx).update(cx, |g, cx| g.leave(name, cx)));
                        }
                    }
                    let (name, ws) = (panel.persistent_name(), ws.clone());
                    menu.separator()
                        .entry("Hide from the Sidebar", None, move |window, cx| set_panel_hidden(name, true, &ws, window, cx))
                        .entry("Show or Hide Panels…", Some(Box::new(ShowOrHidePanels)), |window, cx| window.dispatch_action(Box::new(ShowOrHidePanels), cx))
                })
            })
            .trigger(move |_, _, _| {
                // Forge's panels use Forge's icons; the open panel's icon takes the accent.
                let glyph = match forge_ui::panel_icon(handle_name) {
                    Some(path) => Icon::from_path(path),
                    None => Icon::new(icon),
                };
                ButtonLike::new(gpui::ElementId::NamedInteger(name.clone(), is_active as u64))
                    .size(if vertical { ButtonSize::Large } else { ButtonSize::Default })
                    .toggle_state(is_active)
                    .tooltip(Tooltip::text(tooltip.clone()))
                    .child(glyph.size(if vertical { IconSize::Medium } else { IconSize::Small }).color(if is_active { Color::Accent } else { Color::Muted }))
                    .on_click({
                        // The open panel's button hides its dock; the others open their panel.
                        let (action, dock) = (action.boxed_clone(), dock.downgrade());
                        move |_, window, cx| {
                            if is_active {
                                dock.update(cx, |dock, cx| dock.set_open(false, window, cx)).ok();
                            } else {
                                window.dispatch_action(action.boxed_clone(), cx)
                            }
                        }
                    })
            });
        let accent = cx.theme().colors().border_focused;
        // Dropping a panel on the middle of this button shows the two together; on its
        // leading or trailing edge, puts the panel's button before or after this one (the
        // strip handles the drop).
        let spot = self.drop_hint.filter(|h| h.target == handle_name && cx.has_active_drag()).map(|h| h.spot);
        let this = cx.weak_entity();
        let insertion_bar = |after: bool| {
            let bar = div().absolute().bg(accent);
            let bar = if vertical { bar.left_0().right_0().h(px(2.)) } else { bar.top_0().bottom_0().w(px(2.)) };
            match (vertical, after) {
                (true, false) => bar.top(px(-4.)),
                (true, true) => bar.bottom(px(-4.)),
                (false, false) => bar.left(px(-3.)),
                (false, true) => bar.right(px(-3.)),
            }
        };
        Some(
            div()
                .id(("forge-dock-drag", ix))
                .debug_selector(|| format!("forge-dock-button-{}", handle_name))
                .relative()
                .rounded_md()
                .border_1()
                .border_color(if spot == Some(Spot::On) { accent } else { gpui::transparent_black() })
                .on_drag_move::<DraggedPanel>(move |e, _, cx| {
                    let (b, p) = (e.bounds, e.event.position);
                    if !b.contains(&p) {
                        return;
                    }
                    let hint = (e.drag(cx).panel.persistent_name() != handle_name).then(|| {
                        let f = if vertical { (p.y - b.origin.y) / b.size.height } else { (p.x - b.origin.x) / b.size.width };
                        let spot = if f < 0.3 { Spot::Before } else if f > 0.7 { Spot::After } else { Spot::On };
                        DropHint { target: handle_name, spot }
                    });
                    this.update(cx, |this, cx| {
                        if this.drop_hint != hint {
                            this.drop_hint = hint;
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .when(spot == Some(Spot::Before), |d| d.child(insertion_bar(false)))
                .when(spot == Some(Spot::After), |d| d.child(insertion_bar(true)))
                .child(button)
                .on_drag(DraggedPanel { panel: handle, title, icon: Some(icon) }, |d, _, _, cx| forge_ui::drag_preview(d.title.clone(), d.icon, cx))
                .into_any_element(),
        )
    }

    fn drop_zones(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = cx.theme().colors().clone();
        let size = window.viewport_size();
        let (w, h) = (f32::from(size.width), f32::from(size.height));
        let zone = |position: DockPosition, x: f32, y: f32, zw: f32, zh: f32| {
            let (ws, host, registry) = (self.workspace.clone(), self.extension_host.clone(), self.registry.clone());
            let (ws2, accent, fill) = (self.workspace.clone(), colors.border_focused, colors.drop_target_background);
            div()
                .id(SharedString::from(format!("forge-drop-{}", forge_ui::dock_name(position))))
                .debug_selector(move || format!("forge-drop-{}", forge_ui::dock_name(position)))
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(zw))
                .h(px(zh))
                .flex()
                .items_center()
                .justify_center()
                .rounded_lg()
                .border_2()
                .border_dashed()
                .border_color(accent.opacity(0.5))
                .bg(fill.opacity(0.35))
                .drag_over::<DraggedPanel>(move |s, _, _, _| s.bg(fill).border_color(accent))
                .drag_over::<DraggedExtensionTab>(move |s, _, _, _| s.bg(fill).border_color(accent))
                .child(Label::new(format!("Dock {}", forge_ui::dock_label(position))).size(LabelSize::Small))
                .on_drop(move |d: &DraggedPanel, window, cx| {
                    forge_ui::end_drag(cx);
                    forge_ui::move_panel(d.panel.clone(), position, ws.clone(), window, cx);
                })
                .on_drop(move |d: &DraggedExtensionTab, window, cx| {
                    forge_ui::end_drag(cx);
                    let Some(host) = &host else { return };
                    // Give the tab a slot of its own and put that slot on this side.
                    let Some(slot) = host.update(cx, |h, cx| h.move_to_free_slot(&d.tab, cx)) else { return };
                    let key = SLOT_KEYS[slot];
                    let handle = registry.read(cx).entries.iter().find(|e| e.handle.persistent_name() == key).map(|e| e.handle.clone());
                    if let Some(handle) = handle {
                        forge_ui::move_panel(handle, position, ws2.clone(), window, cx);
                    }
                })
        };
        let top = 44.0;
        let bottom_bar = 32.0;
        // Clear of the button strips along the edges: dropping on a button groups panels.
        let strip = 44.0;
        let side_w = (w * 0.18).clamp(120.0, 260.0);
        let bottom_h = (h * 0.2).clamp(90.0, 200.0);
        let overlay = div()
            .size_full()
            .child(zone(DockPosition::Left, strip, top, side_w, h - top - bottom_bar - bottom_h - 16.0))
            .child(zone(DockPosition::Right, w - side_w - strip, top, side_w, h - top - bottom_bar - bottom_h - 16.0))
            .child(zone(DockPosition::Bottom, 8.0, h - bottom_bar - bottom_h - 4.0, w - 16.0, bottom_h))
            // Releasing the mouse anywhere ends the drag (drops outside a zone do nothing).
            .child(
                canvas(|_, _, _| {}, |_, _, window, _| {
                    window.on_mouse_event(|_: &MouseUpEvent, phase, _, cx| {
                        if phase.bubble() {
                            forge_ui::end_drag(cx);
                        }
                    })
                })
                .size_0(),
            );
        deferred(anchored().position_mode(AnchoredPositionMode::Window).position(point(px(0.), px(0.))).child(v_flex().w(px(w)).h(px(h)).child(overlay)))
            .with_priority(10)
            .into_any_element()
    }
}

impl Render for DockButtons {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Registry indexes keep the buttons' element ids stable when they are reordered.
        let mut entries: Vec<(usize, Arc<dyn PanelHandle>, Rc<dyn Fn(&App) -> SharedString>)> =
            self.registry.read(cx).entries.iter().enumerate().map(|(ix, e)| (ix, e.handle.clone(), e.title.clone())).collect();
        PanelOrder::global(cx).read(cx).sort(&mut entries, |(_, handle, _)| handle.persistent_name());
        let hidden = HiddenPanels::global(cx);
        let mut buttons = Vec::new();
        let mut last = None;
        for (ix, handle, title) in entries {
            if handle.position(window, cx) != self.position || !handle.enabled(cx) || hidden.read(cx).is_hidden(handle.persistent_name()) {
                continue;
            }
            let title = title(cx);
            let name = handle.persistent_name();
            if let Some(button) = self.button(ix, handle, title, window, cx) {
                buttons.push(button);
                last = Some(name);
            }
        }
        let drop_zones = (self.draws_drop_zones && forge_ui::drag_in_progress(cx)).then(|| self.drop_zones(window, cx));
        // Dropping a panel's button on the strip: see `DockButtons::button`.
        let (this, this_for_drop) = (cx.weak_entity(), cx.weak_entity());
        let (ws, registry, position) = (self.workspace.clone(), self.registry.clone(), self.position);
        let droppable = move |strip: gpui::Stateful<gpui::Div>| {
            strip
                .on_drag_move::<DraggedPanel>(move |e, _, cx| {
                    if !e.bounds.contains(&e.event.position) {
                        this.update(cx, |this, cx| {
                            if this.drop_hint.take().is_some() {
                                cx.notify();
                            }
                        })
                        .ok();
                    }
                })
                .on_drop(move |d: &DraggedPanel, window, cx| {
                    forge_ui::end_drag(cx);
                    let hint = this_for_drop.update(cx, |this, cx| {
                        cx.notify();
                        this.drop_hint.take()
                    });
                    drop_on_strip(d, hint.ok().flatten(), position, last, &ws, &registry, window, cx);
                })
        };
        if self.position == DockPosition::Bottom {
            let row = h_flex().id("forge-dock-strip-bottom").gap_0p5().children(buttons);
            return h_flex().child(droppable(row)).children(drop_zones);
        }
        // A strip along the window edge; nothing at all when no panel lives on that side.
        let colors = cx.theme().colors();
        h_flex().h_full().children(drop_zones).when(!buttons.is_empty(), |strip| {
            strip.child(
                droppable(v_flex().id(SharedString::from(format!("forge-dock-strip-{}", forge_ui::dock_name(self.position)))))
                    .h_full()
                    .flex_none()
                    .px_1()
                    .py_1()
                    .gap_1()
                    .items_center()
                    .bg(colors.status_bar_background)
                    .border_color(colors.border)
                    .map(|strip| if self.position == DockPosition::Left { strip.border_r_1() } else { strip.border_l_1() })
                    .overflow_y_scroll()
                    .children(buttons),
            )
        })
    }
}

impl StatusItemView for DockButtons {
    fn set_active_pane_item(&mut self, _: Option<&dyn ItemHandle>, _: &mut Window, _: &mut Context<Self>) {}
    /// Always shown: no "Hide Button" entry.
    fn hide_setting(&self, _: &gpui::App) -> Option<workspace::HideStatusItem> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, Modifiers, MouseButton, Pixels, Point, TestAppContext, VisualTestContext};

    /// Each test gets its own database: hidden panels, groups and their sizes are saved
    /// there, and tests running in parallel must not see each other's.
    fn test_app_state(cx: &mut App) -> Arc<workspace::AppState> {
        cx.set_global(db::AppDatabase::test_new());
        workspace::AppState::test(cx)
    }

    fn center(b: Bounds<Pixels>) -> Point<Pixels> {
        b.center()
    }

    /// Clicking the button of the panel that is showing hides its dock; clicking it again
    /// brings the panel back.
    #[gpui::test]
    async fn clicking_the_open_panel_button_hides_its_dock(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let params = cx.update(test_app_state);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            forge_output::panel::init(cx);
        });
        let project = project::Project::test(params.fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, |ws, window, cx| {
            let panel = cx.new(|cx| forge_output::OutputPanel::new(ws, window, cx));
            ws.add_panel(panel.clone(), window, cx);
            let entries = vec![DockEntry { handle: Arc::new(panel), title: Rc::new(|_| "Output".into()) }];
            install(ws, entries, None, window, cx);
            ws.focus_panel::<forge_output::OutputPanel>(window, cx);
        });
        cx.run_until_parked();
        let bottom_open = |cx: &mut VisualTestContext| workspace.read_with(cx, |ws, cx| ws.bottom_dock().read(cx).is_open());
        assert!(bottom_open(cx), "the Output panel is showing");

        let icon = cx.debug_bounds("forge-dock-button-ForgeOutputPanel").expect("Output icon in the status bar");
        cx.simulate_click(center(icon), Modifiers::none());
        cx.run_until_parked();
        assert!(!bottom_open(cx), "clicking the open panel's button hides the dock");

        let icon = cx.debug_bounds("forge-dock-button-ForgeOutputPanel").expect("Output icon in the status bar");
        cx.simulate_click(center(icon), Modifiers::none());
        cx.run_until_parked();
        assert!(bottom_open(cx), "clicking it again shows the panel");
    }

    /// A panel hidden from the side bars loses its button (and its dock closes if it was
    /// showing); shown again, it comes back.
    #[gpui::test]
    async fn hides_and_shows_a_panel(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let params = cx.update(test_app_state);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            forge_output::panel::init(cx);
        });
        let project = project::Project::test(params.fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        workspace.update_in(cx, |ws, window, cx| {
            let panel = cx.new(|cx| forge_output::OutputPanel::new(ws, window, cx));
            ws.add_panel(panel.clone(), window, cx);
            let entries = vec![DockEntry { handle: Arc::new(panel), title: Rc::new(|_| "Output".into()) }];
            install(ws, entries, None, window, cx);
            ws.focus_panel::<forge_output::OutputPanel>(window, cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("forge-dock-button-ForgeOutputPanel").is_some());

        let weak = workspace.downgrade();
        cx.update(|window, cx| set_panel_hidden("ForgeOutputPanel", true, &weak, window, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("forge-dock-button-ForgeOutputPanel").is_none(), "no button once hidden");
        assert!(!workspace.read_with(cx, |ws, cx| ws.bottom_dock().read(cx).is_open()), "its dock closed");

        cx.update(|window, cx| set_panel_hidden("ForgeOutputPanel", false, &weak, window, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("forge-dock-button-ForgeOutputPanel").is_some(), "back after showing it");
    }

    /// Two panels shown together: the second moves to the first's dock, both are on
    /// screen at once and both buttons light up; "Show Alone" splits them again.
    #[gpui::test]
    async fn shows_panels_together(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let params = cx.update(test_app_state);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            forge_output::panel::init(cx);
            forge_tests::init(cx);
        });
        let project = project::Project::test(params.fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let (output, tests) = workspace.update_in(cx, |ws, window, cx| {
            let output = cx.new(|cx| forge_output::OutputPanel::new(ws, window, cx));
            ws.add_panel(output.clone(), window, cx);
            let tests = cx.new(|cx| forge_tests::TestPanel::new(ws, window, cx));
            ws.add_panel(tests.clone(), window, cx);
            let entries = vec![
                DockEntry { handle: Arc::new(output.clone()), title: Rc::new(|_| "Output".into()) },
                DockEntry { handle: Arc::new(tests.clone()), title: Rc::new(|_| "Tests".into()) },
            ];
            install(ws, entries, None, window, cx);
            ws.focus_panel::<forge_output::OutputPanel>(window, cx);
            (output, tests)
        });
        cx.run_until_parked();
        let shown = |cx: &mut VisualTestContext| workspace.read_with(cx, |ws, cx| ws.bottom_dock().read(cx).shown_panels().iter().map(|p| p.persistent_name()).collect::<Vec<_>>());
        assert_eq!(shown(cx), ["ForgeOutputPanel"]);

        let weak = workspace.downgrade();
        let (with, panel): (Arc<dyn PanelHandle>, Arc<dyn PanelHandle>) = (Arc::new(output), Arc::new(tests));
        cx.update(|window, cx| show_together("ForgeTestPanel", with, panel, &weak, window, cx));
        cx.run_until_parked();
        assert_eq!(shown(cx), ["ForgeOutputPanel", "ForgeTestPanel"], "both on screen, in the bottom dock");
        assert!(cx.debug_bounds("forge-dock-button-ForgeTestPanel").is_some());

        // Dragging the divider between them resizes them, and the sizes are remembered.
        let divider = cx.debug_bounds("dock-group-divider-1").expect("a divider between the two");
        let start = divider.center();
        cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::none());
        for step in 1..=10 {
            cx.simulate_mouse_move(start - point(px(15. * step as f32), px(0.)), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        }
        cx.simulate_mouse_up(start - point(px(150.), px(0.)), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
        let (output_share, tests_share) = cx.update(|_, cx| {
            let groups = PanelGroups::global(cx);
            (groups.read(cx).weight("ForgeOutputPanel").unwrap_or(1.), groups.read(cx).weight("ForgeTestPanel").unwrap_or(1.))
        });
        assert!(output_share < tests_share, "the left panel got smaller: {output_share} vs {tests_share}");

        cx.update(|_, cx| PanelGroups::global(cx).update(cx, |g, cx| g.leave("ForgeTestPanel", cx)));
        cx.run_until_parked();
        assert_eq!(shown(cx), ["ForgeTestPanel"], "alone again: the dock shows just its active panel");
    }

    /// Dropping one panel's button on another's (in another dock) shows them together.
    #[gpui::test]
    async fn dropping_a_button_on_another_groups_them(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let params = cx.update(test_app_state);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            forge_output::panel::init(cx);
            forge_tests::init(cx);
        });
        let project = project::Project::test(params.fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        workspace.update_in(cx, |ws, window, cx| {
            let output = cx.new(|cx| forge_output::OutputPanel::new(ws, window, cx));
            ws.add_panel(output.clone(), window, cx);
            let tests = cx.new(|cx| forge_tests::TestPanel::new(ws, window, cx));
            ws.add_panel(tests.clone(), window, cx);
            let entries = vec![
                DockEntry { handle: Arc::new(output), title: Rc::new(|_| "Output".into()) },
                DockEntry { handle: Arc::new(tests), title: Rc::new(|_| "Tests".into()) },
            ];
            install(ws, entries, None, window, cx);
        });
        cx.run_until_parked();
        let output_button = cx.debug_bounds("forge-dock-button-ForgeOutputPanel").unwrap();
        let tests_button = cx.debug_bounds("forge-dock-button-ForgeTestPanel").unwrap();
        cx.simulate_mouse_down(center(output_button), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(center(output_button) + gpui::point(gpui::px(20.), gpui::px(-20.)), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        // The drop zones leave the button strips free (in the app the left zone sits level
        // with the buttons).
        let left_zone = cx.debug_bounds("forge-drop-left").expect("drop zones while dragging");
        assert!(left_zone.origin.x >= tests_button.origin.x + tests_button.size.width, "the left zone clears the strip: {left_zone:?} vs {tests_button:?}");
        cx.simulate_mouse_move(center(tests_button), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(center(tests_button), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        let shown = workspace.read_with(cx, |ws, cx| ws.left_dock().read(cx).shown_panels().iter().map(|p| p.persistent_name()).collect::<Vec<_>>());
        assert_eq!(shown, ["ForgeTestPanel", "ForgeOutputPanel"], "Output joined Tests in the left dock");
        cx.update(|_, cx| PanelGroups::global(cx).update(cx, |g, cx| g.leave("ForgeOutputPanel", cx)));
    }

    /// Dropping a button on another's trailing edge puts it after that one, in the same
    /// strip, without grouping them; the order is remembered.
    #[gpui::test]
    async fn dropping_a_button_between_others_reorders_them(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let params = cx.update(test_app_state);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            forge_output::panel::init(cx);
            forge_tests::init(cx);
        });
        let project = project::Project::test(params.fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        workspace.update_in(cx, |ws, window, cx| {
            let output = cx.new(|cx| forge_output::OutputPanel::new(ws, window, cx));
            ws.add_panel(output.clone(), window, cx);
            let tests = cx.new(|cx| forge_tests::TestPanel::new(ws, window, cx));
            ws.add_panel(tests.clone(), window, cx);
            let entries = vec![
                DockEntry { handle: Arc::new(output), title: Rc::new(|_| "Output".into()) },
                DockEntry { handle: Arc::new(tests), title: Rc::new(|_| "Tests".into()) },
            ];
            install(ws, entries, None, window, cx);
        });
        cx.run_until_parked();
        // Put Output in the left strip, below Tests.
        let output_button = cx.debug_bounds("forge-dock-button-ForgeOutputPanel").unwrap();
        let tests_button = cx.debug_bounds("forge-dock-button-ForgeTestPanel").unwrap();
        let below_tests = point(tests_button.center().x, tests_button.bottom() - px(2.));
        cx.simulate_mouse_down(center(output_button), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(center(output_button) + point(px(20.), px(-20.)), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_move(below_tests, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_up(below_tests, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        let in_left = workspace.read_with(cx, |ws, cx| ws.left_dock().read(cx).panel::<forge_output::OutputPanel>().is_some());
        assert!(in_left, "Output moved to the left dock");
        assert!(cx.update(|_, cx| PanelGroups::global(cx).read(cx).group_of("ForgeOutputPanel").is_none()), "not shown together");
        let (output_button, tests_button) =
            (cx.debug_bounds("forge-dock-button-ForgeOutputPanel").unwrap(), cx.debug_bounds("forge-dock-button-ForgeTestPanel").unwrap());
        assert!(output_button.origin.y > tests_button.origin.y, "Output's button is below Tests'");

        // Now drop Output on Tests' leading edge: it goes first.
        let above_tests = point(tests_button.center().x, tests_button.top() + px(2.));
        cx.simulate_mouse_down(center(output_button), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(center(output_button) + point(px(20.), px(-20.)), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_move(above_tests, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_up(above_tests, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        let (output_button, tests_button) =
            (cx.debug_bounds("forge-dock-button-ForgeOutputPanel").unwrap(), cx.debug_bounds("forge-dock-button-ForgeTestPanel").unwrap());
        assert!(output_button.origin.y < tests_button.origin.y, "Output's button is above Tests' now");
        let order = cx.update(|_, cx| PanelOrder::global(cx).read(cx).0.clone());
        assert_eq!(order, ["ForgeOutputPanel", "ForgeTestPanel"]);
    }

    /// Drags the Output panel's status-bar icon onto the left drop zone with real mouse
    /// events and checks the workspace moved it.
    #[gpui::test]
    async fn dragging_a_dock_icon_moves_the_panel(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let params = cx.update(test_app_state);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
        });
        let project = project::Project::test(params.fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let output = workspace.update_in(cx, |ws, window, cx| {
            let panel = cx.new(|cx| forge_output::OutputPanel::new(ws, window, cx));
            ws.add_panel(panel.clone(), window, cx);
            let entries = vec![DockEntry { handle: Arc::new(panel.clone()), title: Rc::new(|_| "Output".into()) }];
            install(ws, entries, None, window, cx);
            panel
        });
        cx.run_until_parked();
        let still_zed_buttons = workspace.read_with(cx, |ws, cx| ws.status_bar().read(cx).item_of_type::<PanelButtons>().is_some());
        assert!(!still_zed_buttons, "Zed's panel buttons were replaced");

        let icon = cx.debug_bounds("forge-dock-button-ForgeOutputPanel").expect("Output icon in the status bar");
        assert!(cx.debug_bounds("forge-drop-left").is_none(), "no drop zones before dragging");

        // Press, move past the drag threshold, then over the left zone, and release.
        cx.simulate_mouse_down(center(icon), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(center(icon) + gpui::point(gpui::px(20.), gpui::px(-20.)), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        let left_zone = cx.debug_bounds("forge-drop-left").expect("drop zones appear while dragging");
        cx.simulate_mouse_move(center(left_zone), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(center(left_zone), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        let (in_left, open) = workspace.read_with(cx, |ws, cx| {
            let dock = ws.left_dock().read(cx);
            (dock.panel::<forge_output::OutputPanel>().is_some(), dock.is_open())
        });
        assert!(in_left, "the Output panel now lives in the left dock");
        assert!(open, "and the left dock was opened on it");
        assert!(cx.debug_bounds("forge-drop-left").is_none(), "drop zones are gone after the drop");
        drop(output);
    }
}
