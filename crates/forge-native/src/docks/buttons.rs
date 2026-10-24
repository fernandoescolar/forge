//! The dock buttons: one strip per dock, their menus, dragging them, and the drop zones
//! shown while a panel or an extension tab is dragged.

use super::*;

/// Where a dragged panel button would land: on another button (shown together with it),
/// or before or after it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Spot {
    Before,
    On,
    After,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct DropHint {
    target: &'static str,
    spot: Spot,
}

/// Drops `dragged` on the button strip of the dock at `position`: shows it together with
/// the button it is on, or puts its button where it was dropped (last, `last` being the
/// strip's last button, when not over a button), moving it to that dock.
pub(super) fn drop_on_strip(
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
    pub(super) fn new(
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

    pub(super) fn button(&self, ix: usize, handle: Arc<dyn PanelHandle>, title: SharedString, window: &Window, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
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

    pub(super) fn drop_zones(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
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
