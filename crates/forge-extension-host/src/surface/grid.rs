//! The data grid: virtualized rows, resizable columns, selecting rows (with Shift and
//! Cmd/Ctrl), editing cells, copying rows and the rows' context menu.

use super::*;

impl Surface {
    pub(super) fn render_grid(&self, node: &Node, id: NodeId, style: &Value, cx: &Context<Self>) -> AnyElement {
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

    pub(super) fn render_grid_rows(&mut self, id: NodeId, range: Range<usize>, _: &mut Window, cx: &mut Context<Self>) -> Vec<Vec<AnyElement>> {
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

    pub(super) fn selected_rows(&self, id: NodeId, cx: &App) -> Vec<usize> {
        let node = self.tree(cx).and_then(|t| t.get(id));
        node.and_then(|n| n.prop("selectedRows")).and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_u64().map(|v| v as usize)).collect()
    }

    pub(super) fn set_selection(&mut self, id: NodeId, rows: Vec<usize>, cx: &mut Context<Self>) {
        self.host.read(cx).dispatch(id, "onSelect", json!({ "rows": rows }));
    }

    /// Click: select the row; cmd-click: toggle it; shift-click: select the range from the
    /// last clicked row.
    pub(super) fn select_row(&mut self, id: NodeId, row: usize, event: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn start_edit(&mut self, id: NodeId, row: usize, column: usize, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn finish_edit(&mut self, id: NodeId, commit: bool, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn copy_rows(&self, id: NodeId, cx: &mut Context<Self>) {
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
    pub(super) fn deploy_menu(&mut self, id: NodeId, items: Value, extra: Value, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
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
