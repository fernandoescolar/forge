//! Git in Forge: Zed's git panel, blame and file history, plus a History panel (the commit
//! graph), a three-pane merge editor, an offer to `git init` folders that aren't repositories yet and a background
//! fetch, so the title bar can show commits waiting on the server.

mod auto_fetch;
pub mod conflicts;
pub mod history;
pub mod init;
pub mod merge_editor;

pub use history::HistoryPanel;

pub fn init(cx: &mut gpui::App) {
    // GitHub, GitLab, … links (blame permalinks, commit URLs) need the registry first.
    git::GitHostingProviderRegistry::set_global(std::sync::Arc::new(git::GitHostingProviderRegistry::new()), cx);
    git_hosting_providers::init(cx);
    git_ui::init(cx);
    history::init(cx);
    init::init(cx);
    auto_fetch::init(cx);
    merge_editor::init(cx);
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext as _, TestAppContext, VisualTestContext};
    use project::Project;
    use serde_json::json;

    use crate::{HistoryPanel, init::uninitialized_root};

    /// A plain folder is offered `git init`; afterwards it is a repository and the
    /// History panel shows its graph.
    #[gpui::test]
    async fn initializes_a_folder_and_shows_its_history(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let params = workspace::AppState::test(cx);
            drop(params);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "main.rs": "fn main() {}" })).await;
        let project = Project::test(fs.clone(), ["/project".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();

        assert!(cx.update(|_, cx| uninitialized_root(&project, cx)).is_some(), "not a repository yet");
        let panel = workspace.update_in(cx, |ws, window, cx| cx.new(|cx| HistoryPanel::new(ws, window, cx)));
        assert!(panel.read_with(cx, |p, _| p.has_graph()) == false);

        workspace.update_in(cx, |ws, window, cx| crate::init::init_repository(ws, window, cx));
        cx.run_until_parked();

        assert!(cx.update(|_, cx| uninitialized_root(&project, cx)).is_none(), "now a repository");
        assert!(panel.read_with(cx, |p, _| p.has_graph()), "the History panel shows the new repository");
    }
}
