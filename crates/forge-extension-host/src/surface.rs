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
    _subscription: Subscription,
}

impl Surface {
    pub fn new(host: Entity<ExtensionHost>, panel: String, fill: bool, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&host, |_, _, cx| cx.notify());
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

    /// Creates or updates the entities behind `input`, `grid` and `markdown` nodes (and
    /// decodes `data:` images) before rendering.
    pub(crate) fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_media(cx);
        let (inputs, grids) = {
            let Some(tree) = self.tree(cx) else { return };
            let inputs: Vec<(NodeId, String, String, bool, bool, bool, Option<String>)> = collect(tree, |n| is(n, "input"))
                .into_iter()
                .map(|id| {
                    let n = tree.get(id).unwrap();
                    let flag = |k: &str| n.prop(k).and_then(Value::as_bool).unwrap_or(false);
                    let language = n.str_prop("language").filter(|l| !l.is_empty()).map(str::to_string);
                    (id, n.str_prop("value").unwrap_or_default().to_string(), n.str_prop("placeholder").unwrap_or_default().to_string(), flag("multiline"), flag("password"), flag("autoFocus"), language)
                })
                .collect();
            let grids: Vec<(NodeId, Vec<(String, Option<f64>)>)> = collect(tree, |n| is(n, "grid"))
                .into_iter()
                .map(|id| {
                    let columns = tree.get(id).unwrap().prop("columns").and_then(Value::as_array).cloned().unwrap_or_default();
                    let columns = columns.iter().map(|c| (c.get("name").and_then(Value::as_str).unwrap_or_default().to_string(), c.get("width").and_then(Value::as_f64))).collect();
                    (id, columns)
                })
                .collect();
            (inputs, grids)
        };

        self.inputs.retain(|id, _| inputs.iter().any(|(w, ..)| w == id));
        for (id, value, placeholder, multiline, password, auto_focus, language) in inputs {
            if self.inputs.get(&id).is_some_and(|s| s.kind != (multiline, password)) {
                self.inputs.remove(&id);
            }
            let created = !self.inputs.contains_key(&id);
            if created {
                let editor = cx.new(|cx| {
                    let mut editor = if multiline { Editor::auto_height(4, 24, window, cx) } else { Editor::single_line(window, cx) };
                    if password {
                        editor.set_masked(true, cx);
                    }
                    editor
                });
                let host = self.host.clone();
                let sent: Rc<RefCell<VecDeque<String>>> = Rc::default();
                let sent_by_editor = sent.clone();
                let known: Rc<RefCell<String>> = Rc::new(RefCell::new(value.clone()));
                let known_by_editor = known.clone();
                let subscription = cx.subscribe(&editor, move |_, editor, event: &EditorEvent, cx| {
                    if matches!(event, EditorEvent::Edited { .. }) {
                        let text = editor.read(cx).text(cx);
                        // The text the extension set (or already has) is not a change to report,
                        // as in React: setting `value` doesn't call `onChange`. Reporting it
                        // would bounce values back and forth with late echoes.
                        if *known_by_editor.borrow() == text {
                            return;
                        }
                        let mut sent = sent_by_editor.borrow_mut();
                        sent.push_back(text.clone());
                        if sent.len() > 64 {
                            sent.pop_front();
                        }
                        drop(sent);
                        host.read(cx).dispatch(id, "onChange", json!(text));
                    }
                });
                editor.update(cx, |e, cx| e.set_text(value.as_str(), window, cx));
                sent.borrow_mut().clear();
                self.inputs.insert(id, InputState { editor, kind: (multiline, password), language: None, value: known, sent, _subscription: subscription });
            }
            if self.inputs[&id].language != language {
                self.inputs.get_mut(&id).unwrap().language = language.clone();
                set_language(&self.inputs[&id].editor, language, cx);
            }
            // Controlled input: the extension's value replaces the text when the extension
            // changes it (clearing after a submit, loading a record), not when it merely
            // hasn't caught up with what is being typed: it runs on its own thread, and a
            // redraw before it answers would otherwise undo the keys.
            let state = self.inputs.get_mut(&id).unwrap();
            let changed = *state.value.borrow() != value;
            let echo = changed && {
                let mut sent = state.sent.borrow_mut();
                match sent.iter().position(|text| *text == value) {
                    Some(at) => {
                        sent.drain(..=at);
                        true
                    }
                    None => false,
                }
            };
            *state.value.borrow_mut() = value.clone();
            let editor = state.editor.clone();
            let sent = state.sent.clone();
            editor.update(cx, |e, cx| {
                if changed && !echo && e.text(cx) != value {
                    e.set_text(value.as_str(), window, cx);
                    // What was typed before is overwritten: its echoes are no longer ours.
                    sent.borrow_mut().clear();
                } else if !changed && e.text(cx) == value {
                    // Caught up: nothing typed is waiting for its echo.
                    sent.borrow_mut().clear();
                }
                e.set_placeholder_text(&placeholder, window, cx);
            });
            if created && auto_focus {
                editor.focus_handle(cx).focus(window, cx);
            }
        }

