//! What the user arranged, remembered across restarts: hidden panels, the buttons' order
//! and the panels shown together (with their sizes).

use super::*;

/// Panels the user took out of the side bars (by `Panel::persistent_name`), for every
/// window, remembered across restarts. Their actions still open them.
pub struct HiddenPanels(std::collections::BTreeSet<String>);

pub(super) struct GlobalHiddenPanels(Entity<HiddenPanels>);
impl gpui::Global for GlobalHiddenPanels {}

pub(super) const HIDDEN_KEY: &str = "forge-hidden-panels";

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
pub struct PanelOrder(pub(super) Vec<String>);

pub(super) struct GlobalPanelOrder(Entity<PanelOrder>);
impl gpui::Global for GlobalPanelOrder {}

pub(super) const ORDER_KEY: &str = "forge-panel-order";

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
    pub(super) fn sort<T>(&self, items: &mut [T], name: impl Fn(&T) -> &str) {
        items.sort_by_key(|item| self.0.iter().position(|n| n == name(item)).unwrap_or(usize::MAX));
    }

    /// Puts `name` right before (or, `after`, right after) `target`, or last without one.
    /// `current` is every panel, in button order.
    pub(super) fn place(&mut self, name: &str, target: Option<(&str, bool)>, current: Vec<String>, cx: &mut Context<Self>) {
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

/// Panels shown together in a dock (by `Panel::persistent_name`, in order), remembered
/// across restarts. A group only applies to its members that share a dock.
pub struct PanelGroups(Vec<Vec<String>>, std::collections::BTreeMap<String, f32>);

pub(super) struct GlobalPanelGroups(Entity<PanelGroups>);
impl gpui::Global for GlobalPanelGroups {}

pub(super) const GROUPS_KEY: &str = "forge-panel-groups";
/// How much of its group's space each panel takes (only those resized).
pub(super) const WEIGHTS_KEY: &str = "forge-panel-weights";

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
    pub(super) fn set_weights(&mut self, weights: Vec<(&'static str, Option<f32>)>, cx: &mut Context<Self>) {
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

    pub(super) fn remove(&mut self, name: &str) {
        for group in &mut self.0 {
            group.retain(|n| n != name);
        }
        self.0.retain(|g| g.len() > 1);
    }

    pub(super) fn save(&mut self, cx: &mut Context<Self>) {
        let kvp = db::kvp::KeyValueStore::global(cx);
        let value = serde_json::to_string(&self.0).unwrap_or_default();
        cx.background_spawn(async move { kvp.write_kvp(GROUPS_KEY.to_string(), value).await.ok() }).detach();
        cx.notify();
    }
}

/// Gives each dock the groups of the panels it holds (only when they changed: docks
/// notify, and this runs when they do).
pub(super) fn apply_groups(workspace: &Workspace, registry: &Entity<PanelRegistry>, window: &mut Window, cx: &mut App) {
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
pub(super) fn restore_weights(workspace: &Workspace, registry: &Entity<PanelRegistry>, cx: &mut App) {
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
pub(super) fn save_weights(dock: &Entity<workspace::dock::Dock>, registry: &Entity<PanelRegistry>, window: &Window, cx: &mut App) {
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
pub(super) fn show_together(name: &'static str, with: Arc<dyn PanelHandle>, panel: Arc<dyn PanelHandle>, workspace: &WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
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
