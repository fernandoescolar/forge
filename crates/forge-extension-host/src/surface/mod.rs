//! One extension UI surface: the React tree of a panel or an editor tab, rendered with
//! Zed's `ui` components and the active theme's colours.
//!
//! Host components (see packages/forge-api/src/index.ts): `view`, `scroll`, `text`,
//! `button`, `input` (single line, multi-line, password), `checkbox`, `icon`, `divider`,
//! `spinner`, `select`, `treeItem`, `grid` (a virtualized data table with resizable
//! columns, selection and cell editing), `markdown`, `image` and `chart` (chart.rs). Any
//! element can carry a `contextMenu`.

use crate::{
    host::ExtensionHost,
    tree::{Node, NodeId, NodeKind, Tree},
};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, DismissEvent, Entity, FocusHandle, Focusable, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    KeyBinding, MouseButton, MouseDownEvent, ParentElement as _, Pixels, Point, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, actions,
    anchored, deferred, div, prelude::FluentBuilder as _, px,
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    ops::Range,
    rc::Rc,
    str::FromStr as _,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use gpui::StyledImage as _;
use std::sync::Arc;
use theme::ActiveTheme as _;
use ui::{
    Button, ButtonCommon as _, ButtonStyle, Checkbox, Clickable as _, Color, ColumnWidthConfig, CommonAnimationExt as _, ContextMenu, Disableable as _, Divider, Icon, IconName, IconSize,
    Label, LabelCommon as _, LabelSize, ListItem, PopoverMenu, ResizableColumnsState, Table, TableInteractionState, TableResizeBehavior, ToggleState, Toggleable as _, Tooltip, h_flex,
    v_flex,
};

mod controls;
mod grid;
mod style;
mod sync;

pub(crate) use style::*;

actions!(forge_extensions, [
    /// Copies the selected rows of a data grid as tab-separated text.
    GridCopy,
    /// Asks the extension to delete the selected rows of a data grid.
    GridDelete,
]);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", menu::Confirm, Some("ForgeInput > Editor")),
        KeyBinding::new("secondary-enter", menu::SecondaryConfirm, Some("ForgeInput > Editor")),
        KeyBinding::new("enter", menu::Confirm, Some("ForgeGridCell > Editor")),
        KeyBinding::new("escape", menu::Cancel, Some("ForgeGridCell > Editor")),
        KeyBinding::new("secondary-c", GridCopy, Some("ForgeGrid")),
        KeyBinding::new("backspace", GridDelete, Some("ForgeGrid")),
        KeyBinding::new("delete", GridDelete, Some("ForgeGrid")),
    ]);
}

/// Height of a data grid row.
const ROW_HEIGHT: f32 = 24.;

struct InputState {
    editor: Entity<Editor>,
    /// `(multiline, password)` it was created for; a change recreates it.
    kind: (bool, bool),
    /// The language its text is highlighted as (Zed's name: "JSON", "SQL"…), once set.
    language: Option<String>,
    /// The extension's value at the last sync: the text changes only when that does. Shared
    /// with the editor subscription: text equal to it is not news to the extension.
    value: Rc<RefCell<String>>,
    /// What was typed and sent to the extension (`onChange`) and not yet echoed back as
    /// its value: an echo is not a change to apply. Shared with the editor subscription.
    sent: Rc<RefCell<VecDeque<String>>>,
    /// The placeholder set last: setting one builds a new editor for it.
    placeholder: String,
    _subscription: Subscription,
}

struct GridState {
    interaction: Entity<TableInteractionState>,
    widths: Entity<ResizableColumnsState>,
    /// Column names the widths were made for; other columns start over.
    columns: Vec<String>,
    /// Where the last plain click or shift-click selection started.
    anchor: Option<usize>,
    /// The cell being edited and its editor.
    editing: Option<(usize, usize, Entity<Editor>, Subscription)>,
}

struct OpenMenu {
    menu: Entity<ContextMenu>,
    position: Point<Pixels>,
    _subscription: Subscription,
}

