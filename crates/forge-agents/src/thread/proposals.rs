//! Commit and push proposals: the cards the agent's `propose_commit` / `propose_push` (or
//! a redirected `git commit` / `git push`) show, and what the user does with them.

use super::*;

impl Thread {
    /// Shows a commit card for `files` (relative to the thread's folder; every change when
    /// empty), signed off when the project signs off.
    pub(super) fn propose_commit(&mut self, message: String, files: Vec<String>, origin: crate::commit_proposal::Origin, reply: Option<oneshot::Sender<crate::forge_mcp::ToolReply>>, cx: &mut Context<Self>) {
        let (root, project_root) = (self.root.clone(), self.project_root(cx));
        // Only the user signs off, through git.
        let message = forge_git::signoff::without_trailer(&message);
        cx.spawn(async move |this, cx| {
            let resolved = cx
                .background_spawn(async move {
                    let files = crate::commit_proposal::resolve_files(&root, &files)?;
                    anyhow::Ok((files, forge_git::signoff::configured(&project_root).unwrap_or(false), forge_git::signoff::signer(&root)))
                })
                .await;
            this.update(cx, |this, cx| {
                match resolved {
                    Ok((files, signoff, signer)) => {
                        let state = crate::commit_proposal::CommitState::Waiting;
                        this.entries.push(Entry::Commit { message, files, signoff, signer, origin, state, reply });
                    }
                    Err(e) => {
                        let e = format!("{e:#}");
                        this.system(format!("The agent proposed a commit, but {e}."), Color::Warning);
                        if let Some(reply) = reply {
                            let _ = reply.send(Err(format!("Nothing to commit: {e}")));
                        }
                    }
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// The commit card at `ix`, while the user can still act on it: its message, files and
    /// whether it is signed off.
    fn open_commit(&self, ix: usize) -> Option<(String, Vec<PathBuf>, bool)> {
        match self.entries.get(ix) {
            Some(Entry::Commit { message, files, signoff, state, .. }) if state.is_open() => Some((message.clone(), files.clone(), *signoff)),
            _ => None,
        }
    }

    /// Commits the proposal at `ix` as it is.
    pub(crate) fn commit_proposal(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some((message, files, signoff)) = self.open_commit(ix) else { return };
        self.set_commit_state(ix, crate::commit_proposal::CommitState::Committing, cx);
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { crate::commit_proposal::commit(&root, &files, &message, signoff) }).await;
            let state = match result {
                Ok((sha, message)) => crate::commit_proposal::CommitState::Committed { sha, message },
                Err(e) => crate::commit_proposal::CommitState::Failed(format!("{e:#}")),
            };
            this.update(cx, |this, cx| this.set_commit_state(ix, state, cx)).ok();
        })
        .detach();
    }

    /// Stages the proposal's files and puts its message in the Git panel, where the user
    /// edits and commits it; the card follows HEAD to see the commit made.
    pub(crate) fn commit_proposal_in_panel(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        use crate::commit_proposal::{CommitState, head, last_commit, stage_only};
        // The Git panel's Signoff toggle follows the project's setting, like the card.
        let Some((message, files, _)) = self.open_commit(ix) else { return };
        let (root, workspace) = (self.root.clone(), self.workspace.clone());
        cx.spawn_in(window, async move |this, cx| {
            let staged = cx.background_spawn({
                let root = root.clone();
                async move { stage_only(&root, &files).map(|()| head(&root)) }
            });
            let before = match staged.await {
                Ok(before) => before,
                Err(e) => {
                    this.update(cx, |this, cx| this.set_commit_state(ix, CommitState::Failed(format!("{e:#}")), cx)).ok();
                    return;
                }
            };
            this.update(cx, |this, cx| this.set_commit_state(ix, CommitState::InPanel { head: before.clone() }, cx)).ok();
            workspace
                .update_in(cx, |ws, window, cx| {
                    let Some(panel) = ws.panel::<git_ui::git_panel::GitPanel>(cx) else { return };
                    panel.read(cx).commit_message_buffer(cx).update(cx, |b, cx| b.set_text(message, cx));
                    ws.focus_panel::<git_ui::git_panel::GitPanel>(window, cx);
                })
                .ok();
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(800)).await;
                let waiting = this.read_with(cx, |t, _| matches!(t.entries.get(ix), Some(Entry::Commit { state: CommitState::InPanel { .. }, .. }))).unwrap_or(false);
                if !waiting {
                    break;
                }
                let root = root.clone();
                let before = before.clone();
                let made = cx.background_spawn(async move { (head(&root) != before).then(|| last_commit(&root).ok()).flatten() }).await;
                if let Some((sha, message)) = made {
                    this.update(cx, |this, cx| this.set_commit_state(ix, CommitState::Committed { sha, message }, cx)).ok();
                    break;
                }
            }
        })
        .detach();
    }

    /// Signs the proposal at `ix` off, or stops signing it off; the project keeps the
    /// choice for its next commits.
    pub(crate) fn toggle_commit_signoff(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some((_, _, signoff)) = self.open_commit(ix) else { return };
        if let Some(Entry::Commit { signoff: on, .. }) = self.entries.get_mut(ix) {
            *on = !signoff;
        }
        forge_git::signoff::set(self.project_root(cx), !signoff, cx);
        self.changed(cx);
    }

    /// The project's folder, whose `.forge/` holds its settings (not the thread's worktree,
    /// which has its own copy).
    fn project_root(&self, cx: &gpui::App) -> PathBuf {
        self.project.upgrade().and_then(|project| forge_git::signoff::project_root(project.read(cx), cx)).unwrap_or_else(|| self.root.clone())
    }

    pub(crate) fn decline_commit_proposal(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.open_commit(ix).is_some() {
            self.set_commit_state(ix, crate::commit_proposal::CommitState::Declined, cx);
        }
    }

    /// Moves the card at `ix` on; a decision (commit made or declined) answers the agent.
    fn set_commit_state(&mut self, ix: usize, new: crate::commit_proposal::CommitState, cx: &mut Context<Self>) {
        use crate::commit_proposal::CommitState;
        if let Some(Entry::Commit { state, reply, .. }) = self.entries.get_mut(ix) {
            *state = new;
            if matches!(state, CommitState::Committed { .. } | CommitState::Declined) {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(crate::commit_proposal::report(state));
                }
            }
        }
        self.changed(cx);
    }

