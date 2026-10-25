//! A thread's own git worktree (see `worktree`): applying its changes to the project, and
//! removing it.

use super::*;

impl Thread {
    /// Applies the worktree's changes to the project, as uncommitted changes.
    pub(crate) fn apply_worktree(&mut self, cx: &mut Context<Self>) {
        let Some(worktree) = self.worktree.clone() else { return };
        if self.status == Status::Busy {
            self.system("The agent is still working; apply its changes once it is done.", Color::Warning);
            return self.changed(cx);
        }
        let repo = worktree.repo.clone();
        let task = cx.background_spawn(async move { crate::worktree::apply(&worktree) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(crate::worktree::Applied::Clean { files: 0 }) => this.system("The worktree has no changes to apply.", Color::Muted),
                    Ok(crate::worktree::Applied::Clean { files }) => this.system(
                        format!("Applied the changes to {files} file{} in the project. They are uncommitted: review them in the Git panel.", if files == 1 { "" } else { "s" }),
                        Color::Success,
                    ),
                    Ok(crate::worktree::Applied::Conflicts(files)) => {
                        this.system(format!("Applied, with conflicts in {}: you and the agent changed the same lines. Resolve them in the merge editor (Git › Open Merge Editor…).", files.join(", ")), Color::Warning);
                        if let (Some(workspace), Some(first)) = (this.workspace.upgrade(), files.first()) {
                            let path = repo.join(first).to_string_lossy().into_owned();
                            let toast = workspace::Toast::new(workspace::notifications::NotificationId::named("forge-worktree-conflicts".into()), format!("Conflicts in {}", files.join(", ")))
                                .on_click("Open Merge Editor", move |window, cx| window.dispatch_action(Box::new(forge_ui::OpenMergeEditor { path: Some(path.clone()) }), cx));
                            workspace.update(cx, |ws, cx| ws.show_toast(toast, cx));
                        }
                    }
                    Err(e) => this.system(format!("{e:#}"), Color::Error),
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Ends the thread's work in its worktree: stops the agent and removes the worktree
    /// (and its branch unless `keep_branch`).
    pub(crate) fn remove_worktree(&mut self, keep_branch: bool, cx: &mut Context<Self>) {
        let Some(worktree) = self.worktree.clone() else { return };
        self.disconnect(cx);
        let task = cx.background_spawn(async move { crate::worktree::remove(&worktree, keep_branch).map(|_| worktree) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(worktree) => {
                        this.worktree = None;
                        this.ended = true;
                        let kept = if keep_branch { format!("; its branch {} is still there", worktree.branch()) } else { String::new() };
                        this.system(format!("Removed the worktree{kept}. This thread is over."), Color::Muted);
                    }
                    Err(e) => this.system(format!("Could not remove the worktree: {e:#}"), Color::Error),
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }
}
