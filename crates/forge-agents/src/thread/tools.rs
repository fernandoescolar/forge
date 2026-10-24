//! Calls to the tools of Forge's MCP server (`forge_mcp`): those that answer at once, and
//! the cards that wait for the user (questions, notes, edits, extensions' and Forge's tools
//! that run code).

use super::*;

impl Thread {
    /// The workspace's folders (the thread's root first).
    pub(super) fn roots(&self, cx: &gpui::App) -> Vec<PathBuf> {
        let mut roots = vec![self.root.clone()];
        if let Some(project) = self.project.upgrade() {
            roots.extend(project.read(cx).visible_worktrees(cx).map(|wt| wt.read(cx).abs_path().to_path_buf()));
        }
        roots
    }

    /// What Forge's tools work on, for this thread.
    pub(super) fn ide(&self) -> crate::forge_tools::Ide {
        forge_ui::ide_tools::ToolContext { project: self.project.clone(), workspace: self.workspace.clone(), root: self.root.clone() }
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
                let ide = self.ide();
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
                let ide = self.ide();
                cx.spawn_in(window, async move |_, cx| {
                    let _ = reply.send(run_forge_tool(ide, &name, args, cx).await);
                })
                .detach();
            }
        }
    }

    /// Answers the card of Forge's tool at `ix`: runs it, or tells the agent no.
    pub(crate) fn answer_forge_tool(&mut self, ix: usize, allow: bool, window: &mut Window, cx: &mut Context<Self>) {
        use crate::forge_tools::EditState;
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
        let (name, args, ide) = (name.clone(), args.clone(), self.ide());
        self.changed(cx);
        cx.spawn_in(window, async move |this, cx| {
            let answer = run_forge_tool(ide, &name, args, cx).await;
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
    pub(super) fn notify_user(&mut self, message: String, cx: &mut Context<Self>) {
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
    pub(super) fn extension_tool(&mut self, name: String, args: Value, reply: oneshot::Sender<crate::forge_mcp::ToolReply>, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn propose_edit(&mut self, name: String, args: Value, reply: oneshot::Sender<crate::forge_mcp::ToolReply>, window: &mut Window, cx: &mut Context<Self>) {
        let review = self.config.as_ref().is_none_or(|c| c.review_writes) && !self.permissions.get().skips_review();
        let ide = self.ide();
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
    pub(super) fn edited(&mut self, plan: &crate::forge_tools::EditPlan, applied: Result<Vec<WriteRecord>, String>, cx: &mut Context<Self>) -> crate::forge_mcp::ToolReply {
        let records = applied?;
        if records.is_empty() {
            return Ok(format!("Nothing to change: {} changed no file.", plan.summary()));
        }
        let files: Vec<String> = records.iter().map(|r| self.ide().show(&r.path)).collect();
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
        let ide = self.ide();
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

/// What the thread calls one of Forge's tools ("Run tests").
pub(crate) fn forge_tool_title(name: &str) -> String {
    crate::forge_mcp::tool_title(name).unwrap_or_else(|| "Use a Forge tool".into())
}

/// Runs one of Forge's tools that answer from the project (no card of their own): this
/// crate's, or one another crate registered (`forge_ui::ide_tools`).
pub(super) async fn run_forge_tool(ide: crate::forge_tools::Ide, name: &str, args: Value, cx: &mut gpui::AsyncWindowContext) -> crate::forge_mcp::ToolReply {
    use crate::forge_tools as tools;
    match name {
        "diagnostics" => tools::diagnostics(ide, args, cx).await,
        "go_to_definition" => tools::definition(ide, args, cx).await,
        "find_references" => tools::references(ide, args, cx).await,
        "hover" => tools::hover(ide, args, cx).await,
        "workspace_symbols" => tools::workspace_symbols(ide, args, cx).await,
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