    /// Shows a push card for the thread's current branch (to `remote`, or its upstream).
    pub(super) fn propose_push(&mut self, remote: Option<String>, origin: crate::commit_proposal::Origin, reply: Option<oneshot::Sender<crate::forge_mcp::ToolReply>>, cx: &mut Context<Self>) {
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let plan = cx.background_spawn(async move { crate::push_proposal::plan(&root, remote.as_deref()) }).await;
            this.update(cx, |this, cx| {
                match plan {
                    Ok(plan) => {
                        let state = crate::push_proposal::PushState::Waiting;
                        this.entries.push(Entry::Push { plan, origin, state, reply });
                    }
                    Err(e) => {
                        let e = format!("{e:#}");
                        this.system(format!("The agent proposed a push, but {e}."), Color::Warning);
                        if let Some(reply) = reply {
                            let _ = reply.send(Err(format!("Nothing to push: {e}")));
                        }
                    }
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Pushes as the card at `ix` says, through Zed's repository: credentials are asked as
    /// in the Git panel.
    pub(crate) fn push_proposal(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        use crate::push_proposal::PushState;
        let plan = match self.entries.get(ix) {
            Some(Entry::Push { plan, state, .. }) if state.is_open() => plan.clone(),
            _ => return,
        };
        // The repository the thread's folder is in (a worktree's commits are in it too).
        let repo = self.project.upgrade().and_then(|project| {
            let store = project.read(cx).git_store().read(cx);
            store.repositories().values().filter(|r| self.root.starts_with(&r.read(cx).work_directory_abs_path)).max_by_key(|r| r.read(cx).work_directory_abs_path.as_os_str().len()).cloned()
        });
        let Some(repo) = repo else {
            self.set_push_state(ix, PushState::Failed("Forge doesn't know this folder's repository.".into()), cx);
            return;
        };
        self.set_push_state(ix, PushState::Pushing, cx);
        let (workspace, window_handle) = (self.workspace.clone(), window.window_handle());
        cx.spawn_in(window, async move |this, cx| {
            let operation: gpui::SharedString = format!("git push {} {}", plan.remote, plan.branch).into();
            let askpass = askpass::AskPassDelegate::new_with_cancellation(cx, move |prompt, tx, cancellation, cx| {
                window_handle
                    .update(cx, |_, window, cx| {
                        workspace
                            .update(cx, |ws, cx| ws.toggle_modal(window, cx, |window, cx| git_ui_core::askpass_modal::AskPassModal::new(operation.clone(), prompt.into(), tx, cancellation, window, cx)))
                            .ok();
                    })
                    .ok();
            });
            let options = plan.set_upstream.then_some(git::repository::PushOptions::SetUpstream);
            let pushed = repo.update(cx, |repo, cx| repo.push(plan.branch.clone().into(), plan.remote_branch.clone().into(), plan.remote.clone().into(), options, askpass, cx));
            let state = match pushed.await {
                Ok(Ok(_)) => PushState::Pushed,
                Ok(Err(e)) => PushState::Failed(format!("{e:#}")),
                Err(_) => PushState::Failed("The push was canceled.".into()),
            };
            this.update(cx, |this, cx| this.set_push_state(ix, state, cx)).ok();
        })
        .detach();
    }

    pub(crate) fn decline_push_proposal(&mut self, ix: usize, cx: &mut Context<Self>) {
        if matches!(self.entries.get(ix), Some(Entry::Push { state, .. }) if state.is_open()) {
            self.set_push_state(ix, crate::push_proposal::PushState::Declined, cx);
        }
    }

    /// Moves the card at `ix` on; a decision (pushed or declined) answers the agent.
    fn set_push_state(&mut self, ix: usize, new: crate::push_proposal::PushState, cx: &mut Context<Self>) {
        use crate::push_proposal::PushState;
        if let Some(Entry::Push { plan, state, reply, .. }) = self.entries.get_mut(ix) {
            *state = new;
            if matches!(state, PushState::Pushed | PushState::Declined) {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(crate::push_proposal::report(plan, state));
                }
            }
        }
        self.changed(cx);
    }
}
