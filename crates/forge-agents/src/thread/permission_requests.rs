//! Answering the agent's permission requests, and turning a `git commit` or `git push`
//! it asked to run into a proposal the user makes from Forge.

use super::*;

impl Thread {
    pub(crate) fn answer_permission(&mut self, request_id: String, option: Option<(String, String)>, cx: &mut Context<Self>) {
        let Some((agent, _)) = self.connected.clone() else { return };
        // The agent's `git commit` or `git push` becomes a proposal: it is told no, the user
        // makes it here.
        if option.as_ref().is_some_and(|(id, _)| id == COMMIT_IN_FORGE || id == PUSH_IN_FORGE) {
            let reject = self.entries.iter().find_map(|e| match e {
                Entry::Permission { request_id: r, options, .. } if *r == request_id => options.iter().find(|o| o.kind.starts_with("reject")).map(|o| o.id.clone()),
                _ => None,
            });
            let origin = crate::commit_proposal::Origin::Command;
            match self.git_redirects.remove(&request_id) {
                Some(GitRedirect::Commit(message)) => {
                    self.answer_permission(request_id, reject.map(|id| (id, "Moved to a commit you make from Forge".to_string())), cx);
                    self.propose_commit(message, vec![], origin, None, cx);
                }
                Some(GitRedirect::Push) => {
                    self.answer_permission(request_id, reject.map(|id| (id, "Moved to a push you make from Forge".to_string())), cx);
                    self.propose_push(None, origin, None, cx);
                }
                None => {}
            }
            return;
        }
        self.git_redirects.remove(&request_id);
        let allowed = option.as_ref().is_some_and(|(id, _)| {
            self.entries.iter().any(|e| matches!(e, Entry::Permission { request_id: r, options, .. } if *r == request_id && options.iter().any(|o| &o.id == id && o.allow)))
        });
        for entry in &mut self.entries {
            if let Entry::Permission { request_id: r, resolved, diffs, .. } = entry {
                if *r == request_id {
                    *resolved = Some(option.as_ref().map(|(_, name)| name.clone()).unwrap_or_else(|| "Cancelled".into()));
                    // The user has seen and approved these exact diffs: don't review the writes again.
                    if allowed {
                        for d in diffs.iter() {
                            self.approved_edits.approve(PathBuf::from(&d.edit.path), d.edit.new_text.clone());
                        }
                    }
                }
            }
        }
        let outcome = match option {
            Some((option_id, _)) => PermissionOutcome::Selected { option_id },
            None => PermissionOutcome::Cancelled,
        };
        let rt = self.runtime.clone();
        Tokio::spawn(cx, async move {
            if let Err(e) = rt.respond_permission(&agent, &request_id, outcome).await {
                log::warn!("couldn't answer {agent}'s permission request: {e}");
            }
        })
        .detach();
        self.changed(cx);
    }
}
