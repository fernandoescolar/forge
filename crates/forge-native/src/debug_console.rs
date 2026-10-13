//! Debug Console: the active debug session's console (the program's output, and
//! expressions to evaluate) as a panel of its own, instead of a tab of the Debug panel. It
//! can live in any dock, or be shown together with the Debug panel (`docks::PanelGroups`).

use std::{collections::HashMap, sync::Arc};

use debugger_ui::debugger_panel::DebugPanel;
use project::debugger::session::Session;
use gpui::{
    Action, App, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, ParentElement as _, Pixels, Render, Styled as _,
    Subscription, WeakEntity, Window, actions, div, px,
};
use theme::ActiveTheme as _;
use ui::{Color, IconName, InteractiveElement as _, Label, LabelCommon as _, LabelSize, h_flex, v_flex};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(forge_debug_console, [ToggleFocus]);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<DebugConsolePanel>(window, cx);
        });
    })
    .detach();
}

pub struct DebugConsolePanel {
    debug_panel: Entity<DebugPanel>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    /// The session shown last, to open the panel when another one starts.
    session: Option<EntityId>,
    /// The console takes new output when it renders: a session's output re-renders the
    /// panel (in the Debug panel, the session's panes did).
    session_observers: HashMap<EntityId, Subscription>,
    _subscription: Subscription,
}

impl DebugConsolePanel {
    /// Takes the consoles out of `debug_panel`'s sessions: this panel shows them.
    pub fn new(workspace: &Workspace, debug_panel: Entity<DebugPanel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        debug_panel.update(cx, |panel, cx| panel.set_console_elsewhere(true, window, cx));
        let subscription = cx.observe_in(&debug_panel, window, |this, debug_panel, window, cx| {
            let session = debug_panel.read(cx).active_session().map(|s| s.entity_id());
            if session.is_some() && session != this.session {
                this.reveal(window, cx);
            }
            this.session = session;
            this.observe_sessions(cx);
            cx.notify();
        });
        Self {
            debug_panel,
            workspace: workspace.weak_handle(),
            focus_handle: cx.focus_handle(),
            position: forge_ui::dock_position::load(<Self as Panel>::panel_key(), cx).unwrap_or(DockPosition::Bottom),
            session: None,
            session_observers: HashMap::default(),
            _subscription: subscription,
        }
    }

    fn observe_sessions(&mut self, cx: &mut Context<Self>) {
        let sessions: Vec<Entity<Session>> = self.debug_panel.read(cx).project().read(cx).dap_store().read(cx).sessions().cloned().collect();
        self.session_observers.retain(|id, _| sessions.iter().any(|s| s.entity_id() == *id));
        for session in sessions {
            self.session_observers.entry(session.entity_id()).or_insert_with(|| cx.observe(&session, |_, _, cx| cx.notify()));
        }
    }

    /// A session started, and the Debug panel opened: show the console too, without taking
    /// the focus. In the Debug panel's dock it only shows when grouped with it (it would
    /// replace it otherwise), which the dock does by itself.
    fn reveal(&self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let debug_panel = self.debug_panel.clone();
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else { return };
            workspace.update(cx, |workspace, cx| {
                let console = workspace.panel::<DebugConsolePanel>(cx);
                let debug_dock = debug_panel.read(cx).position(window, cx);
                let shared_dock = console.is_some_and(|console| console.read(cx).position(window, cx) == debug_dock);
                if !shared_dock {
                    workspace.open_panel::<DebugConsolePanel>(window, cx);
                }
            });
        });
    }
}

impl Render for DebugConsolePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let console = self.debug_panel.read(cx).active_console(cx);
        let header = h_flex()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(colors.border)
            .child(forge_ui::panel_grip("debug-console-grip", Arc::new(cx.entity()), "Debug Console", Some(IconName::Terminal)))
            .child(Label::new("Debug Console").weight(FontWeight::BOLD).single_line());
        let body = match console {
            Some(console) => div().flex_1().min_h_0().size_full().child(console),
            None => div()
                .flex_1()
                .p_3()
                .child(Label::new("The output of the program being debugged shows here.").size(LabelSize::Small).color(Color::Muted)),
        };
        v_flex()
            .key_context("ForgeDebugConsole")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.panel_background)
            .child(header)
            .child(body)
    }
}

impl Focusable for DebugConsolePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DebugConsolePanel {}

impl Panel for DebugConsolePanel {
    fn persistent_name() -> &'static str {
        "ForgeDebugConsolePanel"
    }
    fn panel_key() -> &'static str {
        "ForgeDebugConsolePanel"
    }
    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }
    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }
    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        forge_ui::dock_position::save(Self::panel_key(), position, cx);
        gpui::BorrowAppContext::update_global::<settings::SettingsStore, _>(cx, |_, _| {});
        cx.notify();
    }
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(260.)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Terminal)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Debug Console")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }
    fn activation_priority(&self) -> u32 {
        // Forge panels use 10–19; Zed's use 1–7 and extension slots 8 and 20+.
        16
    }
}
