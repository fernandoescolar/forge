//! Signing in to the agent: the sign-in card, its methods (a terminal login, an API key
//! the agent asks for) and what is said when it fails.

use super::*;

impl Thread {
    /// Adds a sign-in card (or, if the agent offers no login methods, instructions).
    pub(super) fn show_auth(&mut self, reason: &str, cx: &mut Context<Self>) {
        let pending = self.entries.iter().any(|e| matches!(e, Entry::Auth { state: AuthState::Waiting | AuthState::Running(_), .. }));
        if pending {
            return;
        }
        if self.status == Status::Connecting {
            // Login methods arrive with the connection result; decide then.
            self.auth_pending = Some(reason.to_string());
            return;
        }
        if self.auth_methods.is_empty() {
            let hint = self.login_hint();
            self.system(format!("{reason} {hint}"), Color::Warning);
        } else {
            self.system(reason.to_string(), Color::Warning);
            self.entries.push(Entry::Auth { methods: self.auth_methods.clone(), terminal: None, state: AuthState::Waiting });
        }
        self.changed(cx);
    }

    pub(super) fn set_auth_state(&mut self, ix: usize, new_state: AuthState, cx: &mut Context<Self>) {
        if let Some(Entry::Auth { state, .. }) = self.entries.get_mut(ix) {
            *state = new_state;
        }
        self.changed(cx);
    }

    /// Runs login method `m` of the sign-in card at `ix`; on success restarts the agent.
    pub(crate) fn run_auth(&mut self, ix: usize, m: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Entry::Auth { methods, .. }) = self.entries.get(ix) else { return };
        let Some(method) = methods.get(m).cloned() else { return };
        self.set_auth_state(ix, AuthState::Running(method.name.clone()), cx);
        match method.action {
            AuthAction::Terminal { command, args, env } => {
                let shell_env = self.project.upgrade().map(|project| {
                    let root: Arc<std::path::Path> = self.root.clone().into();
                    project.update(cx, |p, cx| p.environment().update(cx, |env, cx| env.directory_environment(root, cx)))
                });
                let (project, workspace, root) = (self.project.clone(), self.workspace.clone(), self.root.clone());
                cx.spawn_in(window, async move |this, cx| {
                    let mut full_env: Vec<(String, String)> = match shell_env {
                        Some(task) => task.await.unwrap_or_default().into_iter().collect(),
                        None => vec![],
                    };
                    full_env.extend(env);
                    let request = TerminalRequest { command, args, env: full_env, cwd: Some(root), output_byte_limit: None };
                    let terminal = match spawn_interactive(&project, request, cx).await {
                        Ok(t) => t,
                        Err(e) => {
                            this.update(cx, |this, cx| this.set_auth_state(ix, AuthState::Failed(format!("{e:#}")), cx)).ok();
                            return;
                        }
                    };
                    let exit = this.update_in(cx, |this, window, cx| {
                        let view = cx.new(|cx| {
                            let mut view = TerminalView::new(terminal.clone(), workspace.clone(), None, project.clone(), window, cx);
                            view.set_embedded_mode(Some(14), cx);
                            view
                        });
                        // Focus it so the user can type straight away (codes, confirmations).
                        window.focus(&view.focus_handle(cx), cx);
                        if let Some(Entry::Auth { terminal: slot, .. }) = this.entries.get_mut(ix) {
                            *slot = Some(view);
                        }
                        this.changed(cx);
                        terminal.read(cx).wait_for_completed_task(cx)
                    });
                    let Ok(exit) = exit else { return };
                    let status = exit.await;
                    this.update_in(cx, |this, window, cx| match status {
                        Some(s) if s.success() => this.finish_auth(ix, window, cx),
                        Some(s) => this.set_auth_state(ix, AuthState::Failed(format!("login exited with {s}")), cx),
                        None => this.set_auth_state(ix, AuthState::Failed("login was interrupted".into()), cx),
                    })
                    .ok();
                })
                .detach();
            }
            AuthAction::Authenticate => {
                let Some((agent, _)) = self.connected.clone() else { return };
                let rt = self.runtime.clone();
                let id = method.id.clone();
                let task = Tokio::spawn_result(cx, async move { Ok(rt.rpc(&agent, "authenticate", json!({ "methodId": id })).await?) });
                cx.spawn_in(window, async move |this, cx| {
                    let result = task.await;
                    this.update_in(cx, |this, window, cx| match result {
                        Ok(_) => this.finish_auth(ix, window, cx),
                        Err(e) => this.set_auth_state(ix, AuthState::Failed(format!("{e:#}")), cx),
                    })
                    .ok();
                })
                .detach();
            }
        }
    }

    pub(super) fn finish_auth(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.set_auth_state(ix, AuthState::Done, cx);
        self.system("Signed in. Restarting the agent…", Color::Success);
        self.restart_keeping_transcript = true;
        self.connect(window, cx);
    }

    /// How to sign in to the selected agent (agents authenticate through their own CLIs).
    pub(super) fn login_hint(&self) -> String {
        let spec = self.agents.get(self.selected);
        let launches = |needle: &str| spec.is_some_and(|a| a.command.contains(needle) || a.args.iter().any(|x| x.contains(needle)));
        if launches("claude") {
            "Run `claude` in a terminal and type `/login` (it shares credentials with the ACP adapter), then press + for a new session.".into()
        } else if launches("gemini") {
            "Run `gemini` in a terminal and complete the login, then press + for a new session.".into()
        } else if launches("codex") {
            "Run `codex login` in a terminal, then press + for a new session.".into()
        } else if launches("copilot") {
            "Run `npx @github/copilot login` in a terminal (or `copilot login`), then press + for a new session.".into()
        } else {
            "Sign in with the agent's own CLI, then press + for a new session.".into()
        }
    }
}