pub struct Surface {
    pub(crate) host: Entity<ExtensionHost>,
    /// The panel or tab id whose tree this renders.
    pub panel: String,
    /// The root fills the surface (the extension handles scrolling) instead of scrolling.
    fill: bool,
    focus_handle: FocusHandle,
    inputs: HashMap<NodeId, InputState>,
    grids: HashMap<NodeId, GridState>,
    /// The rendered Markdown of `markdown` nodes, and the text it was made from.
    markdowns: HashMap<NodeId, (String, Entity<Markdown>)>,
    /// `image` nodes whose source is a `data:` URI, decoded (with that URI).
    images: HashMap<NodeId, (String, Option<Arc<gpui::Image>>)>,
    /// The point under the mouse in each chart, and where each chart's plot was painted.
    pub(crate) chart_hover: RefCell<HashMap<NodeId, usize>>,
    pub(crate) chart_bounds: Rc<RefCell<HashMap<NodeId, gpui::Bounds<Pixels>>>>,
    menu: Option<OpenMenu>,
    /// The tree generation `sync` last ran for.
    synced: Option<u64>,
    _subscription: Subscription,
}

impl Surface {
    pub fn new(host: Entity<ExtensionHost>, panel: String, fill: bool, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&host, |this, _, event: &crate::host::HostEvent, cx| match event {
            crate::host::HostEvent::Committed { panel } if *panel != this.panel => {}
            _ => cx.notify(),
        });
        Self {
            host,
            panel,
            fill,
            focus_handle: cx.focus_handle(),
            inputs: HashMap::new(),
            grids: HashMap::new(),
            markdowns: HashMap::new(),
            images: HashMap::new(),
            chart_hover: RefCell::default(),
            chart_bounds: Rc::default(),
            menu: None,
            synced: None,
            _subscription: subscription,
        }
    }

    /// The editors behind the inputs, in the order of their nodes.
    #[cfg(test)]
    pub(crate) fn input_editors(&self) -> Vec<Entity<Editor>> {
        let mut inputs: Vec<_> = self.inputs.iter().collect();
        inputs.sort_by_key(|(id, _)| **id);
        inputs.into_iter().map(|(_, i)| i.editor.clone()).collect()
    }

    /// The text of each `markdown` node's rendered Markdown, whether each `data:` image
    /// decoded, and where each chart's plot was painted.
    #[cfg(test)]
    pub(crate) fn media(&self, cx: &App) -> (Vec<String>, Vec<bool>, Vec<gpui::Bounds<Pixels>>) {
        let markdowns = self.markdowns.values().map(|(_, m)| m.read(cx).source().to_string()).collect();
        let images = self.images.values().map(|(_, image)| image.is_some()).collect();
        (markdowns, images, self.chart_bounds.borrow().values().copied().collect())
    }

    /// The language each input's text is highlighted as, by input.
    #[cfg(test)]
    pub(crate) fn input_languages(&self, cx: &App) -> Vec<Option<String>> {
        self.inputs.values().map(|i| i.editor.read(cx).buffer().read(cx).as_singleton().and_then(|b| b.read(cx).language().map(|l| l.name().to_string()))).collect()
    }

    fn tree<'a>(&self, cx: &'a App) -> Option<&'a Tree> {
        self.host.read(cx).trees.get(&self.panel)
    }

    fn render_node(&self, tree: &Tree, id: NodeId, window: &Window, cx: &Context<Self>) -> AnyElement {
        let Some(node) = tree.get(id) else { return div().into_any_element() };
        let kind = match &node.kind {
            NodeKind::Text(t) => return Label::new(t.clone()).into_any_element(),
            NodeKind::Element { kind, .. } => kind.as_str(),
        };
        if node.prop("hidden").and_then(Value::as_bool) == Some(true) {
            return div().into_any_element();
        }
        let style = node.prop("style").cloned().unwrap_or(Value::Null);
        let eid = self.element_id(id);
        let dispatch = self.dispatcher(id);
        let children = || node.children.iter().map(|c| self.render_node(tree, *c, window, cx)).collect::<Vec<_>>();

        let element = match kind {
            "text" => {
                let text = node.str_prop("text").map(str::to_string).unwrap_or_else(|| text_content(tree, id));
                let mut label = Label::new(text).size(label_size(&style));
                if let Some(c) = style.get("color").and_then(Value::as_str) {
                    label = label.color(label_color(c));
                }
                match style.get("weight").and_then(Value::as_str) {
                    Some("bold") => label = label.weight(FontWeight::BOLD),
                    Some("medium") => label = label.weight(FontWeight::MEDIUM),
                    _ => {}
                }
                if style.get("mono").and_then(Value::as_bool) == Some(true) {
                    label = label.buffer_font(cx);
                }
                if style.get("italic").and_then(Value::as_bool) == Some(true) {
                    label = label.italic();
                }
                if style.get("truncate").and_then(Value::as_bool) == Some(true) {
                    label = label.truncate();
                }
                label.into_any_element()
            }
            "button" => {
                let label = node.str_prop("label").map(str::to_string).unwrap_or_else(|| text_content(tree, id));
                let icon = node.str_prop("icon").and_then(icon_name);
                let style = match node.str_prop("variant") {
                    Some("filled") => ButtonStyle::Filled,
                    Some("ghost") => ButtonStyle::Transparent,
                    _ => ButtonStyle::Subtle,
                };
                let disabled = node.prop("disabled").and_then(Value::as_bool).unwrap_or(false);
                let selected = node.prop("selected").and_then(Value::as_bool).unwrap_or(false);
                let on_click = dispatch("onClick", Value::Null);
                let tooltip = node.str_prop("tooltip").map(|t| t.to_string());
                match (icon, label.is_empty()) {
                    (Some(icon), true) => ui::IconButton::new(eid, icon)
                        .style(style)
                        .icon_size(IconSize::Small)
                        .disabled(disabled)
                        .toggle_state(selected)
                        .when_some(tooltip, |b, t| b.tooltip(Tooltip::text(t)))
                        .on_click(move |_, _, cx| on_click(cx))
                        .into_any_element(),
                    (icon, _) => Button::new(eid, label)
                        .style(style)
                        .start_icon(icon.map(|i| Icon::new(i).size(IconSize::Small)))
                        .disabled(disabled)
                        .toggle_state(selected)
                        .when_some(tooltip, |b, t| b.tooltip(Tooltip::text(t)))
                        .on_click(move |_, _, cx| on_click(cx))
                        .into_any_element(),
                }
            }
            "checkbox" => {
                let checked = node.prop("checked").and_then(Value::as_bool).unwrap_or(false);
                let host = self.host.clone();
                let mut checkbox = Checkbox::new(eid, ToggleState::from(checked)).on_click(move |state: &ToggleState, _, cx| host.read(cx).dispatch(id, "onChange", json!(state.selected())));
                if let Some(label) = node.str_prop("label") {
                    checkbox = checkbox.label(label.to_string());
                }
                checkbox.into_any_element()
            }
            "icon" => {
                let name = node.str_prop("name").and_then(icon_name).unwrap_or(IconName::Sparkle);
                let mut icon = Icon::new(name).size(icon_size(&style));
                if let Some(c) = style.get("color").and_then(Value::as_str) {
                    icon = icon.color(label_color(c));
                }
                icon.into_any_element()
            }
            "spinner" => Icon::new(IconName::ArrowCircle).size(icon_size(&style)).color(Color::Muted).with_keyed_rotate_animation(eid, 2).into_any_element(),
            "divider" => match node.str_prop("direction") {
                Some("vertical") => Divider::vertical().into_any_element(),
                _ => Divider::horizontal().into_any_element(),
            },
            "input" => self.render_input(node, id, &style, cx),
            "select" => self.render_select(node, id, eid, cx),
            "treeItem" => self.render_tree_item(node, id, eid, cx),
            "grid" => self.render_grid(node, id, &style, cx),
            "markdown" => self.render_markdown(id, &style, window, cx),
            "image" => self.render_image(node, id, &style, cx),
            "chart" => self.render_chart(node, id, eid, &style, cx),
            // "view", "scroll" and anything unknown render as a flex container.
            _ => {
                let mut el = styled(div().id(eid), &style, cx).children(children());
                if kind == "scroll" {
                    el = el.overflow_y_scroll();
                }
                if node.has_event("onClick") {
                    let on_click = dispatch("onClick", Value::Null);
                    el = el.cursor_pointer().hover(|s| s.bg(cx.theme().colors().ghost_element_hover)).on_click(move |_, _, cx| on_click(cx));
                }
                if let Some(tip) = node.str_prop("tooltip") {
                    el = el.tooltip(Tooltip::text(tip.to_string()));
                }
                el.into_any_element()
            }
        };
        if node.prop("contextMenu").is_some_and(Value::is_array) && kind != "grid" {
            let items = node.prop("contextMenu").cloned().unwrap_or_default();
            return div()
                .on_mouse_down(MouseButton::Right, cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.deploy_menu(id, items.clone(), json!({}), event.position, window, cx);
                }))
                .child(element)
                .into_any_element();
        }
        element
    }

    fn element_id(&self, id: NodeId) -> gpui::ElementId {
        gpui::ElementId::NamedInteger(SharedString::from(format!("forge-{}", self.panel)), id as u64)
    }

    /// `dispatch(event, payload)` returns a callback that sends the event to node `id`.
    fn dispatcher(&self, id: NodeId) -> impl Fn(&'static str, Value) -> Box<dyn Fn(&mut App)> {
        let host = self.host.clone();
        move |event: &'static str, payload: Value| {
            let host = host.clone();
            Box::new(move |cx: &mut App| host.read(cx).dispatch(id, event, payload.clone()))
        }
    }

}

