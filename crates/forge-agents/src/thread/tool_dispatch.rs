//! Calls to Forge's own tools (see `forge_mcp`): answered right away, or as cards the
//! user answers (questions, notes, settings, commands, extension tools, edits).

use super::*;

impl Thread {
    /// The calls to Forge's tools still waiting for the user end with the turn: their cards
    /// close and the agent hears so.
    pub(super) fn close_waiting_calls(&mut self) {
        use crate::forge_tools::EditState;
        const CANCELLED: &str = "The user cancelled the turn before answering.";
        for entry in &mut self.entries {
            let reply = match entry {
                Entry::Question { answer, reply, .. } if answer.is_none() && reply.is_some() => {
                    *answer = Some(String::new());
                    reply.take()
                }
                Entry::Commit { state, reply, .. } if matches!(state, crate::commit_proposal::CommitState::Waiting | crate::commit_proposal::CommitState::InPanel { .. }) && reply.is_some() => {
                    *state = crate::commit_proposal::CommitState::Declined;
                    reply.take()
                }
                Entry::Push { state, reply, .. } if *state == crate::push_proposal::PushState::Waiting && reply.is_some() => {
                    *state = crate::push_proposal::PushState::Declined;
                    reply.take()
                }
                Entry::LspEdit { state, reply, .. } | Entry::ExtensionTool { state, reply, .. } | Entry::ForgeTool { state, reply, .. } | Entry::Remember { state, reply, .. } | Entry::Config { state, reply, .. } | Entry::Command { state, reply, .. }
                    if *state == EditState::Waiting && reply.is_some() =>
                {
                    *state = EditState::Declined;
                    reply.take()
                }
                _ => None,
            };
            if let Some(reply) = reply {
                let _ = reply.send(Err(CANCELLED.into()));
            }
        }
    }

    /// The workspace's folders (the thread's root first).
    pub(super) fn roots(&self, cx: &gpui::App) -> Vec<PathBuf> {
        let mut roots = vec![self.root.clone()];
        if let Some(project) = self.project.upgrade() {
            roots.extend(project.read(cx).visible_worktrees(cx).map(|wt| wt.read(cx).abs_path().to_path_buf()));
        }
        roots
    }

    /// What Forge's tools work on, for this thread.
    fn ide(&self, cx: &gpui::App) -> crate::forge_tools::Ide {
        crate::forge_tools::Ide {
            project: self.project.clone(),
            workspace: self.workspace.clone(),
            root: self.root.clone(),
            roots: self.roots(cx),
            outside_ok: self.permissions.get().may_touch_outside(),
        }
    }

