//! The agent's session settings: its modes (and the one Forge's permissions pick), its
//! config options, side questions and plan usage.

use super::*;

impl Thread {
    /// ACP `modes` from `session/new`: `{currentModeId, availableModes: [{id, name, description}]}`.
    pub(super) fn read_modes(&mut self, modes: &Value) {
        self.modes = modes
            .get("availableModes")
            .and_then(Value::as_array)
            .map(|xs| {
                xs.iter()
                    .filter_map(|m| {
                        Some(SessionMode {
                            id: m.get("id")?.as_str()?.to_string(),
                            name: m.get("name")?.as_str()?.to_string(),
                            description: m.get("description").and_then(Value::as_str).map(str::to_string),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.current_mode = modes.get("currentModeId").and_then(Value::as_str).map(str::to_string);
    }

    /// Starts the agent in the mode that matches Forge's permissions, when it has one:
    /// fewer questions at the source (Claude stops asking about edits in Accept edits).
    pub(super) fn apply_policy_mode(&mut self, cx: &mut Context<Self>) {
        use crate::permissions::PermissionMode;
        // The agent's own session modes that match the policy, best first (Claude Code:
        // `auto`, `acceptEdits`, `bypassPermissions`; Codex: `auto`; Gemini: `autoEdit`).
        let wanted: &[&str] = match self.permissions.get().mode {
            PermissionMode::Ask => &[],
            PermissionMode::AllowEdits | PermissionMode::AllowWorkspace => &["acceptEdits", "autoEdit"],
            PermissionMode::Auto => &["auto", "acceptEdits", "autoEdit"],
            PermissionMode::SuperUser => &["bypassPermissions", "acceptEdits", "autoEdit"],
        };
        if let Some(mode) = wanted.iter().find(|id| self.modes.iter().any(|m| m.id == **id)) {
            if self.current_mode.as_deref() != Some(mode) {
                self.set_mode(mode.to_string(), cx);
            }
        }
    }

    /// Changes one of the agent's session settings (`session/set_config_option`); the
    /// agent answers with all of them (a new model can change the efforts on offer).
    pub(crate) fn set_config_option(&mut self, id: String, value: Value, cx: &mut Context<Self>) {
        let Some((agent, session)) = self.connected.clone() else { return };
        // Show the choice right away; the answer replaces it.
        if let Some(option) = self.config_options.iter_mut().find(|o| o.id == id) {
            match (&mut option.value, &value) {
                (ConfigValue::Select { current, .. }, Value::String(v)) => *current = v.clone(),
                (ConfigValue::Boolean(on), Value::Bool(v)) => *on = *v,
                _ => {}
            }
            if option.category.as_deref() == Some("mode") {
                self.current_mode = value.as_str().map(str::to_string);
            }
        }
        let rt = self.runtime.clone();
        let task = Tokio::spawn_result(cx, async move {
            Ok(rt.rpc(&agent, "session/set_config_option", json!({"sessionId": session, "configId": id, "value": value})).await?)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(answer) => {
                        let options = parse_config_options(answer.get("configOptions"));
                        if !options.is_empty() {
                            this.config_options = options;
                        }
                    }
                    Err(e) => this.system(format!("Could not change that setting: {e:#}"), Color::Warning),
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
        self.changed(cx);
    }

    /// Switches the agent's session mode (`session/set_mode`).
    pub(crate) fn set_mode(&mut self, mode_id: String, cx: &mut Context<Self>) {
        let Some((agent, session)) = self.connected.clone() else { return };
        self.current_mode = Some(mode_id.clone());
        let rt = self.runtime.clone();
        let task = Tokio::spawn_result(cx, async move { Ok(rt.rpc(&agent, "session/set_mode", json!({"sessionId": session, "modeId": mode_id})).await?) });
        cx.spawn(async move |this, cx| {
            if let Err(e) = task.await {
                this.update(cx, |this, cx| {
                    this.system(format!("Could not change the mode: {e:#}"), Color::Warning);
                    this.changed(cx);
                })
                .ok();
            }
        })
        .detach();
        self.changed(cx);
    }

    /// Asks the connected agent `prompt` in a hidden session of its own and returns its
    /// answer (commit messages and other one-off questions; the thread shows nothing).
    pub fn ask_aside(&mut self, prompt: String, cx: &mut Context<Self>) -> gpui::Task<anyhow::Result<String>> {
        let Some((agent, _)) = self.connected.clone() else { return gpui::Task::ready(Err(anyhow::anyhow!("the agent is not connected"))) };
        let (rt, root) = (self.runtime.clone(), self.root.clone());
        let agent_for_session = agent.clone();
        let rt_for_session = rt.clone();
        let session = Tokio::spawn_result(cx, async move {
            rt_for_session
                .new_session(&agent_for_session, &root, &[])
                .await?
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("no sessionId"))
        });
        cx.spawn(async move |this, cx| {
            let session = session.await?;
            this.update(cx, |this, _| this.side_sessions.insert(session.clone(), String::new()))?;
            let prompt_session = session.clone();
            let result = Tokio::spawn_result(cx, async move { Ok(rt.prompt_content(&agent, &prompt_session, vec![json!({ "type": "text", "text": prompt })]).await?) }).await;
            let reply = this.update(cx, |this, _| this.side_sessions.remove(&session).unwrap_or_default())?;
            result?;
            Ok(reply)
        })
    }

    /// Asks the agent for its account's usage limits, when it offers `/usage` (Claude),
    /// in a hidden session; at most once a minute per agent.
    pub(crate) fn refresh_plan_usage(&mut self, cx: &mut Context<Self>) {
        let Some((agent, _)) = self.connected.clone() else { return };
        if !self.agent_commands.iter().any(|c| c.name == "usage") || self.usage_reply.is_some() || !crate::plan_usage::should_ask(&agent, cx) {
            return;
        }
        self.usage_reply = Some(String::new());
        let (rt, root, existing) = (self.runtime.clone(), self.root.clone(), self.usage_session.clone());
        let agent_for_task = agent.clone();
        let task = Tokio::spawn_result(cx, async move {
            let session = match existing {
                Some(session) => session,
                None => rt
                    .new_session(&agent_for_task, &root, &[])
                    .await?
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("no sessionId"))?,
            };
            anyhow::Ok(session)
        });
        cx.spawn(async move |this, cx| {
            let session = task.await;
            let prompt = this.update(cx, |this, cx| match session {
                Ok(session) => {
                    this.usage_session = Some(session.clone());
                    let rt = this.runtime.clone();
                    let agent = agent.clone();
                    Some(Tokio::spawn_result(cx, async move { Ok(rt.prompt_content(&agent, &session, vec![json!({"type": "text", "text": "/usage"})]).await?) }))
                }
                Err(e) => {
                    log::info!("could not open a session for /usage: {e:#}");
                    this.usage_reply = None;
                    None
                }
            });
            let Ok(Some(prompt)) = prompt else { return };
            let result = prompt.await;
            this.update(cx, |this, cx| {
                let reply = this.usage_reply.take().unwrap_or_default();
                match result {
                    Ok(_) => match crate::plan_usage::parse(&reply) {
                        Some(usage) => crate::plan_usage::set(&agent, usage, cx),
                        None => log::info!("/usage answered without limits"),
                    },
                    Err(e) => log::info!("/usage failed: {e:#}"),
                }
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// The account usage of this thread's agent, if known.
    pub fn plan_usage(&self, cx: &gpui::App) -> Option<crate::plan_usage::PlanUsage> {
        let agent = self.connected.as_ref().map(|(a, _)| a.clone()).or_else(|| self.agents.get(self.selected).map(|a| a.id.clone()))?;
        crate::plan_usage::get(&agent, cx)
    }
}
