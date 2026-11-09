//! *Restart Language Servers*, in the command palette and the editor's context menu: for the
//! whole workspace, where Zed's `editor::RestartLanguageServer` covers one editor's buffers.

use std::sync::Arc;

use editor::EditorContextMenuExtension;
use gpui::{App, Entity, actions};
use language::Buffer;
use project::Project;
use workspace::Workspace;

actions!(languages, [
    /// Restarts the language servers of the open files, and starts the ones that have stopped.
    RestartLanguageServers
]);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &RestartLanguageServers, _, cx| restart(workspace.project(), cx));
    })
    .detach();
    cx.set_global(EditorContextMenuExtension(Arc::new(|menu, _, _| menu.separator().action("Restart Language Servers", Box::new(RestartLanguageServers)))));
}

/// With no selectors, this stops whatever serves `buffers` and starts it again. It also
/// forgets which servers were stopped, so those start too, on the host in a remote project.
pub fn restart(project: &Entity<Project>, cx: &mut App) {
    let buffers = workspace_buffers(project, cx);
    project.update(cx, |project, cx| project.restart_language_servers_for_buffers(buffers, Default::default(), cx));
}

/// The open files of the workspace's folders. Decompiled sources sit in hidden worktrees of
/// their own, and restarting for each of those would start another server loading the whole
/// solution.
pub fn workspace_buffers(project: &Entity<Project>, cx: &App) -> Vec<Entity<Buffer>> {
    let project = project.read(cx);
    project
        .buffer_store()
        .read(cx)
        .buffers()
        .filter(|buffer| buffer.read(cx).file().and_then(|file| project.worktree_for_id(file.worktree_id(cx), cx)).is_some_and(|worktree| worktree.read(cx).is_visible()))
        .collect()
}