        self.grids.retain(|id, _| grids.iter().any(|(w, _)| w == id));
        for (id, columns) in grids {
            let names: Vec<String> = columns.iter().map(|(n, _)| n.clone()).collect();
            if self.grids.get(&id).is_some_and(|g| g.columns == names) {
                continue;
            }
            // The row-number column, then one per data column.
            let mut widths = vec![px(48.)];
            widths.extend(columns.iter().map(|(name, width)| px(width.map(|w| w as f32).unwrap_or_else(|| (name.chars().count() as f32 * 8. + 48.).clamp(90., 240.)))));
            let mut behavior = vec![TableResizeBehavior::None];
            behavior.extend(columns.iter().map(|_| TableResizeBehavior::MinSize(2.)));
            let cols = widths.len();
            let state = self.grids.remove(&id);
            let interaction = state.map(|s| s.interaction).unwrap_or_else(|| cx.new(|cx| TableInteractionState::new(cx)));
            let widths = cx.new(|_| ResizableColumnsState::new(cols, widths, behavior));
            self.grids.insert(id, GridState { interaction, widths, columns: names, anchor: None, editing: None });
        }
    }

    fn sync_media(&mut self, cx: &mut Context<Self>) {
        let (markdowns, images) = {
            let Some(tree) = self.tree(cx) else { return };
            let markdowns: Vec<(NodeId, String)> = collect(tree, |n| is(n, "markdown")).into_iter().map(|id| (id, tree.get(id).unwrap().str_prop("text").unwrap_or_default().to_string())).collect();
            let images: Vec<(NodeId, String)> = collect(tree, |n| is(n, "image"))
                .into_iter()
                .filter_map(|id| Some((id, tree.get(id).unwrap().str_prop("src")?.to_string())))
                .filter(|(_, src)| src.starts_with("data:"))
                .collect();
            (markdowns, images)
        };
        self.markdowns.retain(|id, _| markdowns.iter().any(|(m, _)| m == id));
        let languages = self.host.read(cx).workspace().map(|ws| ws.read(cx).app_state().languages.clone());
        for (id, text) in markdowns {
            match self.markdowns.get_mut(&id) {
                Some((known, markdown)) if *known != text => {
                    *known = text.clone();
                    markdown.update(cx, |m, cx| m.reset(text.into(), cx));
                }
                Some(_) => {}
                None => {
                    let markdown = cx.new(|cx| Markdown::new(text.clone().into(), languages.clone(), None, cx));
                    self.markdowns.insert(id, (text, markdown));
                }
            }
        }
        self.images.retain(|id, _| images.iter().any(|(i, _)| i == id));
        for (id, src) in images {
            if self.images.get(&id).is_none_or(|(known, _)| *known != src) {
                let image = decode_data_uri(&src).map(Arc::new);
                if image.is_none() {
                    log::warn!("extension image {id}: not a base64 data: URI of a known image type");
                }
                self.images.insert(id, (src, image));
            }
        }
    }

    fn render_markdown(&self, id: NodeId, style: &Value, window: &Window, cx: &Context<Self>) -> AnyElement {
        let Some((_, markdown)) = self.markdowns.get(&id) else { return div().into_any_element() };
        let font = if style.get("mono").and_then(Value::as_bool) == Some(true) { MarkdownFont::Editor } else { MarkdownFont::Agent };
        styled(div().min_w_0(), style, cx).child(MarkdownElement::new(markdown.clone(), MarkdownStyle::themed(font, window, cx))).into_any_element()
    }

    fn render_image(&self, node: &Node, id: NodeId, style: &Value, cx: &Context<Self>) -> AnyElement {
        let src = node.str_prop("src").unwrap_or_default();
        let alt = node.str_prop("alt").map(str::to_string);
        let source: Option<gpui::ImageSource> = if src.starts_with("data:") {
            self.images.get(&id).and_then(|(_, image)| image.clone()).map(Into::into)
        } else if src.starts_with("http://") || src.starts_with("https://") {
            Some(SharedString::from(src.to_string()).into())
        } else if std::path::Path::new(src).is_absolute() {
            Some(std::path::PathBuf::from(src).into())
        } else {
            None
        };
        let fit = match node.str_prop("fit") {
            Some("cover") => gpui::ObjectFit::Cover,
            Some("fill") => gpui::ObjectFit::Fill,
            Some("none") => gpui::ObjectFit::None,
            _ => gpui::ObjectFit::Contain,
        };
        let fallback_text = alt.clone().unwrap_or_else(|| "Image not found".into());
        let element = match source {
            Some(source) => gpui::img(source)
                .object_fit(fit)
                .size_full()
                .with_fallback(move || Label::new(fallback_text.clone()).size(LabelSize::Small).color(Color::Muted).into_any_element())
                .into_any_element(),
            None => Label::new(fallback_text).size(LabelSize::Small).color(Color::Muted).into_any_element(),
        };
        let mut container = styled(div().id(self.element_id(id)), style, cx);
        // Without a size the image takes its own; with one it fits in it.
        if style.get("width").is_none() && style.get("height").is_none() {
            container = container.w_full().h(px(node.prop("height").and_then(Value::as_f64).unwrap_or(160.) as f32));
        }
        container.when_some(alt, |el, alt| el.tooltip(Tooltip::text(alt))).child(element).into_any_element()
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

    fn render_input(&self, node: &Node, id: NodeId, style: &Value, cx: &Context<Self>) -> AnyElement {
        let Some(input) = self.inputs.get(&id) else { return div().into_any_element() };
        let editor = input.editor.clone();
        let (host, submit_editor) = (self.host.clone(), editor.clone());
        let (secondary_host, secondary_editor) = (self.host.clone(), editor.clone());
        let colors = cx.theme().colors();
        let multiline = node.prop("multiline").and_then(Value::as_bool).unwrap_or(false);
        styled(div(), style, cx)
            .key_context("ForgeInput")
            .on_action(move |_: &menu::Confirm, _, cx| {
                if !multiline {
                    let text = submit_editor.read(cx).text(cx);
                    host.read(cx).dispatch(id, "onSubmit", json!(text));
                }
            })
            .on_action(move |_: &menu::SecondaryConfirm, _, cx| {
                let text = secondary_editor.read(cx).text(cx);
                secondary_host.read(cx).dispatch(id, "onSubmit", json!(text));
            })
            .px_2()
            .py_1()
            .border_1()
            .border_color(colors.border)
            .rounded_md()
            .bg(colors.editor_background)
            .child(editor)
            .into_any_element()
    }

    fn render_select(&self, node: &Node, id: NodeId, eid: gpui::ElementId, cx: &Context<Self>) -> AnyElement {
        let options = node.prop("options").and_then(Value::as_array).cloned().unwrap_or_default();
        let value = node.prop("value").cloned().unwrap_or(Value::Null);
        let label = options
            .iter()
            .find(|o| o.get("value") == Some(&value))
            .and_then(|o| o.get("label").and_then(Value::as_str))
            .or(node.str_prop("placeholder"))
            .unwrap_or("Select…")
            .to_string();
        let disabled = node.prop("disabled").and_then(Value::as_bool).unwrap_or(false);
        let host = self.host.clone();
        let _ = cx;
        PopoverMenu::new(eid.clone())
            .menu(move |window, cx| {
                let (options, value, host) = (options.clone(), value.clone(), host.clone());
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for option in &options {
                        let label = option.get("label").and_then(Value::as_str).unwrap_or_default().to_string();
                        let option_value = option.get("value").cloned().unwrap_or(Value::Null);
                        let host = host.clone();
                        let checked = option_value == value;
                        menu = menu.toggleable_entry(label, checked, ui::IconPosition::Start, None, move |_, cx| host.read(cx).dispatch(id, "onChange", option_value.clone()));
                    }
                    menu
                }))
            })
            .trigger(Button::new(eid, label).style(ButtonStyle::Outlined).end_icon(Icon::new(IconName::ChevronUpDown).size(IconSize::XSmall)).disabled(disabled))
            .into_any_element()
    }

    fn render_tree_item(&self, node: &Node, id: NodeId, eid: gpui::ElementId, cx: &Context<Self>) -> AnyElement {
        let dispatch = self.dispatcher(id);
        let label = node.str_prop("label").unwrap_or_default().to_string();
        let depth = node.prop("depth").and_then(Value::as_u64).unwrap_or(0) as usize;
        let expanded = node.prop("expanded").and_then(Value::as_bool);
        let selected = node.prop("selected").and_then(Value::as_bool).unwrap_or(false);
        let loading = node.prop("loading").and_then(Value::as_bool).unwrap_or(false);
        let icon = node.str_prop("icon").and_then(icon_name);
        let icon_color = node.str_prop("iconColor").map(label_color).unwrap_or(Color::Muted);
        let description = node.str_prop("description").map(str::to_string);
        let on_toggle = dispatch("onToggle", Value::Null);
        let (on_click, on_double_click) = (dispatch("onClick", Value::Null), dispatch("onDoubleClick", Value::Null));
        let _ = cx;
        ListItem::new(eid)
            .indent_level(depth)
            .indent_step_size(px(12.))
            .spacing(ui::ListItemSpacing::Dense)
            .toggle(expanded)
            // Zed shows an open node's chevron only on hover; a tree reads better with it always.
            .always_show_disclosure_icon(true)
            .on_toggle(move |_, _, cx| on_toggle(cx))
            .toggle_state(selected)
            .start_slot::<AnyElement>(match (loading, icon) {
                (true, _) => Some(Icon::new(IconName::ArrowCircle).size(IconSize::Small).color(Color::Muted).with_rotate_animation(2).into_any_element()),
                (false, Some(icon)) => Some(Icon::new(icon).size(IconSize::Small).color(icon_color).into_any_element()),
                _ => None,
            })
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(Label::new(label).truncate())
                    .when_some(description, |row, d| row.child(Label::new(d).size(LabelSize::Small).color(Color::Muted).truncate())),
            )
            .on_click(move |event: &ClickEvent, _, cx| if event.click_count() >= 2 { on_double_click(cx) } else { on_click(cx) })
            .into_any_element()
    }

    // ------------------------------------------------------------------------------------
    // Data grid

    fn render_grid(&self, node: &Node, id: NodeId, style: &Value, cx: &Context<Self>) -> AnyElement {
        let Some(state) = self.grids.get(&id) else { return div().into_any_element() };
        let columns = node.prop("columns").and_then(Value::as_array).cloned().unwrap_or_default();
        let row_count = node.prop("rows").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
        let sort = node.prop("sort").cloned().unwrap_or(Value::Null);
        let colors = cx.theme().colors();

        let mut headers: Vec<AnyElement> = vec![div().into_any_element()];
        for (index, column) in columns.iter().enumerate() {
            let name = column.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
            let ty = column.get("type").and_then(Value::as_str).map(str::to_string);
            let key = column.get("primaryKey").and_then(Value::as_bool).unwrap_or(false);
            let sorted = (sort.get("column").and_then(Value::as_str) == Some(name.as_str())).then(|| sort.get("desc").and_then(Value::as_bool).unwrap_or(false));
            let on_sort = self.dispatcher(id)("onSort", json!({ "column": name, "index": index }));
            headers.push(
                h_flex()
                    .id(("forge-grid-header", index))
                    .gap_1()
                    .w_full()
                    .overflow_hidden()
                    .cursor_pointer()
                    .when(key, |h| h.child(Icon::new(IconName::Hash).size(IconSize::XSmall).color(Color::Accent)))
                    .child(Label::new(name).size(LabelSize::Small).weight(FontWeight::MEDIUM).truncate())
                    .when_some(ty, |h, ty| h.child(Label::new(ty).size(LabelSize::XSmall).color(Color::Muted).truncate()))
                    .when_some(sorted, |h, desc| h.child(Icon::new(if desc { IconName::ArrowDown } else { IconName::ArrowUp }).size(IconSize::XSmall).color(Color::Muted)))
                    .on_click(move |_, _, cx| on_sort(cx))
                    .into_any_element(),
            );
        }

        let empty = node.str_prop("emptyText").unwrap_or("No rows").to_string();
        let table = Table::new(columns.len() + 1)
            .interactable(&state.interaction)
            .width_config(ColumnWidthConfig::Resizable(state.widths.clone()))
            .pin_cols(1)
            .striped()
            .header(headers)
            .uniform_list(SharedString::from(format!("forge-grid-{}-{id}", self.panel)), row_count, cx.processor(move |this, range: Range<usize>, window, cx| this.render_grid_rows(id, range, window, cx)))
            .empty_table_callback(move |_, _| v_flex().p_4().child(Label::new(empty.clone()).color(Color::Muted)).into_any_element());

        let focus = state.interaction.read(cx).focus_handle.clone();
        let panel = self.panel.clone();
        styled(div(), style, cx)
            .id(self.element_id(id))
            // Tests measure the grid (it must get room to show its rows).
            .debug_selector(move || format!("forge-grid-{panel}-{id}"))
            .key_context("ForgeGrid")
            .track_focus(&focus)
            .on_action(cx.listener(move |this, _: &GridCopy, _, cx| this.copy_rows(id, cx)))
            .on_action(cx.listener(move |this, _: &GridDelete, _, cx| {
                let rows = this.selected_rows(id, cx);
                if !rows.is_empty() {
                    this.host.read(cx).dispatch(id, "onDeleteRows", json!({ "rows": rows }));
                }
            }))
            .min_h_0()
            .overflow_hidden()
            .border_1()
            .border_color(colors.border_variant)
            .child(table)
            .into_any_element()
    }

    fn render_grid_rows(&mut self, id: NodeId, range: Range<usize>, _: &mut Window, cx: &mut Context<Self>) -> Vec<Vec<AnyElement>> {
        let host = self.host.clone();
        let Some(node) = host.read(cx).trees.get(&self.panel).and_then(|t| t.get(id)) else { return vec![] };
        let rows = node.prop("rows").and_then(Value::as_array);
        let columns = node.prop("columns").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
        let offset = node.prop("rowOffset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let selected: Vec<usize> = node.prop("selectedRows").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_u64().map(|v| v as usize)).collect();
        let row_states = node.prop("rowStates").and_then(Value::as_object);
        let edited: Vec<&str> = node.prop("editedCells").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        let editing = self.grids.get(&id).and_then(|g| g.editing.as_ref().map(|(r, c, e, _)| (*r, *c, e.clone())));
        let theme = cx.theme();
        let (colors, status) = (theme.colors(), theme.status());
        let selected_bg = colors.element_selected;
        let line_number = colors.editor_line_number;

        let mut out = Vec::with_capacity(range.len());
        for row in range {
            let cells = rows.and_then(|r| r.get(row)).and_then(Value::as_array);
            let state = row_states.and_then(|s| s.get(&row.to_string())).and_then(Value::as_str);
            let row_bg: Option<Hsla> = match state {
                Some("new") => Some(status.created_background.opacity(0.35)),
                Some("deleted") => Some(status.deleted_background.opacity(0.35)),
                _ if selected.contains(&row) => Some(selected_bg),
                _ => None,
            };
            let mut elements: Vec<AnyElement> = Vec::with_capacity(columns + 1);
            elements.push(
                div()
                    .id(("forge-grid-rownum", row))
                    .size_full()
                    .h(px(ROW_HEIGHT))
                    .flex()
                    .items_center()
                    .justify_end()
                    .pr_1()
                    .text_color(line_number)
                    .when_some(row_bg, |d, bg| d.bg(bg))
                    .child(Label::new((offset + row + 1).to_string()).size(LabelSize::Small).color(Color::Custom(line_number)))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| this.select_row(id, row, event, window, cx)))
                    .into_any_element(),
            );
            for column in 0..columns {
                let cell = cells.and_then(|c| c.get(column)).unwrap_or(&Value::Null);
                let mut content = div().size_full().h(px(ROW_HEIGHT)).flex().items_center().overflow_hidden().whitespace_nowrap();
                if let Some((_, _, editor)) = editing.as_ref().filter(|(r, c, _)| *r == row && *c == column) {
                    content = content.key_context("ForgeGridCell").child(
                        div()
                            .w_full()
                            .px_1()
                            .border_1()
                            .border_color(colors.border_focused)
                            .bg(colors.editor_background)
                            .on_action(cx.listener(move |this, _: &menu::Confirm, window, cx| this.finish_edit(id, true, window, cx)))
                            .on_action(cx.listener(move |this, _: &menu::Cancel, window, cx| this.finish_edit(id, false, window, cx)))
                            .child(editor.clone()),
                    );
                } else {
                    let (text, color, italic) = match cell {
                        Value::Null => ("NULL".to_string(), Color::Muted, true),
                        Value::String(s) => (first_line(s), Color::Default, false),
                        Value::Bool(b) => (b.to_string(), Color::Accent, false),
                        Value::Number(n) => (n.to_string(), Color::Default, false),
                        other => (other.to_string(), Color::Default, false),
                    };
                    let mut label = Label::new(text).size(LabelSize::Small).color(color).truncate().buffer_font(cx);
                    if italic {
                        label = label.italic();
                    }
                    let is_edited = edited.iter().any(|e| *e == format!("{row}:{column}"));
                    content = content
                        .px_1()
                        .when(cell.is_number(), |c| c.justify_end())
                        .when(is_edited && state != Some("new"), |c| c.bg(status.modified_background.opacity(0.4)))
                        .child(label);
                }
                elements.push(
                    div()
                        .id(("forge-grid-cell", row * 4096 + column))
                        .size_full()
                        .when_some(row_bg, |d, bg| d.bg(bg))
                        .child(content)
                        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                            if event.click_count() >= 2 {
                                this.start_edit(id, row, column, window, cx);
                            } else {
                                this.select_row(id, row, event, window, cx);
                            }
                        }))
                        .on_mouse_down(MouseButton::Right, cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            if !this.selected_rows(id, cx).contains(&row) {
                                this.set_selection(id, vec![row], cx);
                            }
                            let items = this.tree(cx).and_then(|t| t.get(id)).and_then(|n| n.prop("contextMenu").cloned()).unwrap_or_default();
                            if items.is_array() {
                                let rows = this.selected_rows(id, cx);
                                this.deploy_menu(id, items, json!({ "row": row, "column": column, "rows": rows }), event.position, window, cx);
                            }
                        }))
                        .into_any_element(),
                );
            }
            out.push(elements);
        }
        out
    }

    fn selected_rows(&self, id: NodeId, cx: &App) -> Vec<usize> {
        let node = self.tree(cx).and_then(|t| t.get(id));
        node.and_then(|n| n.prop("selectedRows")).and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_u64().map(|v| v as usize)).collect()
    }

    fn set_selection(&mut self, id: NodeId, rows: Vec<usize>, cx: &mut Context<Self>) {
        self.host.read(cx).dispatch(id, "onSelect", json!({ "rows": rows }));
    }

    /// Click: select the row; cmd-click: toggle it; shift-click: select the range from the
    /// last clicked row.
    fn select_row(&mut self, id: NodeId, row: usize, event: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let mut rows = self.selected_rows(id, cx);
        let modifiers = event.modifiers();
        let Some(state) = self.grids.get_mut(&id) else { return };
        state.interaction.read(cx).focus_handle.clone().focus(window, cx);
        if modifiers.shift {
            let anchor = state.anchor.unwrap_or(row);
            rows = (anchor.min(row)..=anchor.max(row)).collect();
        } else if modifiers.platform {
            if let Some(i) = rows.iter().position(|r| *r == row) {
                rows.remove(i);
            } else {
                rows.push(row);
            }
            state.anchor = Some(row);
        } else {
            rows = vec![row];
            state.anchor = Some(row);
        }
        self.set_selection(id, rows, cx);
    }

    fn start_edit(&mut self, id: NodeId, row: usize, column: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.tree(cx).and_then(|t| t.get(id)) else { return };
        if node.prop("editable").and_then(Value::as_bool) != Some(true) {
            self.host.read(cx).dispatch(id, "onRowActivate", json!({ "row": row, "column": column }));
            return;
        }
        let value = node.prop("rows").and_then(|r| r.get(row)).and_then(|r| r.get(column)).cloned().unwrap_or(Value::Null);
        let text = match value {
            Value::Null => String::new(),
            Value::String(s) => s,
            other => other.to_string(),
        };
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(text, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor
        });
        let subscription = cx.subscribe_in(&editor, window, move |this, _, event: &EditorEvent, window, cx| {
            if matches!(event, EditorEvent::Blurred) {
                this.finish_edit(id, true, window, cx);
            }
        });
        editor.focus_handle(cx).focus(window, cx);
        if let Some(state) = self.grids.get_mut(&id) {
            state.editing = Some((row, column, editor, subscription));
        }
        cx.notify();
    }

    /// Ends editing a cell; `commit` sends the new text to the extension (`onCellEdit`).
    fn finish_edit(&mut self, id: NodeId, commit: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.grids.get_mut(&id) else { return };
        let Some((row, column, editor, _)) = state.editing.take() else { return };
        let focus = state.interaction.read(cx).focus_handle.clone();
        if commit {
            let value = editor.read(cx).text(cx);
            self.host.read(cx).dispatch(id, "onCellEdit", json!({ "row": row, "column": column, "value": value }));
        }
        focus.focus(window, cx);
        cx.notify();
    }

    fn copy_rows(&self, id: NodeId, cx: &mut Context<Self>) {
        let rows = self.selected_rows(id, cx);
        let Some(node) = self.tree(cx).and_then(|t| t.get(id)) else { return };
        let data = node.prop("rows").and_then(Value::as_array);
        let text: Vec<String> = rows
            .iter()
            .filter_map(|r| data.and_then(|d| d.get(*r)).and_then(Value::as_array))
            .map(|cells| {
                cells
                    .iter()
                    .map(|c| match c {
                        Value::Null => "NULL".to_string(),
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect();
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text.join("\n")));
        }
    }

    // ------------------------------------------------------------------------------------
    // Context menus

    /// Shows a node's `contextMenu` at `position`; choosing an item sends `onContextMenu`
    /// with its id (plus `extra`, e.g. the grid cell).
    fn deploy_menu(&mut self, id: NodeId, items: Value, extra: Value, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.host.clone();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            for item in items.as_array().into_iter().flatten() {
                if item.get("separator").and_then(Value::as_bool) == Some(true) {
                    menu = menu.separator();
                    continue;
                }
                if let Some(header) = item.get("header").and_then(Value::as_str) {
                    menu = menu.header(header.to_string());
                    continue;
                }
                let label = item.get("label").and_then(Value::as_str).unwrap_or_default().to_string();
                let item_id = item.get("id").cloned().unwrap_or(Value::Null);
                let mut payload = extra.clone();
                if let Some(object) = payload.as_object_mut() {
                    object.insert("id".into(), item_id);
                }
                let host = host.clone();
                let mut entry = ui::ContextMenuEntry::new(label)
                    .disabled(item.get("disabled").and_then(Value::as_bool).unwrap_or(false))
                    .handler(move |_, cx| host.read(cx).dispatch(id, "onContextMenu", payload.clone()));
                if let Some(icon) = item.get("icon").and_then(Value::as_str).and_then(icon_name) {
                    entry = entry.icon(icon).icon_color(Color::Muted);
                }
                if item.get("danger").and_then(Value::as_bool) == Some(true) {
                    entry = entry.icon_color(Color::Error);
                }
                menu = menu.item(entry);
            }
            menu
        });
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.menu = None;
            cx.notify();
        });
        self.menu = Some(OpenMenu { menu, position, _subscription: subscription });
        cx.notify();
    }
}

