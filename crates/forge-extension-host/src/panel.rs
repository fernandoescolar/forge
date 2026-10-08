//! Extension UI in the dock.
//!
//! * [`SlotPanel<N>`]: eight dock panels ("slots"), each showing one or more extension
//!   panels as tabs. Slots are independent: drag one (by its header grip or its status-bar
//!   icon) to another side of the window; drag a tab onto another slot to group them,
//!   within a slot to reorder, or to a window edge to give it a slot of its own.
//! * [`ExtensionsPanel`] (in `overview.rs`): the extensions, what they offer, and installing.
//!
//! Extension trees are rendered with Zed's `ui` components and the active theme's colours.

use crate::{
    host::{ExtensionHost, HostEvent, PanelInfo},
    layout::SLOTS,
    surface::Surface,
};
use forge_ui::{DraggedExtensionTab, DraggedPanel};
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, actions, div, px,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{collections::HashMap, rc::Rc, str::FromStr as _, sync::Arc};
use theme::ActiveTheme as _;
use ui::{
    Color, Icon, IconName, Label, LabelCommon as _, LabelSize,
    Tooltip, h_flex, v_flex,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent, PanelHandle},
};

actions!(forge_extensions, [ToggleFocus]);

/// Toggles extension slot `slot` (a dock panel holding extension panels).
#[derive(PartialEq, Clone, Deserialize, Default, JsonSchema, Action)]
#[action(namespace = forge_extensions)]
#[serde(deny_unknown_fields)]
pub struct ToggleSlot {
    pub slot: usize,
}

/// Shows extension panel `id`: its slot opens, with it as the visible tab.
#[derive(PartialEq, Clone, Deserialize, Default, JsonSchema, Action)]
#[action(namespace = forge_extensions)]
#[serde(deny_unknown_fields)]
pub struct ShowPanel {
    pub id: String,
}

/// Dock identity of each slot (Zed keys panels by a static name per Rust type).
pub const SLOT_KEYS: [&str; SLOTS] = [
    "ForgeExtensionSlot0",
    "ForgeExtensionSlot1",
    "ForgeExtensionSlot2",
    "ForgeExtensionSlot3",
    "ForgeExtensionSlot4",
    "ForgeExtensionSlot5",
    "ForgeExtensionSlot6",
    "ForgeExtensionSlot7",
];

macro_rules! for_each_slot {
    ($mac:ident) => {
        $mac!(0);
        $mac!(1);
        $mac!(2);
        $mac!(3);
        $mac!(4);
        $mac!(5);
        $mac!(6);
        $mac!(7);
    };
}

pub fn init(cx: &mut App) {
    crate::surface::init(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ExtensionsPanel>(window, cx);
        });
        workspace.register_action(|workspace, action: &ShowPanel, window, cx| {
            let Some(host) = ExtensionHost::global(cx) else { return };
            let Some(slot) = host.read(cx).slot_of(&action.id) else { return };
            host.update(cx, |h, cx| h.reveal(&action.id, cx));
            macro_rules! show {
                ($n:literal) => {
                    if slot == $n {
                        workspace.focus_panel::<SlotPanel<$n>>(window, cx);
                    }
                };
            }
            for_each_slot!(show);
        });
        workspace.register_action(|workspace, action: &ToggleSlot, window, cx| {
            macro_rules! toggle {
                ($n:literal) => {
                    if action.slot == $n {
                        workspace.toggle_panel_focus::<SlotPanel<$n>>(window, cx);
                    }
                };
            }
            for_each_slot!(toggle);
        });
    })
    .detach();
}

/// A panel to register with Forge's dock buttons: its handle and a live title.
pub struct DockEntry {
    pub handle: Arc<dyn PanelHandle>,
    pub title: Rc<dyn Fn(&App) -> SharedString>,
}

