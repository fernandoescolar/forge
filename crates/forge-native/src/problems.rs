//! The Problems tab's toolbar: Zed's controls (show warnings, refresh) and Forge's *Fix
//! with Agent*, which asks an agent to fix every problem in the project. Zed adds its
//! controls in its `initialize_pane`, which Forge doesn't run.

use diagnostics::ToolbarControls;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, IntoElement, Render, Window, div};
use ui::{Button, ButtonCommon as _, ButtonSize, ButtonStyle, Clickable as _, Icon, IconName, IconSize, Tooltip};
use workspace::{ItemHandle, Pane, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace};

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let center_pane = workspace.active_pane().clone();
        add_to_pane(&center_pane, window, cx);
        cx.subscribe_in(&cx.entity(), window, |_, _, event, window, cx| {
            if let workspace::Event::PaneAdded(pane) = event {
                add_to_pane(pane, window, cx);
            }
        })
        .detach();
    })
    .detach();
}

fn add_to_pane(pane: &Entity<Pane>, window: &mut Window, cx: &mut Context<Workspace>) {
    pane.update(cx, |pane, cx| {
        pane.toolbar().update(cx, |toolbar, cx| {
            toolbar.add_item(cx.new(|_| FixWithAgent { visible: false }), window, cx);
            toolbar.add_item(cx.new(|_| ToolbarControls::new()), window, cx);
        })
    });
}

/// Whether `item` is Zed's Problems tab. Its type is private to Zed's `diagnostics` crate:
/// it is recognised by the name it reports itself with.
fn is_problems_tab(item: &dyn ItemHandle, cx: &App) -> bool {
    item.telemetry_event_text(cx) == Some("Project Diagnostics Opened")
}

/// *Fix with Agent*, on the Problems tab.
pub struct FixWithAgent {
    visible: bool,
}

impl EventEmitter<ToolbarItemEvent> for FixWithAgent {}

impl ToolbarItemView for FixWithAgent {
    fn set_active_pane_item(&mut self, item: Option<&dyn ItemHandle>, _: &mut Window, cx: &mut Context<Self>) -> ToolbarItemLocation {
        self.visible = item.is_some_and(|item| is_problems_tab(item, cx));
        if self.visible { ToolbarItemLocation::PrimaryRight } else { ToolbarItemLocation::Hidden }
    }
}

impl Render for FixWithAgent {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        if !self.visible {
            return div().into_any_element();
        }
        Button::new("fix-problems-with-agent", "Fix with Agent")
            .style(ButtonStyle::Subtle)
            .size(ButtonSize::Compact)
            .start_icon(Some(Icon::new(IconName::ZedAgent).size(IconSize::Small)))
            .tooltip(Tooltip::for_action_title("Ask an agent to fix every error and warning in the project", &forge_agents::FixProblemsInProject))
            .on_click(|_, window, cx| window.dispatch_action(Box::new(forge_agents::FixProblemsInProject), cx))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;

    /// The Problems tab shows *Fix with Agent* (and Zed's controls); a file doesn't.
    #[gpui::test]
    async fn the_problems_tab_offers_to_fix_them(cx: &mut TestAppContext) {
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            diagnostics::init(cx);
            init(cx);
        });
        params.fs.as_fake().insert_tree("/root", json!({ "a.txt": "one\n" })).await;
        let project = project::Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
        let fix_button = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |ws, cx| ws.active_pane().read(cx).toolbar().read(cx).item_of_type::<FixWithAgent>()).map(|b| b.read_with(cx, |b, _| b.visible))
        };

        let worktree = workspace.read_with(cx, |ws, cx| ws.project().read(cx).worktrees(cx).next().unwrap().read(cx).id());
        workspace.update_in(cx, |ws, window, cx| ws.open_path((worktree, util::rel_path::rel_path("a.txt")), None, true, window, cx)).await.unwrap();
        cx.run_until_parked();
        assert_eq!(fix_button(cx), Some(false), "not for a file");

        cx.dispatch_action(diagnostics::Deploy);
        cx.run_until_parked();
        assert_eq!(fix_button(cx), Some(true), "on the Problems tab");
        assert!(workspace.read_with(cx, |ws, cx| ws.active_pane().read(cx).toolbar().read(cx).item_of_type::<ToolbarControls>().is_some()), "with Zed's controls");
    }
}