impl Render for Surface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync(window, cx);
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

fn icon_name(name: &str) -> Option<IconName> {
    IconName::from_str(name).ok()
}

fn icon_size(style: &Value) -> IconSize {
    match style.get("size").and_then(Value::as_str) {
        Some("xs") => IconSize::XSmall,
        Some("sm") => IconSize::Small,
        Some("lg") => IconSize::Medium,
        _ => IconSize::Small,
    }
}

fn label_size(style: &Value) -> LabelSize {
    match style.get("size").and_then(Value::as_str) {
        Some("xs") => LabelSize::XSmall,
        Some("sm") => LabelSize::Small,
        Some("lg") => LabelSize::Large,
        _ => LabelSize::Default,
    }
}

fn label_color(token: &str) -> Color {
    match token {
        "muted" => Color::Muted,
        "accent" => Color::Accent,
        "error" => Color::Error,
        "warning" => Color::Warning,
        "success" => Color::Success,
        _ => Color::Default,
    }
}

pub(crate) fn token_color(token: &str, cx: &App) -> Option<Hsla> {
    let c = cx.theme().colors();
    let s = cx.theme().status();
    Some(match token {
        "default" => c.text,
        "muted" => c.text_muted,
        "accent" => c.text_accent,
        "error" => s.error,
        "warning" => s.warning,
        "success" => s.success,
        "surface" => c.surface_background,
        "elevated" => c.elevated_surface_background,
        "panel" => c.panel_background,
        "editor" => c.editor_background,
        "transparent" => gpui::transparent_black(),
        _ => return None,
    })
}

