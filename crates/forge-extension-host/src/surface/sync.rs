//! Keeping the entities behind the tree's nodes in step with it before drawing: editors
//! for inputs (controlled, without losing typed keys), grids' column state, rendered
//! Markdown and decoded images.

use super::*;

impl Surface {
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
                self.inputs.insert(id, InputState { editor, kind: (multiline, password), language: None, value: known, sent, placeholder: String::new(), _subscription: subscription });
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
            let new_placeholder = (state.placeholder != placeholder).then(|| placeholder.clone());
            state.placeholder = placeholder;
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
                if let Some(placeholder) = &new_placeholder {
                    e.set_placeholder_text(placeholder, window, cx);
                }
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

    pub(super) fn sync_media(&mut self, cx: &mut Context<Self>) {
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
}

/// Applies the `Style` subset from packages/forge-api/src/index.ts.
/// Highlights `editor`'s text as `language` (Zed's name for it), or as plain text.
pub(super) fn set_language(editor: &Entity<Editor>, language: Option<String>, cx: &mut App) {
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

/// A `data:image/<type>;base64,…` URI's image.
pub(super) fn decode_data_uri(uri: &str) -> Option<gpui::Image> {
    use base64::Engine as _;
    let (header, data) = uri.strip_prefix("data:")?.split_once(',')?;
    let mime = header.strip_suffix(";base64")?;
    let format = gpui::ImageFormat::from_mime_type(mime)?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()?;
    Some(gpui::Image::from_bytes(format, bytes))
}