/// One host serves every window: what extensions open (tabs, files, dialogs) goes to the
/// window the user is in, so the host follows the window that was activated last.
pub(crate) fn follow_active_window(host: Entity<ExtensionHost>, window: &mut Window, cx: &mut Context<Workspace>) {
    cx.observe_window_activation(window, move |workspace, window, cx| {
        if window.is_window_active() {
            let (weak, handle) = (workspace.weak_handle(), window.window_handle());
            host.update(cx, |h, cx| h.set_workspace(weak, handle, cx));
        }
    })
    .detach();
}

/// Adds the overview panel and every extension slot to `workspace`.
pub fn add_panels(host: Entity<ExtensionHost>, workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) -> Vec<DockEntry> {
    follow_active_window(host.clone(), window, cx);
    let overview = cx.new(|cx| ExtensionsPanel::new(host.clone(), workspace, window, cx));
    workspace.add_panel(overview.clone(), window, cx);
    let mut entries = vec![DockEntry { handle: Arc::new(overview), title: Rc::new(|_| "Extensions".into()) }];
    macro_rules! add_slot {
        ($n:literal) => {{
            let panel = cx.new(|cx| SlotPanel::<$n>::new(host.clone(), window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            let host = host.clone();
            entries.push(DockEntry {
                handle: Arc::new(panel),
                title: Rc::new(move |cx| {
                    let titles: Vec<String> = host.read(cx).slot_panels($n).into_iter().map(|p| p.title).collect();
                    titles.join(" · ").into()
                }),
            });
        }};
    }
    for_each_slot!(add_slot);
    entries
}

// ---------------------------------------------------------------------------------------
// Slot contents

pub struct ExtensionSlotView {
    slot: usize,
    host: Entity<ExtensionHost>,
    focus_handle: FocusHandle,
    /// The slot's own dock panel, for dragging the whole slot.
    panel_handle: Option<Arc<dyn PanelHandle>>,
    active: Option<String>,
    /// The rendered surface of each extension panel shown here.
    surfaces: HashMap<String, Entity<Surface>>,
    /// System web views of webview panels, created on first show.
    webviews: HashMap<String, Rc<wry::WebView>>,
    /// Messages from pages, forwarded to their extensions.
    page_tx: futures::channel::mpsc::UnboundedSender<(String, String)>,
    _subscription: Subscription,
}

impl ExtensionSlotView {
    fn new(slot: usize, host: Entity<ExtensionHost>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&host, |this, host, event: &HostEvent, cx| {
            if let HostEvent::Reveal { panel } = event {
                if host.read(cx).slot_panels(this.slot).iter().any(|p| &p.id == panel) {
                    this.active = Some(panel.clone());
                    cx.notify();
                }
                return;
            }
            if let HostEvent::WebviewMessage { panel, json } = event {
                if let Some(view) = this.webviews.get(panel) {
                    log::debug!("extension → webview {panel}: {json}");
                    crate::webview::post(view, json);
                }
                return;
            }
            let tabs = host.read(cx).slot_panels(this.slot);
            this.webviews.retain(|id, _| tabs.iter().any(|p| &p.id == id));
            this.surfaces.retain(|id, _| tabs.iter().any(|p| &p.id == id));
            if this.active.as_ref().is_none_or(|a| !tabs.iter().any(|p| &p.id == a)) {
                this.active = tabs.first().map(|p| p.id.clone());
            }
            cx.notify();
        });
        let (page_tx, mut page_rx) = futures::channel::mpsc::unbounded::<(String, String)>();
        let host_for_pages = host.downgrade();
        cx.spawn(async move |_, cx| {
            use futures::StreamExt as _;
            while let Some((panel, json)) = page_rx.next().await {
                if host_for_pages.update(cx, |h, _| h.page_message(panel, json)).is_err() {
                    break;
                }
            }
        })
        .detach();
        // Development aid: FORGE_EXTENSIONS_PANEL=<panel id> starts on that tab.
        let tabs = host.read(cx).slot_panels(slot);
        let wanted = std::env::var("FORGE_EXTENSIONS_PANEL").ok().filter(|w| tabs.iter().any(|p| &p.id == w));
        let active = wanted.or_else(|| tabs.first().map(|p| p.id.clone()));
        Self {
            slot,
            host,
            focus_handle: cx.focus_handle(),
            panel_handle: None,
            active,
            surfaces: HashMap::new(),
            webviews: HashMap::new(),
            page_tx,
            _subscription: subscription,
        }
    }

    fn tabs(&self, cx: &App) -> Vec<PanelInfo> {
        self.host.read(cx).slot_panels(self.slot)
    }

    pub fn has_tabs(&self, cx: &App) -> bool {
        !self.tabs(cx).is_empty()
    }

    fn icon(&self, cx: &App) -> IconName {
        self.tabs(cx).first().and_then(|p| IconName::from_str(&p.icon).ok()).unwrap_or(IconName::Blocks)
    }

    /// The surface rendering extension panel `panel` in this slot (created on first show).
    fn surface(&mut self, panel: &PanelInfo, cx: &mut Context<Self>) -> Entity<Surface> {
        let host = self.host.clone();
        self.surfaces.entry(panel.id.clone()).or_insert_with(|| cx.new(|cx| Surface::new(host, panel.id.clone(), panel.fill, cx))).clone()
    }

    fn render_tab_bar(&self, tabs: &[PanelInfo], cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let slot = self.slot;
        let mut bar = h_flex().w_full().gap_1().px_1().py_1().border_b_1().border_color(colors.border);

        // Grip: drags the whole slot to another dock.
        if let Some(handle) = self.panel_handle.clone() {
            let title: SharedString = tabs.iter().map(|t| t.title.clone()).collect::<Vec<_>>().join(" · ").into();
            let icon = self.icon(cx);
            bar = bar.child(
                div()
                    .id("slot-grip")
                    .px_1()
                    .cursor_grab()
                    .tooltip(Tooltip::text("Drag to move this panel to another side"))
                    .child(Icon::new(IconName::Ellipsis).size(ui::IconSize::Small).color(Color::Muted))
                    .on_drag(DraggedPanel { panel: handle, title: title.clone(), icon: Some(icon) }, move |d, _, _, cx| {
                        forge_ui::drag_preview(d.title.clone(), d.icon, cx)
                    }),
            );
        }

        for (i, tab) in tabs.iter().enumerate() {
            let pid = tab.id.clone();
            let icon = IconName::from_str(&tab.icon).unwrap_or(IconName::Sparkle);
            let active = self.active.as_deref() == Some(&tab.id);
            let host = self.host.clone();
            let accent = colors.border_focused;
            bar = bar.child(
                h_flex()
                    .id(SharedString::from(format!("ext-tab-{}", tab.id)))
                    .debug_selector({
                        let id = tab.id.clone();
                        move || format!("ext-tab-{id}")
                    })
                    .gap_1()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .cursor_pointer()
                    .when(active, |el| el.bg(colors.element_selected))
                    .hover(|s| s.bg(colors.ghost_element_hover))
                    .child(Icon::new(icon).size(ui::IconSize::Small).color(if active { Color::Default } else { Color::Muted }))
                    .child(Label::new(tab.title.clone()).size(LabelSize::Small).color(if active { Color::Default } else { Color::Muted }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active = Some(pid.clone());
                        cx.notify();
                    }))
                    .on_drag(DraggedExtensionTab { tab: tab.id.clone(), title: tab.title.clone().into(), from_slot: slot }, move |d, _, _, cx| {
                        forge_ui::drag_preview(d.title.clone(), Some(icon), cx)
                    })
                    .drag_over::<DraggedExtensionTab>(move |style, _, _, _| style.border_l_2().border_color(accent))
                    .on_drop(move |d: &DraggedExtensionTab, _, cx| {
                        forge_ui::end_drag(cx);
                        host.update(cx, |h, cx| h.move_tab(&d.tab, slot, Some(i), cx));
                    }),
            );
        }
        // The rest of the bar: drop here to append.
        let host = self.host.clone();
        let accent = colors.drop_target_background;
        bar.child(
            div()
                .id("ext-tab-end")
                .flex_1()
                .h(px(22.))
                .drag_over::<DraggedExtensionTab>(move |style, _, _, _| style.bg(accent))
                .on_drop(move |d: &DraggedExtensionTab, _, cx| {
                    forge_ui::end_drag(cx);
                    host.update(cx, |h, cx| h.move_tab(&d.tab, slot, None, cx));
                }),
        )
        .into_any_element()
    }

    /// Returns the web view to show for the active tab (creating it on first use) and hides
    /// every other one.
    fn active_webview(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Rc<wry::WebView>> {
        let source = self.active.as_ref().and_then(|a| self.host.read(cx).panels.iter().find(|p| &p.id == a)?.webview.clone());
        let active = self.active.clone().filter(|_| source.is_some());
        for (id, view) in &self.webviews {
            if Some(id) != active.as_ref() {
                let _ = view.set_visible(false);
            }
        }
        let (id, source) = (active?, source?);
        if !self.webviews.contains_key(&id) {
            match crate::webview::create(&id, &source, self.page_tx.clone(), window, cx) {
                Ok(view) => {
                    log::info!("created webview for {id} ({})", source.html);
                    self.webviews.insert(id.clone(), Rc::new(view));
                }
                Err(e) => {
                    log::error!("webview {id}: {e}");
                    return None;
                }
            }
        }
        self.webviews.get(&id).cloned()
    }

    fn hide_webviews(&self) {
        for view in self.webviews.values() {
            let _ = view.set_visible(false);
        }
    }
}

use gpui::prelude::FluentBuilder as _;

impl Render for ExtensionSlotView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = self.tabs(cx);
        if self.active.as_ref().is_none_or(|a| !tabs.iter().any(|p| &p.id == a)) {
            self.active = tabs.first().map(|p| p.id.clone());
        }
        let webview = self.active_webview(window, cx);
        let colors = cx.theme().colors().clone();
        let tab_bar = self.render_tab_bar(&tabs, cx);

        // Dropping a tab anywhere on the slot appends it here.
        let host = self.host.clone();
        let slot = self.slot;
        let drop_bg = colors.drop_target_background;
        let base = v_flex()
            .key_context("ForgeExtensionSlot")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.panel_background)
            .drag_over::<DraggedExtensionTab>(move |style, _, _, _| style.bg(drop_bg))
            .on_drop(move |d: &DraggedExtensionTab, _, cx| {
                forge_ui::end_drag(cx);
                host.update(cx, |h, cx| h.move_tab(&d.tab, slot, None, cx));
            })
            .child(tab_bar);

        if let Some(view) = webview {
            // The system web view is drawn by the OS over whatever bounds GPUI gives this
            // canvas; it is repositioned on every frame.
            let content = gpui::canvas(|_, _, _| {}, move |bounds, _, _, _| crate::webview::set_bounds(&view, bounds)).size_full();
            return base.child(div().flex_1().child(content)).into_any_element();
        }
        let active = self.active.as_ref().and_then(|a| tabs.iter().find(|p| &p.id == a)).cloned();
        let content = match active {
            Some(panel) => self.surface(&panel, cx).into_any_element(),
            None => v_flex().p_2().child(Label::new("Drop an extension tab here").color(Color::Muted)).into_any_element(),
        };
        base.child(div().flex_1().min_h_0().child(content)).into_any_element()
    }
}