    /// A call to a tool of Forge's MCP server (see `forge_mcp`).
    pub(crate) fn handle_tool_request(&mut self, request: crate::forge_mcp::ToolRequest, window: &mut Window, cx: &mut Context<Self>) {
        use crate::commit_proposal::Origin;
        use crate::forge_tools::{self as tools, str_arg};
        let crate::forge_mcp::ToolRequest { name, args, reply } = request;
        match name.as_str() {
            "propose_commit" => {
                let Some(message) = str_arg(&args, "message") else {
                    let _ = reply.send(Err("`message` is empty.".into()));
                    return;
                };
                let files = args.get("files").and_then(Value::as_array).map(|fs| fs.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
                self.propose_commit(message, files, Origin::Tool, Some(reply), cx);
            }
            "propose_push" => self.propose_push(str_arg(&args, "remote"), Origin::Tool, Some(reply), cx),
            "rename_symbol" | "apply_code_action" | "format_file" => self.propose_edit(name, args, reply, window, cx),
            "ask_user" => {
                let Some(question) = str_arg(&args, "question") else {
                    let _ = reply.send(Err("`question` is empty.".into()));
                    return;
                };
                let options: Vec<String> = args.get("options").and_then(Value::as_array).map(|xs| xs.iter().filter_map(Value::as_str).map(str::trim).filter(|o| !o.is_empty()).take(6).map(str::to_string).collect()).unwrap_or_default();
                let input = cx.new(|cx| {
                    let mut editor = editor::Editor::single_line(window, cx);
                    editor.set_placeholder_text(if options.is_empty() { "Your answer…" } else { "Or write another answer…" }, window, cx);
                    editor
                });
                self.entries.push(Entry::Question { question, options, input, answer: None, reply: Some(reply) });
                self.changed(cx);
            }
            "user_context" => {
                let ide = self.ide(cx);
                cx.spawn_in(window, async move |_, cx| {
                    let _ = reply.send(tools::user_context(ide, cx).await);
                })
                .detach();
            }
            "remember" => {
                let Some(note) = str_arg(&args, "note") else {
                    let _ = reply.send(Err("`note` is empty.".into()));
                    return;
                };
                let file = crate::rules::project_file(&self.root, &self.instructions_files());
                let editor = cx.new(|cx| {
                    let mut editor = editor::Editor::auto_height(1, 8, window, cx);
                    editor.set_text(note, window, cx);
                    editor
                });
                self.entries.push(Entry::Remember { note: editor, file, state: crate::forge_tools::EditState::Waiting, reply: Some(reply) });
                self.changed(cx);
            }
            "notify" => {
                let Some(message) = str_arg(&args, "message") else {
                    let _ = reply.send(Err("`message` is empty.".into()));
                    return;
                };
                self.notify_user(message.clone(), cx);
                let _ = reply.send(Ok("The user was notified.".into()));
            }
            "forge_settings" => {
                let pages = forge_ui::settings_registry::SettingsRegistry::global(cx).read(cx).pages().to_vec();
                let answer = crate::configure::search_settings(&pages, &str_arg(&args, "query").unwrap_or_default(), str_arg(&args, "page").as_deref(), &|file| file.read());
                let _ = reply.send(Ok(answer));
            }
            "forge_keybindings" => {
                let _ = reply.send(crate::configure::describe_bindings(str_arg(&args, "keys").as_deref(), str_arg(&args, "action").as_deref(), cx));
            }
            "forge_guide" => {
                let _ = reply.send(Ok(crate::configure::guide(str_arg(&args, "topic").as_deref())));
            }
            "run_command" => {
                let Some(command) = str_arg(&args, "command") else {
                    let _ = reply.send(Err("`command` is missing.".into()));
                    return;
                };
                // Outside the workspace it asks (see `permissions::decide`).
                let cwd = str_arg(&args, "cwd").map(|c| crate::forge_tools::normalize(&self.root.join(c))).unwrap_or_else(|| self.root.clone());
                let timeout = std::time::Duration::from_secs(args.get("timeout_seconds").and_then(Value::as_u64).unwrap_or(600).clamp(1, 3600));
                let kept = crate::agent_ignore::AgentIgnore::load(&self.root);
                let call = json!({ "kind": "execute", "title": command, "rawInput": { "command": command, "cwd": cwd } });
                let decision = crate::permissions::decide(&self.permissions.get(), &call, &self.roots(cx), &kept);
                if let crate::permissions::Decision::Deny(reason) = decision {
                    let _ = reply.send(Err(format!("{reason}: the user keeps those files from agents.")));
                    return;
                }
                let filter = match str_arg(&args, "filter").map(|f| regex::Regex::new(&f)) {
                    Some(Err(e)) => {
                        let _ = reply.send(Err(format!("Bad `filter` regex: {e}")));
                        return;
                    }
                    other => other.and_then(Result::ok),
                };
                let lines = |key: &str| args.get(key).and_then(Value::as_u64).map(|n| n as usize);
                let (head, tail) = (lines("head"), lines("tail"));
                // The agent gets only the part of the output it asked for; the user sees it all.
                let (inner, answer) = oneshot::channel::<crate::forge_mcp::ToolReply>();
                cx.background_spawn(async move {
                    if let Ok(result) = answer.await {
                        let _ = reply.send(result.map(|text| crate::forge_read::filter_output(&text, filter.as_ref(), head, tail)));
                    }
                })
                .detach();
                let ix = self.entries.len();
                self.entries.push(Entry::Command { command, cwd, timeout, state: crate::forge_tools::EditState::Waiting, terminal: None, exit: None, reply: Some(inner) });
                if let crate::permissions::Decision::Allow(_) = decision {
                    self.start_command(ix, window, cx);
                }
                self.changed(cx);
            }
            "change_settings" | "change_keybinding" => {
                let planned = if name == "change_settings" {
                    let pages = forge_ui::settings_registry::SettingsRegistry::global(cx).read(cx).pages().to_vec();
                    crate::configure::plan_setting_changes(&pages, args.get("changes").unwrap_or(&Value::Null), &|file| file.read()).map(|l| l.into_iter().map(crate::configure::ConfigChange::Setting).collect())
                } else {
                    crate::configure::plan_binding_change(&args, cx).map(|b| vec![crate::configure::ConfigChange::Binding(b)])
                };
                match planned {
                    Ok(changes) => {
                        self.entries.push(Entry::Config { changes, state: crate::forge_tools::EditState::Waiting, reply: Some(reply) });
                        self.changed(cx);
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            "show_changes" => {
                let answer = if self.changes.is_empty() {
                    Ok("You haven't changed any file in this conversation.".to_string())
                } else {
                    cx.emit(ThreadEvent::ShowChanges(str_arg(&args, "path").map(|p| self.root.join(p))));
                    Ok(format!("Opened the review of your changes ({} files) for the user.", self.changes.len()))
                };
                let _ = reply.send(answer);
            }
            _ if forge_ui::agent_tools::agent_tools().get(&name).is_some() => self.extension_tool(name, args, reply, window, cx),
            _ if crate::permissions::forge_tool_asks(&self.permissions.get(), crate::permissions::forge_tool_effect(&name)) => {
                self.entries.push(Entry::ForgeTool { name, args, state: crate::forge_tools::EditState::Waiting, reply: Some(reply) });
                self.changed(cx);
            }
            _ => {
                let run = self.forge_tool_run(cx);
                cx.spawn_in(window, async move |_, cx| {
                    let _ = reply.send(run_forge_tool(run, &name, args, cx).await);
                })
                .detach();
            }
        }
    }

    /// What Forge's tools run with: the project, and the files this thread changed (and
    /// still has changed) with their problems before it did.
    fn forge_tool_run(&self, cx: &gpui::App) -> ForgeToolRun {
        ForgeToolRun { ide: self.ide(cx), changed: self.changes.iter().map(|c| c.path.clone()).collect(), baseline: self.problem_baseline.clone() }
    }

    /// Answers the card of Forge's tool at `ix`: runs it, or tells the agent no.
    pub(crate) fn answer_forge_tool(&mut self, ix: usize, allow: bool, window: &mut Window, cx: &mut Context<Self>) {
        use crate::forge_tools::EditState;
        let run = self.forge_tool_run(cx);
        let Some(Entry::ForgeTool { name, args, state, reply }) = self.entries.get_mut(ix) else { return };
        if !state.is_open() {
            return;
        }
        let Some(reply) = reply.take() else { return };
        if !allow {
            *state = EditState::Declined;
            let _ = reply.send(Err(format!("The user didn't let you {}. Nothing was done.", forge_tool_title(name).to_lowercase())));
            self.changed(cx);
            return;
        }
        *state = EditState::Applying;
        let (name, args) = (name.clone(), args.clone());
        self.changed(cx);
        cx.spawn_in(window, async move |this, cx| {
            let answer = run_forge_tool(run, &name, args, cx).await;
            this.update(cx, |this, cx| {
                if let Some(Entry::ForgeTool { state, .. }) = this.entries.get_mut(ix) {
                    *state = match &answer {
                        Ok(_) => EditState::Applied,
                        Err(e) => EditState::Failed(e.clone()),
                    };
                }
                this.changed(cx);
            })
            .ok();
            let _ = reply.send(answer);
        })
        .detach();
    }

    /// Answers the question at `ix`: with `answer`, or with what the user wrote when `None`.
    /// An empty answer skips it.
    pub(crate) fn answer_question(&mut self, ix: usize, answer: Option<String>, cx: &mut Context<Self>) {
        let Some(Entry::Question { input, answer: given, reply, .. }) = self.entries.get_mut(ix) else { return };
        if given.is_some() {
            return;
        }
        let text = answer.unwrap_or_else(|| input.read(cx).text(cx).trim().to_string());
        if let Some(reply) = reply.take() {
            let _ = reply.send(if text.is_empty() { Ok("The user skipped the question: decide yourself, and say what you chose.".into()) } else { Ok(format!("The user answered: {text}")) });
        }
        *given = Some(text);
        self.changed(cx);
    }

    /// `notify`: a toast in the workspace, and the message in the thread.
    fn notify_user(&mut self, message: String, cx: &mut Context<Self>) {
        struct AgentNotification;
        let name = self.agent_label();
        if let Some(workspace) = self.workspace.upgrade() {
            let text = format!("{name}: {message}");
            workspace.update(cx, |ws, cx| ws.show_toast(workspace::Toast::new(workspace::notifications::NotificationId::unique::<AgentNotification>(), text), cx));
        }
        self.system(format!("Notified you: {message}"), Color::Accent);
        self.changed(cx);
    }

    /// A call to an extension's tool: run now when it only reads (or the user lets
    /// everything run), else a card the user answers first.
    fn extension_tool(&mut self, name: String, args: Value, reply: oneshot::Sender<crate::forge_mcp::ToolReply>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tool) = forge_ui::agent_tools::agent_tools().get(&name) else {
            let _ = reply.send(Err(format!("Unknown tool: {name}")));
            return;
        };
        if tool.read_only || self.permissions.get().mode == crate::permissions::PermissionMode::SuperUser {
            let root = self.root.clone();
            cx.spawn_in(window, async move |_, _| {
                let _ = reply.send(forge_ui::agent_tools::agent_tools().run(&name, args, root).await);
            })
            .detach();
            return;
        }
        self.entries.push(Entry::ExtensionTool { tool, args, state: crate::forge_tools::EditState::Waiting, reply: Some(reply) });
        self.changed(cx);
    }

    /// Answers the extension tool card at `ix`: runs the tool, or tells the agent no.
    pub(crate) fn answer_extension_tool(&mut self, ix: usize, allow: bool, window: &mut Window, cx: &mut Context<Self>) {
        use crate::forge_tools::EditState;
        let Some(Entry::ExtensionTool { tool, args, state, reply }) = self.entries.get_mut(ix) else { return };
        if !state.is_open() {
            return;
        }
        let Some(reply) = reply.take() else { return };
        if !allow {
            *state = EditState::Declined;
            let _ = reply.send(Err(format!("The user didn't let {} run. Nothing was done.", tool.title)));
            self.changed(cx);
            return;
        }
        *state = EditState::Applying;
        let (name, args, root) = (tool.name.clone(), args.clone(), self.root.clone());
        self.changed(cx);
        cx.spawn_in(window, async move |this, cx| {
            let answer = forge_ui::agent_tools::agent_tools().run(&name, args, root).await;
            this.update(cx, |this, cx| {
                if let Some(Entry::ExtensionTool { state, .. }) = this.entries.get_mut(ix) {
                    *state = match &answer {
                        Ok(_) => EditState::Applied,
                        Err(e) => EditState::Failed(e.clone()),
                    };
                }
                this.changed(cx);
            })
            .ok();
            let _ = reply.send(answer);
        })
        .detach();
    }

    /// Answers the note card at `ix`: adds the note (as the user left it) to the project's
    /// instructions file, or tells the agent no.
    /// The user's answer to the command the agent wants to run at `ix`.
    pub(crate) fn answer_command(&mut self, ix: usize, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Entry::Command { state, reply, .. }) = self.entries.get_mut(ix) else { return };
        if *state != crate::forge_tools::EditState::Waiting {
            return;
        }
        if run {
            self.start_command(ix, window, cx);
        } else {
            *state = crate::forge_tools::EditState::Declined;
            if let Some(reply) = reply.take() {
                let _ = reply.send(Err("The user didn't let the command run.".into()));
            }
        }
        self.changed(cx);
    }

    /// Runs the command at `ix` in a terminal embedded in its card, and answers the agent
    /// with its exit code and output once it ends (or is stopped at its timeout).
    fn start_command(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Entry::Command { command, cwd, timeout, state, .. }) = self.entries.get_mut(ix) else { return };
        *state = crate::forge_tools::EditState::Applying;
        let request = ide_api::TerminalRequest { command: command.clone(), args: vec![], env: vec![], cwd: Some(cwd.clone()), output_byte_limit: Some(MAX_COMMAND_OUTPUT) };
        let (timeout, host) = (*timeout, self.terminal_host.clone());
        cx.spawn_in(window, async move |this, cx| {
            let created = Tokio::spawn_result(cx, {
                let host = host.clone();
                async move { Ok(host.create(request).await?) }
            })
            .await;
            let id = match created {
                Ok(id) => id,
                Err(e) => {
                    this.update(cx, |this, cx| this.finish_command(ix, Err(format!("Couldn't start the command: {e:#}")), cx)).ok();
                    return;
                }
            };
            this.update_in(cx, |this, window, cx| {
                if let Some(terminal) = this.terminals.get(&id) {
                    let (workspace, project) = (this.workspace.clone(), this.project.clone());
                    let view = cx.new(|cx| {
                        let mut view = TerminalView::new(terminal, workspace, None, project, window, cx);
                        view.set_embedded_mode(Some(15), cx);
                        view
                    });
                    this.terminal_views.insert(id.clone(), view);
                }
                if let Some(Entry::Command { terminal, .. }) = this.entries.get_mut(ix) {
                    *terminal = Some(id.clone());
                }
                this.changed(cx);
            })
            .ok();
            let waited = {
                let (host, id) = (host.clone(), id.clone());
                let exit = Tokio::spawn_result(cx, async move { Ok(host.wait_for_exit(&id).await?) });
                let timer = cx.background_executor().timer(timeout);
                futures::pin_mut!(exit, timer);
                match futures::future::select(exit, timer).await {
                    futures::future::Either::Left((exit, _)) => Some(exit),
                    futures::future::Either::Right(_) => None,
                }
            };
            let timed_out = waited.is_none();
            if timed_out {
                let (host, id) = (host.clone(), id.clone());
                Tokio::spawn_result(cx, async move { Ok(host.kill(&id).await?) }).await.ok();
            }
            let output = {
                let (host, id) = (host.clone(), id.clone());
                Tokio::spawn_result(cx, async move { Ok(host.output(&id).await?) }).await
            };
            let result = match output {
                Ok(out) => {
                    let code = out.exit.as_ref().and_then(|e| e.exit_code).or(waited.and_then(|w| w.ok()).and_then(|e| e.exit_code));
                    let status = if timed_out { format!("Stopped after {}s (the timeout).", timeout.as_secs()) } else { code.map(|c| format!("Exit code {c}.")).unwrap_or_else(|| "It ended.".into()) };
                    let cut = if out.truncated { "(only the end of the output)\n" } else { "" };
                    Ok(format!("{status}\n{cut}{}", out.output.trim_end()))
                }
                Err(e) => Err(format!("Couldn't read the command's output: {e:#}")),
            };
            this.update(cx, |this, cx| this.finish_command(ix, result, cx)).ok();
        })
        .detach();
    }

    fn finish_command(&mut self, ix: usize, result: Result<String, String>, cx: &mut Context<Self>) {
        let Some(Entry::Command { state, exit, reply, .. }) = self.entries.get_mut(ix) else { return };
        *exit = Some(match &result {
            Ok(text) => text.lines().next().unwrap_or_default().to_string(),
            Err(e) => e.clone(),
        });
        *state = match &result {
            Ok(_) => crate::forge_tools::EditState::Applied,
            Err(e) => crate::forge_tools::EditState::Failed(e.clone()),
        };
        if let Some(reply) = reply.take() {
            let _ = reply.send(result);
        }
        self.changed(cx);
    }

    /// Applies (or discards) the settings and key bindings the agent proposed at `ix`.
    pub(crate) fn answer_config(&mut self, ix: usize, apply: bool, cx: &mut Context<Self>) {
        use crate::forge_tools::EditState;
        let Some(Entry::Config { changes, state, reply }) = self.entries.get_mut(ix) else { return };
        if !state.is_open() {
            return;
        }
        let Some(reply) = reply.take() else { return };
        if !apply {
            *state = EditState::Declined;
            let _ = reply.send(Err("The user didn't apply the changes.".into()));
            self.changed(cx);
            return;
        }
        let changes = changes.clone();
        let result = crate::configure::apply(&changes, paths::keymap_file(), cx);
        if let Some(Entry::Config { state, .. }) = self.entries.get_mut(ix) {
            *state = match &result {
                Ok(_) => EditState::Applied,
                Err(e) => EditState::Failed(e.clone()),
            };
        }
        let _ = reply.send(result);
        self.changed(cx);
    }

    pub(crate) fn answer_remember(&mut self, ix: usize, keep: bool, cx: &mut Context<Self>) {
        use crate::forge_tools::EditState;
        let Some(Entry::Remember { note, file, state, reply }) = self.entries.get_mut(ix) else { return };
        if !state.is_open() {
            return;
        }
        let Some(reply) = reply.take() else { return };
        let text = note.read(cx).text(cx).trim().to_string();
        if !keep || text.is_empty() {
            *state = EditState::Declined;
            let _ = reply.send(Err("The user didn't keep the note.".into()));
            self.changed(cx);
            return;
        }
        *state = EditState::Applying;
        note.update(cx, |e, _| e.set_read_only(true));
        let (file, fs) = (file.clone(), <dyn fs::Fs>::global(cx));
        let shown = file.strip_prefix(&self.root).unwrap_or(&file).to_string_lossy().into_owned();
        self.changed(cx);
        cx.spawn(async move |this, cx| {
            let before = fs.load(&file).await.unwrap_or_default();
            let after = crate::rules::with_note(&before, &text);
            let written = async {
                if let Some(dir) = file.parent() {
                    fs.create_dir(dir).await?;
                }
                fs.atomic_write(file.clone(), after).await
            }
            .await;
            this.update(cx, |this, cx| {
                if let Some(Entry::Remember { state, .. }) = this.entries.get_mut(ix) {
                    *state = match &written {
                        Ok(()) => EditState::Applied,
                        Err(e) => EditState::Failed(format!("{e:#}")),
                    };
                }
                this.changed(cx);
            })
            .ok();
            let _ = reply.send(match written {
                Ok(()) => Ok(format!("Kept in {shown}: agents get it at the start of every new session.")),
                Err(e) => Err(format!("Couldn't write {shown}: {e:#}")),
            });
        })
        .detach();
    }

    /// A language-server edit: a card when the user reviews writes, else made right away.
    fn propose_edit(&mut self, name: String, args: Value, reply: oneshot::Sender<crate::forge_mcp::ToolReply>, window: &mut Window, cx: &mut Context<Self>) {
        let review = self.config.as_ref().is_none_or(|c| c.review_writes) && !self.permissions.get().skips_review();
        let ide = self.ide(cx);
        cx.spawn_in(window, async move |this, cx| {
            let plan = match crate::forge_tools::edit_plan(&name, &ide, &args, cx).await {
                Ok(plan) => plan,
                Err(e) => {
                    let _ = reply.send(Err(e));
                    return;
                }
            };
            if review {
                this.update(cx, |this, cx| {
                    this.entries.push(Entry::LspEdit { plan, state: crate::forge_tools::EditState::Waiting, reply: Some(reply) });
                    this.changed(cx);
                })
                .ok();
                return;
            }
            let applied = crate::forge_tools::apply_edit(&ide, &plan, cx).await;
            let answer = this.update(cx, |this, cx| this.edited(&plan, applied, cx)).unwrap_or_else(|_| Err("The conversation was closed.".into()));
            let _ = reply.send(answer);
        })
        .detach();
    }

    /// Records the files an edit changed (they join the conversation's changes) and says
    /// what happened.
    fn edited(&mut self, plan: &crate::forge_tools::EditPlan, applied: Result<Vec<WriteRecord>, String>, cx: &mut Context<Self>) -> crate::forge_mcp::ToolReply {
        let records = applied?;
        if records.is_empty() {
            return Ok(format!("Nothing to change: {} changed no file.", plan.summary()));
        }
        let files: Vec<String> = records.iter().map(|r| self.ide(cx).show(&r.path)).collect();
        for record in records {
            self.record_write(record, cx);
        }
        Ok(format!("Done ({}), in {}: {}", plan.summary(), crate::push_proposal::plural(files.len(), "file"), files.join(", ")))
    }

    pub(crate) fn apply_edit_proposal(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        use crate::forge_tools::EditState;
        let plan = match self.entries.get_mut(ix) {
            Some(Entry::LspEdit { plan, state, .. }) if state.is_open() => {
                *state = EditState::Applying;
                plan.clone()
            }
            _ => return,
        };
        self.changed(cx);
        let ide = self.ide(cx);
        cx.spawn_in(window, async move |this, cx| {
            let applied = crate::forge_tools::apply_edit(&ide, &plan, cx).await;
            this.update(cx, |this, cx| {
                let answer = this.edited(&plan, applied, cx);
                if let Some(Entry::LspEdit { state, reply, .. }) = this.entries.get_mut(ix) {
                    match &answer {
                        Ok(_) => {
                            *state = EditState::Applied;
                            if let Some(reply) = reply.take() {
                                let _ = reply.send(answer);
                            }
                        }
                        // Stays open: the user can try again or decline.
                        Err(e) => *state = EditState::Failed(e.clone()),
                    }
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn decline_edit(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(Entry::LspEdit { plan, state, reply }) = self.entries.get_mut(ix) {
            if state.is_open() {
                *state = crate::forge_tools::EditState::Declined;
                if let Some(reply) = reply.take() {
                    let _ = reply.send(Err(format!("The user declined: they don't want to {}. Nothing changed.", plan.summary())));
                }
            }
        }
        self.changed(cx);
    }
}

/// The title of Forge's tool `name`, for its card.
pub(crate) fn forge_tool_title(name: &str) -> String {
    crate::forge_mcp::tool_title(name).unwrap_or_else(|| "Use a Forge tool".into())
}

/// What a call to Forge's tools needs from the thread (see `Thread::forge_tool_run`).
struct ForgeToolRun {
    ide: crate::forge_tools::Ide,
    changed: Vec<PathBuf>,
    baseline: HashMap<PathBuf, Vec<crate::verify::Problem>>,
}

/// Runs one of Forge's tools that answer from the project (no card of their own): this
/// crate's, or one another crate registered (`forge_ui::ide_tools`).
async fn run_forge_tool(run: ForgeToolRun, name: &str, args: Value, cx: &mut gpui::AsyncWindowContext) -> crate::forge_mcp::ToolReply {
    use crate::forge_tools as tools;
    let ForgeToolRun { ide, changed, baseline } = run;
    match name {
        "diagnostics" => tools::diagnostics(ide, args, &changed, cx).await,
        "check_file" => tools::check_file(ide, args, &changed, &baseline, cx).await,
        "go_to_definition" => tools::definition(ide, args, cx).await,
        "find_references" => tools::references(ide, args, cx).await,
        "hover" => tools::hover(ide, args, cx).await,
        "go_to_implementation" => tools::implementations(ide, args, cx).await,
        "go_to_type_definition" => tools::type_definition(ide, args, cx).await,
        "document_symbols" => tools::document_symbols(ide, args, cx).await,
        "call_hierarchy" => tools::call_hierarchy(ide, args, cx).await,
        "signature_help" => tools::signature_help(ide, args, cx).await,
        "workspace_symbols" => tools::workspace_symbols(ide, args, cx).await,
        "read_file" => crate::forge_read::read_file(ide, args, cx).await,
        "search" => crate::forge_read::search(ide, args, cx).await,
        "git_history" => crate::forge_read::git_history(ide, args, cx).await,
        "code_actions" => tools::code_actions(ide, args, cx).await,
        "show_file" => tools::show_file(ide, args, cx).await,
        "set_breakpoint" => crate::forge_debug::set_breakpoint(ide, args, cx).await,
        "remove_breakpoint" => crate::forge_debug::remove_breakpoint(ide, args, cx).await,
        "start_debugging" => crate::forge_debug::start_debugging(ide, args, cx).await,
        "debug_step" => crate::forge_debug::debug_step(ide, args, cx).await,
        "debug_evaluate" => crate::forge_debug::debug_evaluate(ide, args, cx).await,
        "stop_debugging" => crate::forge_debug::stop_debugging(ide, cx).await,
        other => match forge_ui::ide_tools::ide_tools().get(other) {
            Some(tool) => (tool.run)(ide, args, cx.clone()).await,
            None => Err(format!("Unknown tool: {other}")),
        },
    }
}