/// Applies the `Style` subset from packages/forge-api/src/index.ts.
/// Highlights `editor`'s text as `language` (Zed's name for it), or as plain text.
fn set_language(editor: &Entity<Editor>, language: Option<String>, cx: &mut App) {
    let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else { return };
    let Some(name) = language else {
        buffer.update(cx, |b, cx| b.set_language(None, cx));
        return;
    };
    let Some(registry) = workspace::AppState::try_global(cx).map(|state| state.languages.clone()) else { return };
    let load = registry.language_for_name(&name);
    cx.spawn(async move |cx| match load.await {
        Ok(language) => buffer.update(cx, |b, cx| b.set_language(Some(language), cx)),
        Err(e) => log::warn!("an input asked for the language {name}: {e:#}"),
    })
    .detach();
}

pub(crate) fn styled<E: gpui::Styled>(el: E, style: &Value, cx: &App) -> E {
    let num = |k: &str| style.get(k).and_then(Value::as_f64).map(|v| px(v as f32));
    let s = |k: &str| style.get(k).and_then(Value::as_str);
    let mut el = el.flex();
    el = if s("direction") == Some("row") { el.flex_row().items_center() } else { el.flex_col() };
    if let Some(v) = num("gap") {
        el = el.gap(v);
    }
    if let Some(v) = num("padding") {
        el = el.p(v);
    }
    if let Some(v) = num("paddingX") {
        el = el.px(v);
    }
    if let Some(v) = num("paddingY") {
        el = el.py(v);
    }
    el = match s("align") {
        Some("start") => el.items_start(),
        Some("center") => el.items_center(),
        Some("end") => el.items_end(),
        Some("stretch") => el.items_stretch(),
        _ => el,
    };
    el = match s("justify") {
        Some("start") => el.justify_start(),
        Some("center") => el.justify_center(),
        Some("end") => el.justify_end(),
        Some("between") => el.justify_between(),
        _ => el,
    };
    if style.get("grow").and_then(Value::as_bool) == Some(true) {
        el = el.flex_grow_1().flex_basis(px(0.)).min_h_0().min_w_0();
    }
    if style.get("shrink").and_then(Value::as_bool) == Some(false) {
        el = el.flex_shrink_0();
    }
    if style.get("wrap").and_then(Value::as_bool) == Some(true) {
        el = el.flex_wrap();
    }
    el = match style.get("width") {
        Some(Value::String(f)) if f == "full" => el.w_full(),
        Some(v) if v.is_number() => el.w(px(v.as_f64().unwrap_or_default() as f32)),
        _ => el,
    };
    el = match style.get("height") {
        Some(Value::String(f)) if f == "full" => el.h_full(),
        Some(v) if v.is_number() => el.h(px(v.as_f64().unwrap_or_default() as f32)),
        _ => el,
    };
    if let Some(v) = num("minWidth") {
        el = el.min_w(v);
    }
    if let Some(v) = num("maxWidth") {
        el = el.max_w(v);
    }
    if let Some(c) = s("background").and_then(|t| token_color(t, cx)) {
        el = el.bg(c);
    }
    if let Some(c) = s("color").and_then(|t| token_color(t, cx)) {
        el = el.text_color(c);
    }
    if style.get("border").and_then(Value::as_bool) == Some(true) {
        el = el.border_1().border_color(cx.theme().colors().border);
    }
    match s("borderSide") {
        Some("top") => el = el.border_t_1().border_color(cx.theme().colors().border),
        Some("bottom") => el = el.border_b_1().border_color(cx.theme().colors().border),
        Some("left") => el = el.border_l_1().border_color(cx.theme().colors().border),
        Some("right") => el = el.border_r_1().border_color(cx.theme().colors().border),
        _ => {}
    }
    if style.get("rounded").and_then(Value::as_bool) == Some(true) {
        el = el.rounded_md();
    }
    el
}

/// A `data:image/<type>;base64,…` URI's image.
fn decode_data_uri(uri: &str) -> Option<gpui::Image> {
    use base64::Engine as _;
    let (header, data) = uri.strip_prefix("data:")?.split_once(',')?;
    let mime = header.strip_suffix(";base64")?;
    let format = gpui::ImageFormat::from_mime_type(mime)?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()?;
    Some(gpui::Image::from_bytes(format, bytes))
}