impl Focusable for ExtensionSlotView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ---------------------------------------------------------------------------------------
// Slot dock panels

/// Dock panel for extension slot `N`. A thin wrapper: Zed identifies panels by Rust type,
/// so each slot needs its own type to be docked and remembered independently.
pub struct SlotPanel<const N: usize> {
    view: Entity<ExtensionSlotView>,
    position: DockPosition,
    _subscription: Subscription,
}

impl<const N: usize> SlotPanel<N> {
    pub fn new(host: Entity<ExtensionHost>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let view = cx.new(|cx| ExtensionSlotView::new(N, host.clone(), cx));
        let handle: Arc<dyn PanelHandle> = Arc::new(cx.entity());
        view.update(cx, |v, _| v.panel_handle = Some(handle));
        // Re-render (and re-evaluate `enabled`) when tabs move in or out.
        let subscription = cx.observe(&host, |_, _, cx| cx.notify());
        Self { view, position: forge_ui::dock_position::load(SLOT_KEYS[N], cx).unwrap_or(DockPosition::Right), _subscription: subscription }
    }
}

impl<const N: usize> Render for SlotPanel<N> {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.view.clone()
    }
}

impl<const N: usize> Focusable for SlotPanel<N> {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.read(cx).focus_handle.clone()
    }
}

