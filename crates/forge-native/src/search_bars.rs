//! The find bar (⌘F) and the project search's query bar (⌘⇧F). Both live in a pane's
//! toolbar: without them the actions have nowhere to show up. Zed adds them in its
//! `initialize_pane`, which Forge doesn't run.

use gpui::{App, AppContext as _, Context, Entity, Window};
use search::{BufferSearchBar, project_search::ProjectSearchBar};
use workspace::{Pane, Workspace};

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let center_pane = workspace.active_pane().clone();
        add_to_pane(workspace, &center_pane, window, cx);
        cx.subscribe_in(&cx.entity(), window, |workspace, _, event, window, cx| {
            if let workspace::Event::PaneAdded(pane) = event {
                add_to_pane(workspace, pane, window, cx);
            }
        })
        .detach();
    })
    .detach();
}

fn add_to_pane(workspace: &Workspace, pane: &Entity<Pane>, window: &mut Window, cx: &mut Context<Workspace>) {
    let languages = workspace.project().read(cx).languages().clone();
    pane.update(cx, |pane, cx| {
        pane.toolbar().update(cx, |toolbar, cx| {
            let buffer_search_bar = cx.new(|cx| BufferSearchBar::new(Some(languages), window, cx));
            toolbar.add_item(buffer_search_bar, window, cx);
            let project_search_bar = cx.new(|_| ProjectSearchBar::new());
            toolbar.add_item(project_search_bar, window, cx);
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;

    async fn workspace_with_files(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            search::init(cx);
            init(cx);
        });
        params.fs.as_fake().insert_tree("/root", json!({ "a.txt": "one needle\n", "b.txt": "two needles\n", "c.txt": "nothing\n" })).await;
        let project = project::Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        (workspace, VisualTestContext::from_window(window.into(), cx).into_mut())
    }

    /// ⌘F in an open file shows the find bar, focused, and finds what is typed.
    #[gpui::test]
    async fn find_in_the_open_file(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_with_files(cx).await;
        let path = workspace.read_with(cx, |ws, cx| ws.project().read(cx).worktrees(cx).next().unwrap().read(cx).id());
        workspace
            .update_in(cx, |ws, window, cx| ws.open_path((path, util::rel_path::rel_path("a.txt")), None, true, window, cx))
            .await
            .unwrap();
        cx.dispatch_action(search::buffer_search::Deploy::find());
        cx.run_until_parked();
        let bar = workspace.read_with(cx, |ws, cx| ws.active_pane().read(cx).toolbar().read(cx).item_of_type::<BufferSearchBar>()).expect("a find bar in the pane");
        assert!(bar.read_with(cx, |bar, _| !bar.is_dismissed()), "the find bar shows");
        assert!(bar.read_with(cx, |bar, _| bar.query_editor_focused()), "and takes the typing");
        cx.simulate_input("needle");
        cx.run_until_parked();
        assert_eq!(bar.read_with(cx, |bar, cx| bar.query(cx)), "needle");
    }

    /// ⌘⇧F with nothing open: typing goes to the query and Enter searches the project.
    #[gpui::test]
    async fn find_in_the_project(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_with_files(cx).await;
        cx.dispatch_action(workspace::DeploySearch::default());
        cx.run_until_parked();
        let view = workspace
            .read_with(cx, |ws, cx| ws.active_item(cx).and_then(|item| item.downcast::<search::ProjectSearchView>()))
            .expect("the project search opens");
        cx.simulate_input("needle");
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, cx| view.search_query_text(cx)), "needle", "the typing is the query");
        assert!(view.read_with(cx, |view, _| view.has_matches()), "and it finds the files");
    }
}
