//! The agent's process and session: connecting (starting the agent, initializing,
//! loading or starting the session), sending prompts and queued messages, cancelling.

use super::*;

impl Thread {
    pub fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ended {
            self.system("This thread's worktree was removed; start a new thread.", Color::Warning);
            return self.changed(cx);
        }
        let Some(mut spec) = self.agents.get(self.selected).cloned() else { return };
        crate::config::allow_npx_install_scripts(&mut spec);
        self.persist(cx);
        self.warned_auth = false;
        self.auth_pending = None;
        self.status = Status::Connecting;
        let resume = self.resume.take();
        self.replaying = resume.is_some();
        let restart = std::mem::take(&mut self.restart_keeping_transcript);
        if resume.is_none() && !restart {
            self.entries.clear();
            self.checkpoints.clear();
            self.started_at = history::now();
        }
        self.stderr.clear();
        self.system(format!("Starting {} ({} {})…", spec.id, spec.command, spec.args.join(" ")), Color::Muted);
        // Agents get the login-shell environment of the project root (PATH, nvm, asdf, …),
        // like Zed's terminals. Without it, a Forge launched from Finder can't find `npx`.
        let shell_env = self.project.upgrade().map(|project| {
            let root: Arc<std::path::Path> = self.root.clone().into();
            project.update(cx, |p, cx| p.environment().update(cx, |env, cx| env.directory_environment(root, cx)))
        });
        let (rt, root, id) = (self.runtime.clone(), self.root.clone(), spec.id.clone());
        let resume_id = resume.as_ref().map(|r| r.session_id.clone());
        let mut mcp = self.mcp_servers.clone();
        let forge_server = self.forge_mcp.as_ref().map(|r| r.acp_server());
        let spec_for_auth = spec.clone();
        // The agent's `initialize` answer, kept even when `session/new` fails: its login
        // methods are what that failure (not signed in) needs.
        let initialized: Arc<std::sync::Mutex<Option<Value>>> = Arc::default();
        let initialized_slot = initialized.clone();
        cx.spawn_in(window, async move |this, cx| {
            let mut env: Vec<(String, String)> = match shell_env {
                Some(task) => task.await.unwrap_or_default().into_iter().collect(),
                None => vec![],
            };
            // Entries from agents.json win over the shell environment.
            env.append(&mut spec.env);
            spec.env = env;
            let task = cx.update(|_, cx| {
                let id = id.clone();
                Tokio::spawn_result(cx, async move {
                    if restart && rt.running().contains(&spec.id) {
                        // Pick up fresh credentials: agents read them at startup.
                        rt.stop(&spec.id).await?;
                        for _ in 0..50 {
                            if !rt.running().contains(&spec.id) {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }
                    if !rt.running().contains(&spec.id) {
                        rt.start(spec).await?;
                    }
                    let init = rt.initialize(&id, PROTOCOL_VERSION).await?;
                    *initialized_slot.lock().unwrap() = Some(init.clone());
                    // Forge's own tools, for agents that take HTTP MCP servers.
                    if let (Some(server), true) = (forge_server, init.pointer("/agentCapabilities/mcpCapabilities/http").and_then(Value::as_bool).unwrap_or(false)) {
                        mcp.push(server);
                    }
                    let can_load = init.pointer("/agentCapabilities/loadSession").and_then(Value::as_bool).unwrap_or(false);
                    if let (Some(sid), true) = (resume_id, can_load) {
                        // Agents may not know the session anymore (e.g. it was restarted); fall back.
                        if let Ok(loaded) = rt.load_session(&id, &sid, &root, &mcp).await {
                            return anyhow::Ok((init, json!({ "sessionId": sid, "resumed": true, "modes": loaded.get("modes").cloned().unwrap_or_default(), "configOptions": loaded.get("configOptions").cloned().unwrap_or_default() })));
                        }
                    }
                    let session = rt.new_session(&id, &root, &mcp).await?;
                    anyhow::Ok((init, session))
                })
            });
            let result = match task {
                Ok(task) => task.await,
                Err(e) => Err(e),
            };
            this.update_in(cx, |this, _window, cx| {
                this.replaying = false;
                match result.and_then(|(init, session)| {
                    let sid = session.get("sessionId").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("agent returned no sessionId: {session}"))?;
                    let resumed = session.get("resumed").and_then(Value::as_bool).unwrap_or(false);
                    Ok((init, sid.to_string(), resumed, session.get("modes").cloned().unwrap_or_default(), parse_config_options(session.get("configOptions"))))
                }) {
                    Ok((init, sid, resumed, modes, config_options)) => {
                        this.read_modes(&modes);
                        this.config_options = config_options;
                        this.auth_methods = parse_auth_methods(&init, &spec_for_auth);
                        this.agent_name = init.pointer("/agentInfo/name").and_then(Value::as_str).map(str::to_string);
                        this.embedded_context = init.pointer("/agentCapabilities/promptCapabilities/embeddedContext").and_then(Value::as_bool).unwrap_or(false);
                        this.accepts_images = init.pointer("/agentCapabilities/promptCapabilities/image").and_then(Value::as_bool).unwrap_or(false);
                        this.forge_tools = this.forge_mcp.is_some() && init.pointer("/agentCapabilities/mcpCapabilities/http").and_then(Value::as_bool).unwrap_or(false);
                        this.connected = Some((id.clone(), sid));
                        // Needs the connection: it changes the session's mode.
                        this.apply_policy_mode(cx);
                        // A resumed session got them with its first message.
                        this.rules_sent = resumed;
                        this.status = Status::Ready;
                        let name = this.agent_name.clone().unwrap_or(id);
                        match (resumed, this.entries.iter().any(|e| matches!(e, Entry::User(..)))) {
                            (true, _) => this.system(format!("Resumed the session with {name}."), Color::Success),
                            (false, true) => this.system(format!("{name} can't resume that session; started a new one (the conversation above is for reference only)."), Color::Warning),
                            (false, false) => this.system(format!("Connected to {name}."), Color::Success),
                        }
                        if let Some(reason) = this.auth_pending.take() {
                            this.show_auth(&reason, cx);
                        }
                        if let Some(text) = this.retry_prompt.take() {
                            cx.emit(ThreadEvent::RestoreInput(text));
                            this.system("Your last message is back in the input; press Enter to send it again.", Color::Muted);
                        }
                        if let Some(text) = this.queued_prompt.take() {
                            this.send(text, None, cx);
                        }
                    }
                    Err(e) => {
                        this.status = Status::Disconnected;
                        this.system(format!("Could not connect: {e:#}"), Color::Error);
                        if let Some(init) = initialized.lock().unwrap().take() {
                            this.auth_methods = parse_auth_methods(&init, &spec_for_auth);
                        }
                        if is_auth_error(&format!("{e:#}")) {
                            this.show_auth("Signing in should fix this.", cx);
                        }
                        for line in this.stderr.iter().rev().take(5).collect::<Vec<_>>().into_iter().rev() {
                            this.entries.push(Entry::System(line.clone(), Color::Muted));
                        }
                    }
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
        self.changed(cx);
    }

    pub fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.persist(cx);
        if let Some((id, _)) = self.connected.take() {
            let rt = self.runtime.clone();
            Tokio::spawn(cx, async move { rt.stop(&id).await }).detach();
        }
        self.status = Status::Disconnected;
        self.changed(cx);
    }

    /// Sends `text` (with `active`, the editor context the view chose to attach). Returns
    /// whether it was sent.
    pub fn send(&mut self, text: String, active: Option<ActiveContext>, cx: &mut Context<Self>) -> bool {
        self.send_with_images(text, active, vec![], cx)
    }

    /// Whether the agent takes images in prompts (ACP `promptCapabilities.image`).
    pub fn accepts_images(&self) -> bool {
        self.accepts_images
    }

    /// [`Thread::send`] with images (pasted or dropped in the composer).
    pub fn send_with_images(&mut self, text: String, active: Option<ActiveContext>, images: Vec<Arc<gpui::Image>>, cx: &mut Context<Self>) -> bool {
        let text = text.trim().to_string();
        if text.is_empty() || self.status != Status::Ready {
            return false;
        }
        let Some((agent, session)) = self.connected.clone() else { return false };
        // `/name …` for one of the user's prompts sends the prompt; the thread shows what was typed.
        let prompt = crate::library::expand_prompt(&text, &self.root);
        let sent = prompt.as_ref().map(|(_, expanded)| expanded.clone()).unwrap_or_else(|| text.clone());
        let mut files: Vec<(PathBuf, Option<u32>)> = Vec::new();
        let mut specials: Vec<String> = Vec::new();
        for mention in resolve_mentions(&sent, &self.root) {
            match mention {
                Mention::Path { path, line } => files.push((path, line)),
                Mention::Special(name) => specials.push(name),
            }
        }
        let mut labels: Vec<String> = files
            .iter()
            .map(|(p, line)| {
                let rel = p.strip_prefix(&self.root).unwrap_or(p).to_string_lossy().into_owned();
                match line {
                    Some(line) => format!("{rel}:{line}"),
                    None => rel,
                }
            })
            .collect();
        labels.extend(specials.iter().map(|s| format!("@{s}")));
        if let Some((prompt, _)) = &prompt {
            labels.insert(0, format!("prompt {}", prompt.path.strip_prefix(&self.root).unwrap_or(&prompt.path).display()));
        }
        let images = if images.is_empty() || self.accepts_images {
            images
        } else {
            self.system(format!("{} doesn't take images; sent without them.", self.agent_label()), Color::Warning);
            vec![]
        };
        labels.extend((1..=images.len()).map(|i| format!("image {i}")));
        if let Some(a) = &active {
            labels.push(a.label(&self.root));
        }
        let mut blocks = prompt_blocks(&sent, &files, active.as_ref(), self.embedded_context, &self.root);
        for image in &images {
            use base64::Engine as _;
            let data = base64::engine::general_purpose::STANDARD.encode(&image.bytes);
            blocks.push(serde_json::json!({ "type": "image", "mimeType": image.format.mime_type(), "data": data }));
        }
        let extras = crate::context::gather(&specials, self.workspace.clone(), self.project.clone(), self.root.clone(), cx);
        let rules = (!self.rules_sent).then(|| crate::rules::load(<dyn fs::Fs>::global(cx), self.root.clone(), crate::rules::user_file(), self.instructions_files()));
        if !self.rules_sent && self.forge_tools {
            blocks.push(json!({ "type": "text", "text": crate::forge_mcp::instructions(&self.root) }));
        }
        self.rules_sent = true;
        let retry_text = text.clone();
        self.entries.push(Entry::User(text, labels));
        self.begin_checkpoint();
        if self.verify_enabled(cx) {
            self.diagnostics_before = self.project.upgrade().map(|p| crate::verify::diagnostic_counts(&p, cx)).unwrap_or_default();
        }
        self.status = Status::Busy;
        let rt = self.runtime.clone();
        cx.spawn(async move |this, cx| {
            blocks.extend(extras.await);
            if let Some(rules) = match rules {
                Some(rules) => rules.await,
                None => None,
            } {
                blocks.push(rules.block());
                this.update(cx, |this, cx| {
                    this.system(format!("Sent your instructions from {}.", rules.label()), Color::Muted);
                    this.changed(cx);
                })
                .ok();
            }
            let result = Tokio::spawn_result(cx, async move { Ok(rt.prompt_content(&agent, &session, blocks).await?) }).await;
            this.update(cx, |this, cx| {
                if this.status == Status::Busy {
                    this.status = Status::Ready;
                }
                if let Some(turn) = this.checkpoints.last_mut() {
                    turn.duration.get_or_insert_with(|| turn.started.elapsed());
                }
                if let Some(usage) = result.as_ref().ok().and_then(|r| r.get("usage")) {
                    this.usage.add_turn(usage);
                }
                this.refresh_plan_usage(cx);
                match result {
                    Ok(r) => match r.get("stopReason").and_then(Value::as_str) {
                        Some("end_turn") | None => {}
                        Some("cancelled") => this.system("Cancelled.", Color::Muted),
                        Some(other) => this.system(format!("Turn ended: {other}"), Color::Warning),
                    },
                    Err(e) => {
                        let msg = format!("{e:#}");
                        if is_auth_error(&msg) {
                            this.retry_prompt = Some(retry_text);
                            this.show_auth("The agent needs you to sign in before it can answer.", cx);
                        } else {
                            this.system(format!("Prompt failed: {msg}"), Color::Error);
                        }
                    }
                }
                this.persist(cx);
                this.check_changed_files(cx);
                this.send_next_queued(cx);
                this.changed(cx);
            })
            .ok();
        })
        .detach();
        self.changed(cx);
        true
    }

    /// Adds a message for after the current turn.
    pub(crate) fn enqueue(&mut self, text: String, active: Option<ActiveContext>, images: Vec<Arc<gpui::Image>>, cx: &mut Context<Self>) {
        self.queue.push(Queued { text, active, images });
        self.changed(cx);
    }

    pub(crate) fn unqueue(&mut self, ix: usize, cx: &mut Context<Self>) -> Option<String> {
        let removed = (ix < self.queue.len()).then(|| self.queue.remove(ix).text);
        self.changed(cx);
        removed
    }

    /// Sends a queued message now: it goes first and the current turn stops.
    pub(crate) fn send_queued_now(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.queue.len() {
            let item = self.queue.remove(ix);
            self.queue.insert(0, item);
        }
        if self.status == Status::Busy {
            self.cancel(cx);
        } else {
            self.send_next_queued(cx);
        }
        self.changed(cx);
    }

    pub(super) fn send_next_queued(&mut self, cx: &mut Context<Self>) {
        if self.status != Status::Ready || self.queue.is_empty() || self.auth_pending.is_some() {
            return;
        }
        let Queued { text, active, images } = self.queue.remove(0);
        self.send_with_images(text, active, images, cx);
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.status != Status::Busy {
            return;
        }
        if let Some((agent, session)) = self.connected.clone() {
            let rt = self.runtime.clone();
            Tokio::spawn(cx, async move { rt.cancel(&agent, &session).await }).detach();
        }
    }
}