impl<const N: usize> EventEmitter<PanelEvent> for SlotPanel<N> {}

impl<const N: usize> Panel for SlotPanel<N> {
    fn persistent_name() -> &'static str {
        SLOT_KEYS[N]
    }
    fn panel_key() -> &'static str {
        SLOT_KEYS[N]
    }
    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }
    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }
    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        forge_ui::dock_position::save(SLOT_KEYS[N], position, cx);
        // Docks re-check positions on settings changes; poke the store so it moves now.
        gpui::BorrowAppContext::update_global::<settings::SettingsStore, _>(cx, |_, _| {});
        cx.notify();
    }
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(340.)
    }
    fn icon(&self, _: &Window, cx: &App) -> Option<IconName> {
        Some(self.view.read(cx).icon(cx))
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Extension panel")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleSlot { slot: N })
    }
    fn activation_priority(&self) -> u32 {
        20 + N as u32
    }
    fn enabled(&self, cx: &App) -> bool {
        self.view.read(cx).has_tabs(cx)
    }
    fn starts_open(&self, _: &Window, cx: &App) -> bool {
        std::env::var("FORGE_EXTENSIONS_PANEL").is_ok_and(|p| self.view.read(cx).tabs(cx).iter().any(|t| t.id == p))
    }
    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        // Native web views don't disappear with the dock; hide them explicitly.
        if !active {
            self.view.read(cx).hide_webviews();
        }
        cx.notify();
    }
}

