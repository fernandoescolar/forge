//! Offers `git init` (branch `main`) for folders that are not in a git repository.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use db::kvp::KeyValueStore;
use gpui::TaskExt as _;
use gpui::{App, AppContext as _, Context, Entity, Window, actions};
use project::Project;
use util::ResultExt as _;
use workspace::{
    Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

actions!(forge_git, [InitRepository]);

/// Used when the user's git config has no `init.defaultBranch`.
pub const DEFAULT_BRANCH: &str = "main";

struct InitOffer;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &InitRepository, window, cx| init_repository(workspace, window, cx));
        if window.is_some() {
            offer_init_when_needed(workspace, cx);
        }
    })
    .detach();
}

/// The folder to initialize: the first one open, if no repository covers it.
pub fn uninitialized_root(project: &Entity<Project>, cx: &App) -> Option<Arc<Path>> {
    let project = project.read(cx);
    let root = project.visible_worktrees(cx).next()?.read(cx).abs_path();
    let covered = project.repositories(cx).values().any(|repo| root.starts_with(&*repo.read(cx).work_directory_abs_path));
    (!covered).then_some(root)
}

pub fn init_repository(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let project = workspace.project().clone();
    let Some(root) = uninitialized_root(&project, cx) else { return };
    let task = project.read(cx).git_init(root.clone(), DEFAULT_BRANCH.into(), cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let result = task.await;
        workspace.update(cx, |workspace, cx| {
            workspace.dismiss_notification(&NotificationId::unique::<InitOffer>(), cx);
            if let Err(err) = result {
                workspace.show_error(err, cx);
            }
        })
    })
    .detach_and_log_err(cx);
}

fn dismissed_key(root: &Path) -> String {
    format!("forge-git-init-dismissed:{}", root.display())
}

/// Once the project has been scanned (repositories show up a moment after the folder),
/// offers to initialize one unless the user already said no for this folder.
fn offer_init_when_needed(workspace: &Workspace, cx: &mut Context<Workspace>) {
    let project = workspace.project().clone();
    cx.spawn(async move |workspace, cx| {
        cx.background_executor().timer(Duration::from_secs(3)).await;
        let Some(root) = cx.update(|cx| uninitialized_root(&project, cx)) else { return };
        let kvp = cx.update(|cx| KeyValueStore::global(cx));
        let key = dismissed_key(&root);
        if kvp.read_kvp(&key).ok().flatten().is_some() {
            return;
        }
        workspace
            .update(cx, |workspace, cx| {
                let folder = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                workspace.show_notification(NotificationId::unique::<InitOffer>(), cx, move |cx| {
                    cx.new(move |cx| {
                        MessageNotification::new(format!("“{folder}” is not a git repository. Track its history with git?"), cx)
                            .primary_message(format!("Initialize repository ({DEFAULT_BRANCH})"))
                            .primary_on_click(|window, cx| window.dispatch_action(Box::new(InitRepository), cx))
                            .secondary_message("Not now")
                            .secondary_on_click(move |_, cx| {
                                let (kvp, key) = (kvp.clone(), key.clone());
                                cx.background_spawn(async move { kvp.write_kvp(key, "1".into()).await.log_err() }).detach();
                                cx.emit(gpui::DismissEvent);
                            })
                    })
                });
            })
            .ok();
    })
    .detach();
}