impl Render for Surface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Inputs, grids, Markdown and images follow the tree: redo them when it changed.
        let generation = self.tree(cx).map(|t| t.generation());
        if generation != self.synced {
            self.sync(window, cx);
            self.synced = generation;
        }
        let menu = self.menu.as_ref().map(|m| deferred(anchored().position(m.position).child(m.menu.clone())).with_priority(1));
        let content = match self.tree(cx) {
            Some(tree) => {
                let roots: Vec<_> = tree.root().children.iter().map(|c| self.render_node(tree, *c, window, cx)).collect();
                if self.fill {
                    v_flex().size_full().children(roots).into_any_element()
                } else {
                    div().id("forge-extension-content").size_full().overflow_y_scroll().child(v_flex().p_2().gap_2().children(roots)).into_any_element()
                }
            }
            None => v_flex().p_2().child(Label::new("Loading…").color(Color::Muted)).into_any_element(),
        };
        div().track_focus(&self.focus_handle).size_full().child(content).children(menu)
    }
}

impl Focusable for Surface {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ---------------------------------------------------------------------------------------
// Helpers

fn is(node: &Node, kind: &str) -> bool {
    matches!(&node.kind, NodeKind::Element { kind: k, .. } if k == kind)
}

pub(crate) fn collect(tree: &Tree, pred: impl Fn(&Node) -> bool) -> Vec<NodeId> {
    fn walk(tree: &Tree, id: NodeId, pred: &dyn Fn(&Node) -> bool, out: &mut Vec<NodeId>) {
        let Some(n) = tree.get(id) else { return };
        if pred(n) {
            out.push(id);
        }
        for c in &n.children {
            walk(tree, *c, pred, out);
        }
    }
    let mut out = vec![];
    walk(tree, crate::tree::ROOT, &pred, &mut out);
    out
}

pub(crate) fn text_content(tree: &Tree, id: NodeId) -> String {
    let Some(n) = tree.get(id) else { return String::new() };
    match &n.kind {
        NodeKind::Text(t) => t.clone(),
        NodeKind::Element { .. } => n.str_prop("text").map(str::to_string).unwrap_or_else(|| n.children.iter().map(|c| text_content(tree, *c)).collect()),
    }
}

/// A cell shows its first line, shortened.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    let mut short: String = line.chars().take(500).collect();
    if short.len() < text.len() {
        short.push('…');
    }
    short
}
