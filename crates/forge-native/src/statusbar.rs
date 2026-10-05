//! Forge's status bar: diagnostics, what the language servers are doing, the debug
//! session and the cursor position, next to the dock buttons (see `docks.rs`).

use gpui::{AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, Subscription, WeakEntity, Window};
use project::Project;
use ui::{ButtonCommon as _, ButtonLike, ButtonStyle, Clickable as _, Color, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip, h_flex};
use workspace::{ItemHandle, StatusItemView, Workspace};

pub fn install(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let diagnostics = cx.new(|cx| diagnostics::items::DiagnosticIndicator::new(workspace, cx));
    let servers = cx.new(|cx| forge_output::LanguageServerStatus::new(workspace, cx));
    let debug = cx.new(|cx| DebugStatus::new(workspace, cx));
    let cursor = cx.new(|_| go_to_line::cursor_position::CursorPosition::new(workspace));
    let agents = cx.new(|cx| forge_agents::presence::AgentStatus::new(workspace, cx));
    // "Resolve merge conflicts with agent", while the project has some.
    let conflicts = cx.new(|cx| git_ui::MergeConflictIndicator::new(workspace, cx));
    let pull_request = cx.new(|cx| forge_github::PullRequestStatus::new(workspace, cx));
    workspace.status_bar().update(cx, |bar, cx| {
        bar.add_left_item(diagnostics, window, cx);
        bar.add_left_item(servers, window, cx);
        bar.add_left_item(debug, window, cx);
        bar.add_left_item(agents, window, cx);
        bar.add_left_item(conflicts, window, cx);
        bar.add_left_item(pull_request, window, cx);
        bar.add_right_item(cursor, window, cx);
    });
}

/// "● Debugging <session>" while a debug session runs; opens the debugger on click.
pub struct DebugStatus {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    _subscription: Subscription,
}

impl DebugStatus {
    fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let subscription = cx.observe(&project.read(cx).dap_store(), |_, _, cx| cx.notify());
        Self { project, workspace: workspace.weak_handle(), _subscription: subscription }
    }
}

impl Render for DebugStatus {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.project.read(cx).dap_store();
        let label = store.read(cx).sessions().find(|s| !s.read(cx).is_terminated()).map(|s| s.read(cx).label());
        let Some(label) = label else {
            return h_flex().into_any_element();
        };
        let workspace = self.workspace.clone();
        ButtonLike::new("forge-debug-status")
            .style(ButtonStyle::Subtle)
            .child(
                h_flex()
                    .gap_1()
                    .child(Icon::new(IconName::Debug).size(IconSize::XSmall).color(Color::Accent))
                    .child(Label::new(format!("Debugging {}", label.unwrap_or_default())).size(LabelSize::XSmall).color(Color::Accent)),
            )
            .tooltip(Tooltip::text("Open the debugger"))
            .on_click(move |_, window, cx| {
                if let Some(workspace) = workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        workspace.focus_panel::<debugger_ui::debugger_panel::DebugPanel>(window, cx);
                    });
                }
            })
            .into_any_element()
    }
}

impl StatusItemView for DebugStatus {
    fn set_active_pane_item(&mut self, _: Option<&dyn ItemHandle>, _: &mut Window, _: &mut Context<Self>) {}
    /// Always shown: no "Hide Button" entry.
    fn hide_setting(&self, _: &gpui::App) -> Option<workspace::HideStatusItem> {
        None
    }
}
