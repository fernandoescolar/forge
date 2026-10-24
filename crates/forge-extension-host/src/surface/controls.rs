//! Drawing Markdown, images, inputs, selects and tree items.

use super::*;

impl Surface {
    pub(super) fn render_markdown(&self, id: NodeId, style: &Value, window: &Window, cx: &Context<Self>) -> AnyElement {
        let Some((_, markdown)) = self.markdowns.get(&id) else { return div().into_any_element() };
        let font = if style.get("mono").and_then(Value::as_bool) == Some(true) { MarkdownFont::Editor } else { MarkdownFont::Agent };
        styled(div().min_w_0(), style, cx).child(MarkdownElement::new(markdown.clone(), MarkdownStyle::themed(font, window, cx))).into_any_element()
    }

    pub(super) fn render_image(&self, node: &Node, id: NodeId, style: &Value, cx: &Context<Self>) -> AnyElement {
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

    pub(super) fn render_input(&self, node: &Node, id: NodeId, style: &Value, cx: &Context<Self>) -> AnyElement {
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

    pub(super) fn render_select(&self, node: &Node, id: NodeId, eid: gpui::ElementId, cx: &Context<Self>) -> AnyElement {
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

    pub(super) fn render_tree_item(&self, node: &Node, id: NodeId, eid: gpui::ElementId, cx: &Context<Self>) -> AnyElement {
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
}
