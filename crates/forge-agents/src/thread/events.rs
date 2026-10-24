//! What the agent reports: session updates (messages, tool calls, plans, usage),
//! permission requests, and how tool calls are titled and shown.

use super::*;

impl Thread {
    /// Records the files a finished edit tool call changed on disk (see `apply_update`).
    pub(super) fn record_tool_edits(&mut self, tool_call_id: &str, cx: &mut Context<Self>) {
        // Each file's diff: before and after, when the tool call carried whole files.
        let edits: Vec<(PathBuf, Option<String>, String)> = self
            .entries
            .iter()
            .find_map(|e| match e {
                Entry::Tool { id, diffs, .. } if id == tool_call_id => Some(diffs.iter().map(|d| (PathBuf::from(&d.edit.path), d.edit.old_text.clone(), d.edit.new_text.clone())).collect()),
                _ => None,
            })
            .unwrap_or_default();
        if edits.is_empty() {
            return;
        }
        let fs = <dyn fs::Fs>::global(cx);
        cx.spawn(async move |this, cx| {
            let mut now = Vec::new();
            for (path, old_text, new_text) in edits {
                now.push((fs.load(&path).await.ok(), path, old_text, new_text));
            }
            this.update(cx, |this, cx| {
                for (on_disk, path, old_text, new_text) in now {
                    // Deleted or unreadable: nothing to show.
                    let Some(on_disk) = on_disk else { continue };
                    // Only whole-file diffs say what the file was: a diff of the edited
                    // snippet doesn't end as the file on disk does.
                    if !same_text(&new_text, &on_disk) || old_text.as_deref() == Some(on_disk.as_str()) {
                        continue;
                    }
                    // Already recorded with this content (it went through `ProjectFs`).
                    if this.changes.iter().any(|c| c.path == path && c.current == on_disk) {
                        continue;
                    }
                    this.record_write(WriteRecord { path, old_text, new_text: on_disk }, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// The project's instructions files (agents.json `instructions_files`).
    pub(crate) fn instructions_files(&self) -> Vec<String> {
        self.config.as_ref().map(|c| c.instructions_files.clone()).unwrap_or_else(crate::config::default_instructions_files)
    }

    /// It works in its own git worktree.
    pub(crate) fn in_worktree(&self) -> bool {
        self.worktree.is_some()
    }

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
        Tokio::spawn(cx, async move { rt.respond_permission(&agent, &request_id, outcome).await }).detach();
        self.changed(cx);
    }

    pub(super) fn handle_event(&mut self, ev: IdeEvent, window: &mut Window, cx: &mut Context<Self>) {
        let IdeEvent::Agent(ev) = ev;
        let ours = |id: &str| self.connected.as_ref().is_some_and(|(a, _)| a == id) || self.status == Status::Connecting;
        match ev {
            AgentEvent::Stderr { agent_id, line } if ours(&agent_id) => {
                self.stderr.push_back(line);
                if self.stderr.len() > 200 {
                    self.stderr.pop_front();
                }
                return;
            }
            AgentEvent::Exited { agent_id, code } if ours(&agent_id) => {
                self.persist(cx);
                self.connected = None;
                self.status = Status::Disconnected;
                self.system(format!("Agent exited{}.", code.map(|c| format!(" with code {c}")).unwrap_or_default()), Color::Muted);
            }
            AgentEvent::TerminalCreated { agent_id, terminal_id } if ours(&agent_id) => {
                if let Some(terminal) = self.terminals.get(&terminal_id) {
                    let (workspace, project) = (self.workspace.clone(), self.project.clone());
                    let view = cx.new(|cx| {
                        let mut view = TerminalView::new(terminal, workspace, None, project, window, cx);
                        view.set_embedded_mode(Some(15), cx);
                        view
                    });
                    self.terminal_views.insert(terminal_id, view);
                }
            }
            AgentEvent::PermissionRequested { agent_id, request_id, params } if ours(&agent_id) => {
                let title = params.pointer("/toolCall/title").and_then(Value::as_str).unwrap_or("The agent wants to run a tool").to_string();
                let mut options: Vec<PermissionOption> = params
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|xs| {
                        xs.iter()
                            .filter_map(|o| {
                                let kind = o.get("kind").and_then(Value::as_str).unwrap_or_default().to_string();
                                Some(PermissionOption {
                                    id: o.get("optionId")?.as_str()?.to_string(),
                                    name: o.get("name")?.as_str()?.to_string(),
                                    allow: kind.starts_with("allow"),
                                    kind,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let diffs = params.get("toolCall").map(Edit::all_from_tool_call).unwrap_or_default();
                let diffs = self.diff_views(diffs, window, cx);
                let decision = crate::permissions::decide(&self.permissions.get(), params.get("toolCall").unwrap_or(&Value::Null), &self.roots(cx));
                let auto = match decision {
                    crate::permissions::Decision::Allow(reason) => params.get("options").and_then(crate::permissions::allow_option).map(|id| (id, reason)),
                    crate::permissions::Decision::Ask => None,
                };
                let tool_call_id = params.pointer("/toolCall/toolCallId").and_then(Value::as_str).map(str::to_string);
                // A `git commit` or `git push`: offer to make it from Forge instead (the commit
                // first when the command does both).
                let command = params.get("toolCall").and_then(crate::permissions::tool_command).unwrap_or_default();
                let redirect = match crate::permissions::git_commit(&command) {
                    Some(message) => Some((GitRedirect::Commit(message), COMMIT_IN_FORGE, "Commit from Forge instead")),
                    None if crate::permissions::git_push(&command) => Some((GitRedirect::Push, PUSH_IN_FORGE, "Push from Forge instead")),
                    None => None,
                };
                if let Some((redirect, id, name)) = redirect {
                    options.insert(0, PermissionOption { id: id.into(), name: name.into(), allow: false, kind: id.into() });
                    self.git_redirects.insert(request_id.clone(), redirect);
                }
                self.entries.push(Entry::Permission { request_id: request_id.clone(), tool_call_id, title, options, resolved: None, diffs });
                // The policy allows it: answer as the user would, and say so in the thread.
                if let Some((option_id, reason)) = auto {
                    self.answer_permission(request_id, Some((option_id, format!("Allowed automatically: {reason}"))), cx);
                }
            }
            AgentEvent::Notification { agent_id, method, params } if ours(&agent_id) && method == "_auth/status_update" => {
                // Claude's adapter reports its login state; tell the user before a prompt fails.
                if params.pointer("/authStatus/kind").and_then(Value::as_str) == Some("none") && !self.warned_auth {
                    self.warned_auth = true;
                    self.show_auth("The agent is not signed in.", cx);
                }
            }
            AgentEvent::Notification { agent_id, method, params }
                if ours(&agent_id) && method == "session/update" && params.get("sessionId").and_then(Value::as_str).is_some_and(|s| self.side_sessions.contains_key(s)) =>
            {
                if params.pointer("/update/sessionUpdate").and_then(Value::as_str) == Some("agent_message_chunk") {
                    let session = params.get("sessionId").and_then(Value::as_str).unwrap_or_default();
                    let text = params.pointer("/update/content/text").and_then(Value::as_str).unwrap_or_default();
                    if let Some(reply) = self.side_sessions.get_mut(session) {
                        reply.push_str(text);
                    }
                }
                return;
            }
            AgentEvent::Notification { agent_id, method, params } if ours(&agent_id) && method == "session/update" && self.usage_session.is_some() && params.get("sessionId").and_then(Value::as_str) == self.usage_session.as_deref() => {
                // The hidden `/usage` session: collect its answer, show nothing.
                if params.pointer("/update/sessionUpdate").and_then(Value::as_str) == Some("agent_message_chunk") {
                    let text = params.pointer("/update/content/text").and_then(Value::as_str).unwrap_or_default();
                    self.usage_reply.get_or_insert_default().push_str(text);
                }
                return;
            }
            AgentEvent::Notification { agent_id, method, params } if ours(&agent_id) && method == "session/update" => {
                let session_matches = self.connected.as_ref().is_none_or(|(_, s)| params.get("sessionId").and_then(Value::as_str) == Some(s.as_str()));
                if session_matches && !self.replaying {
                    self.apply_update(params.get("update").cloned().unwrap_or_default(), window, cx);
                }
            }
            _ => return,
        }
        self.changed(cx);
    }

    pub(crate) fn apply_update(&mut self, u: Value, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(mut location) = tool_location(&u) {
            // Updates often leave the kind out: it is the tool call's.
            if location.kind.is_none() {
                let id = u.get("toolCallId").and_then(Value::as_str).unwrap_or_default();
                location.kind = self.entries.iter().find_map(|e| match e {
                    Entry::Tool { id: tid, kind, .. } if tid == id && !kind.is_empty() => Some(kind.clone()),
                    _ => None,
                });
            }
            self.location = Some(location);
        }
        let text = u.pointer("/content/text").and_then(Value::as_str).unwrap_or_default().to_string();
        let str_of = |k: &str| u.get(k).and_then(Value::as_str).map(str::to_string);
        match u.get("sessionUpdate").and_then(Value::as_str).unwrap_or_default() {
            "user_message_chunk" => match self.entries.last_mut() {
                Some(Entry::User(t, _)) => t.push_str(&text),
                _ => self.entries.push(Entry::User(text, vec![])),
            },
            kind @ ("agent_message_chunk" | "agent_thought_chunk") => {
                let thought = kind == "agent_thought_chunk";
                match self.entries.last() {
                    Some(Entry::Agent(md)) if !thought => md.update(cx, |m, cx| m.append(&text, cx)),
                    Some(Entry::Thought(md)) if thought => md.update(cx, |m, cx| m.append(&text, cx)),
                    _ => {
                        let languages = self.languages.clone();
                        let md = cx.new(|cx| Markdown::new(text.into(), Some(languages), None, cx));
                        self.entries.push(if thought { Entry::Thought(md) } else { Entry::Agent(md) });
                    }
                }
            }
            "tool_call" => {
                let diffs = self.diff_views(Edit::all_from_tool_call(&u), window, cx);
                // A command the agent runs itself: its output goes to a terminal shown in the
                // tool call, as for the commands it runs in Forge's terminals.
                let display = u.pointer("/_meta/terminal_info/terminal_id").and_then(Value::as_str).map(str::to_string);
                if let Some(id) = &display {
                    self.add_display_terminal(id, window, cx);
                }
                let (title, command) = tool_title(&u, &self.root);
                self.entries.push(Entry::Tool {
                    id: str_of("toolCallId").unwrap_or_default(),
                    title: title.unwrap_or_else(|| "Tool call".into()),
                    command,
                    kind: str_of("kind").unwrap_or_default(),
                    status: str_of("status").unwrap_or_else(|| "pending".into()),
                    detail: tool_detail(&u),
                    terminal: tool_terminal(&u).or(display),
                    diffs,
                })
            }
            "tool_call_update" => {
                let id = str_of("toolCallId").unwrap_or_default();
                if let Some(output) = u.pointer("/_meta/terminal_output") {
                    let terminal = output.get("terminal_id").and_then(Value::as_str).and_then(|t| self.display_terminals.get(t));
                    if let (Some(terminal), Some(data)) = (terminal, output.get("data").and_then(Value::as_str)) {
                        terminal.update(cx, |t, cx| t.write_output(data.as_bytes(), cx));
                    }
                }
                let edits = Edit::all_from_tool_call(&u);
                let mut new_diffs = (!edits.is_empty()).then(|| self.diff_views(edits, window, cx));
                let (new_title, new_command) = tool_title(&u, &self.root);
                let completed = str_of("status").as_deref() == Some("completed");
                for entry in &mut self.entries {
                    if let Entry::Tool { id: tid, title, status, detail, terminal, diffs, command, .. } = entry {
                        if *tid == id {
                            if new_command.is_some() {
                                *command = new_command.clone();
                            }
                            if let Some(t) = tool_terminal(&u) {
                                *terminal = Some(t);
                            }
                            if let Some(d) = new_diffs.take() {
                                *diffs = d;
                            }
                            if let Some(s) = str_of("status") {
                                *status = s;
                            }
                            if let Some(t) = new_title.clone() {
                                *title = t;
                            }
                            if let Some(d) = tool_detail(&u) {
                                *detail = Some(d);
                            }
                        }
                    }
                }
                // Agents that write files themselves (Claude Code's adapter does) bypass
                // `ProjectFs`: their finished edits join the changes here, from the file
                // before (the tool call's diff) and as it is now on disk.
                if completed {
                    self.record_tool_edits(&id, cx);
                }
            }
            "usage_update" => self.usage.apply_update(&u),
            "current_mode_update" => {
                self.current_mode = str_of("currentModeId");
                // Keep the mode setting (if the agent also lists it there) in step.
                if let Some(mode) = self.current_mode.clone() {
                    for option in self.config_options.iter_mut().filter(|o| o.category.as_deref() == Some("mode")) {
                        if let ConfigValue::Select { current, .. } = &mut option.value {
                            *current = mode.clone();
                        }
                    }
                }
            }
            "config_option_update" => self.config_options = parse_config_options(u.get("configOptions")),
            "available_commands_update" => {
                self.agent_commands = u
                    .get("availableCommands")
                    .and_then(Value::as_array)
                    .map(|xs| {
                        xs.iter()
                            .filter_map(|c| {
                                Some(AgentCommand {
                                    name: c.get("name")?.as_str()?.trim_start_matches('/').to_string(),
                                    description: c.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
                                    hint: c.get("input").and_then(|i| i.get("hint")).and_then(Value::as_str).map(str::to_string),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if self.status == Status::Ready {
                    self.refresh_plan_usage(cx);
                }
            }
            "plan" => {
                let items = u
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|xs| {
                        xs.iter()
                            .map(|e| (e.get("content").and_then(Value::as_str).unwrap_or_default().to_string(), e.get("status").and_then(Value::as_str).unwrap_or("pending").to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                self.entries.retain(|e| !matches!(e, Entry::Plan(_)));
                self.entries.push(Entry::Plan(items));
            }
            _ => {}
        }
    }
}

/// The last of a tool call's `locations`; lines are zero-based, as Zed reads them.
pub(super) fn tool_location(u: &Value) -> Option<AgentLocation> {
    let location = u.get("locations")?.as_array()?.last()?;
    Some(AgentLocation {
        path: PathBuf::from(location.get("path")?.as_str()?),
        line: location.get("line").and_then(Value::as_u64).map(|l| l as u32),
        kind: u.get("kind").and_then(Value::as_str).map(str::to_string),
    })
}

/// A tool call's title, and for a command, its command line when the title is the agent's
/// description of it (Claude Code's `rawInput.description`). Command lines lose the
/// `cd <folder> &&` agents put first, and say `.` for the thread's folder.
pub(super) fn tool_title(u: &Value, root: &std::path::Path) -> (Option<String>, Option<String>) {
    let title = u.get("title").and_then(Value::as_str).map(str::to_string);
    let input = u.get("rawInput");
    // Forge's own tools: their title, and what they were asked.
    let name = u.pointer("/_meta/claudeCode/toolName").or_else(|| u.get("name")).and_then(Value::as_str).or(title.as_deref());
    if let Some(forge_title) = name.and_then(crate::forge_mcp::tool_title).filter(|_| name.is_some_and(|n| n.starts_with("mcp__"))) {
        return (Some(forge_title), input.and_then(crate::forge_mcp::tool_summary));
    }
    let command = input.and_then(|i| i.get("command")).and_then(Value::as_str).map(str::to_string).or_else(|| (u.get("kind").and_then(Value::as_str) == Some("execute")).then(|| title.clone()).flatten());
    let Some(command) = command.filter(|c| !c.trim().is_empty()) else { return (title, None) };
    let command = tidy_command(&command, root);
    match input.and_then(|i| i.get("description")).and_then(Value::as_str).map(str::trim).filter(|d| !d.is_empty()) {
        Some(description) => (Some(description.to_string()), Some(command)),
        None => (Some(command), None),
    }
}

/// `command` as the user reads it: see [`tool_title`].
pub(super) fn tidy_command(command: &str, root: &std::path::Path) -> String {
    let mut rest = command.trim();
    // `cd <dir> && …` / `cd <dir>; …`, maybe more than one.
    while let Some(after_cd) = rest.strip_prefix("cd ") {
        let cut = [after_cd.find("&&").map(|i| (i, 2)), after_cd.find(';').map(|i| (i, 1))].into_iter().flatten().min_by_key(|(i, _)| *i);
        let Some((i, len)) = cut else { break };
        rest = after_cd[i + len..].trim_start();
    }
    let root = root.to_string_lossy();
    if root.len() <= 1 {
        return rest.to_string();
    }
    rest.replace(&format!("{root}/"), "").replace(root.as_ref(), ".")
}

/// The same text but for trailing whitespace (agents may trim it as they write).
pub(super) fn same_text(a: &str, b: &str) -> bool {
    a.lines().map(str::trim_end).eq(b.lines().map(str::trim_end))
}

/// The terminal a tool call embeds (`{"type": "terminal", "terminalId"}` content).
pub(super) fn tool_terminal(u: &Value) -> Option<String> {
    u.get("content")?.as_array()?.iter().find_map(|c| (c.get("type")?.as_str()? == "terminal").then(|| c.get("terminalId")?.as_str().map(str::to_string)).flatten())
}

/// Lines of a tool call's text shown at most (the last ones: a command's ending matters).
pub(super) const DETAIL_LINES: usize = 200;

/// What a tool call's content says: the diff target, or its first text block (a command's
/// output, often in a code fence, shown without it).
pub(super) fn tool_detail(u: &Value) -> Option<String> {
    let content = u.get("content")?.as_array()?;
    content.iter().find_map(|c| match c.get("type").and_then(Value::as_str)? {
        "diff" => Some(format!("edit {}", c.get("path")?.as_str()?)),
        "content" => {
            let text = unfence(c.pointer("/content/text")?.as_str()?);
            let lines: Vec<&str> = text.lines().collect();
            let shown = match lines.len().checked_sub(DETAIL_LINES) {
                Some(cut) if cut > 0 => format!("… {cut} more lines\n{}", lines[cut..].join("\n")),
                _ => text.to_string(),
            };
            (!shown.trim().is_empty()).then_some(shown)
        }
        _ => None,
    })
}

/// `text` without the code fence around it (```` ```console ```` … ```` ``` ````), if it has one.
pub(super) fn unfence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else { return text };
    let Some(body_start) = rest.find('\n') else { return "" };
    let body = &rest[body_start + 1..];
    body.trim_end().strip_suffix("```").unwrap_or(body).trim_end_matches('\n')
}


#[cfg(test)]
mod detail_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn titles_commands_by_their_description() {
        let root = std::path::Path::new("/work/app");
        let call = json!({ "kind": "execute", "title": "cd /work/app && git status", "rawInput": { "command": "cd /work/app && git status", "description": "Show working tree status" } });
        assert_eq!(tool_title(&call, root), (Some("Show working tree status".into()), Some("git status".into())));
        let bare = json!({ "kind": "execute", "title": "cd \"/work/app\"; ls /work/app/src /tmp" });
        assert_eq!(tool_title(&bare, root), (Some("ls src /tmp".into()), None));
        let read = json!({ "kind": "read", "title": "Read main.rs" });
        assert_eq!(tool_title(&read, root), (Some("Read main.rs".into()), None));
        let forge = json!({ "kind": "other", "title": "mcp__forge__find_references", "rawInput": { "path": "src/a.rs", "line": 12, "symbol": "parse" } });
        assert_eq!(tool_title(&forge, root), (Some("Find references".into()), Some("src/a.rs:12 parse".into())));
        assert_eq!(tidy_command("cd sub", root), "cd sub", "a lone cd stays");
        assert_eq!(tidy_command("cd /work/app && cd src && cargo test", root), "cargo test");
    }

    fn text(t: &str) -> Value {
        json!({ "content": [{ "type": "content", "content": { "type": "text", "text": t } }] })
    }

    #[test]
    fn shows_command_output_without_its_fence() {
        assert_eq!(tool_detail(&text("```console\nline 1\nline 2\n```")).as_deref(), Some("line 1\nline 2"));
        assert_eq!(tool_detail(&text("```\nok\n```\n")).as_deref(), Some("ok"));
        assert_eq!(tool_detail(&text("plain\ntext")).as_deref(), Some("plain\ntext"));
        assert_eq!(tool_detail(&text("```console\n```")), None, "nothing to show");
        let long: String = (1..=250).map(|i| format!("{i}\n")).collect();
        let shown = tool_detail(&text(&format!("```console\n{long}```"))).unwrap();
        assert!(shown.starts_with("… 50 more lines\n51\n") && shown.ends_with("\n250"), "{shown}");
        assert_eq!(tool_detail(&json!({ "content": [{ "type": "diff", "path": "/a.rs" }] })).as_deref(), Some("edit /a.rs"));
    }
}