pub use crate::overview::ExtensionsPanel;

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, MouseButton, TestAppContext, VisualTestContext};
    use std::time::{Duration, Instant};

    /// An extension's declared settings get a Settings page; it reads their values and
    /// hears about changes.
    #[gpui::test]
    async fn extension_settings(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
        });
        let tmp = tempfile::tempdir().unwrap();
        let ext = tmp.path().join("ext-s");
        std::fs::create_dir_all(ext.join("dist")).unwrap();
        let manifest = r#"{"name":"ext-s","displayName":"Settings Test","forge":{"settings":{"properties":{
            "forge-test.size":{"type":"integer","default":3,"description":"A size."}}}}}"#;
        std::fs::write(ext.join("package.json"), manifest).unwrap();
        let code = "var __forgeExtension = { activate() { const f = __forge.modules['@forge-ide/api'].forge; \
            f.settings.get('forge-test.size').then(v => f.commands.register('got-' + v, 'got', () => {})); \
            f.settings.onDidChange((k, v) => f.commands.register('changed-' + k + '-' + v, 'changed', () => {})); } };";
        std::fs::write(ext.join("dist/extension.js"), code).unwrap();
        let host = cx.update(|cx| ExtensionHost::init(vec![tmp.path().to_path_buf()], cx)).unwrap();

        let registry = cx.update(|cx| forge_ui::settings_registry::SettingsRegistry::global(cx));
        let page = registry.read_with(cx, |r, _| r.page("ext:ext-s").cloned()).expect("a page for the extension");
        assert_eq!(page.title, "Settings Test");
        assert_eq!(page.defaults["forge-test.size"], 3);
        assert_eq!(forge_ui::settings_registry::rows(&page)[0].title, "Forge test size");

        let wait_for = |id: &'static str, cx: &mut TestAppContext| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if host.read_with(cx, |h, _| h.commands.iter().any(|c| c.id == id)) {
                    break;
                }
                assert!(Instant::now() < deadline, "no command {id}");
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait_for("got-3", cx);
        registry.update(cx, |_, cx| {
            cx.emit(forge_ui::settings_registry::SettingChanged {
                file: forge_ui::settings_registry::SettingsFile::Config(crate::host::SETTINGS_FILE.into()),
                path: vec!["forge-test.size".into()],
                value: Some(serde_json::json!(5)),
            })
        });
        wait_for("changed-forge-test.size-5", cx);
    }

    /// Two tiny extensions, each registering one (empty) panel.
    fn write_extensions(dir: &std::path::Path) {
        for (id, title) in [("a", "Alpha"), ("b", "Beta")] {
            let ext = dir.join(format!("ext-{id}"));
            std::fs::create_dir_all(ext.join("dist")).unwrap();
            std::fs::write(ext.join("package.json"), format!(r#"{{"name":"ext-{id}","forge":{{}}}}"#)).unwrap();
            std::fs::write(
                ext.join("dist/extension.js"),
                format!("var __forgeExtension = {{ activate() {{ __forge.modules['@forge-ide/api'].forge.panels.register({{ id: '{id}', title: '{title}', render: () => null }}); }} }};"),
            )
            .unwrap();
        }
    }

    #[gpui::test]
    async fn dragging_a_tab_onto_another_slot_groups_them(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            init(cx);
        });
        let tmp = tempfile::tempdir().unwrap();
        write_extensions(tmp.path());
        let host = cx.update(|cx| ExtensionHost::init(vec![tmp.path().to_path_buf()], cx)).unwrap();
        let project = project::Project::test(params.fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let deadline = Instant::now() + Duration::from_secs(10);
        while host.read_with(cx, |h, _| h.panels.len()) < 2 {
            assert!(Instant::now() < deadline, "extensions never registered their panels");
            cx.run_until_parked();
            cx.background_executor.timer(Duration::from_millis(20)).await;
        }
        let (slot_a, slot_b) = host.read_with(cx, |h, _| (h.slot_of("a").unwrap(), h.slot_of("b").unwrap()));
        assert_ne!(slot_a, slot_b, "each extension panel starts in its own slot");

        // Show slot A on the right and slot B on the left so both tab bars are on screen.
        let entries = workspace.update_in(cx, |ws, window, cx| add_panels(host.clone(), ws, window, cx));
        let handle = |slot: usize| entries.iter().find(|e| e.handle.persistent_name() == SLOT_KEYS[slot]).unwrap().handle.clone();
        let weak = workspace.downgrade();
        cx.update(|window, cx| forge_ui::move_panel(handle(slot_b), DockPosition::Left, weak.clone(), window, cx));
        cx.run_until_parked();
        cx.update(|window, cx| forge_ui::move_panel(handle(slot_a), DockPosition::Right, weak.clone(), window, cx));
        cx.run_until_parked();
        // Measure against a fresh frame.
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let tab_b = cx.debug_bounds("ext-tab-b").expect("tab B is visible");
        let tab_a = cx.debug_bounds("ext-tab-a").expect("tab A is visible");
        cx.simulate_mouse_down(tab_b.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(tab_b.center() + gpui::point(px(20.), px(5.)), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(tab_a.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(tab_a.center(), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        let (tabs, b_slot_enabled) = host.read_with(cx, |h, cx| {
            let tabs: Vec<String> = h.slot_panels(slot_a).into_iter().map(|p| p.id).collect();
            (tabs, !h.slot_panels(slot_b).is_empty() || entries.iter().any(|e| e.handle.persistent_name() == SLOT_KEYS[slot_b] && e.handle.enabled(cx)))
        });
        assert_eq!(tabs, ["b", "a"], "B was dropped before A in A's slot");
        assert!(!b_slot_enabled, "B's old slot is empty and hidden");
    }
}
