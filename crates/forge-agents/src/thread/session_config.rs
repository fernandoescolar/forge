//! The agent's session modes and settings (ACP `modes` and `configOptions`).

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
}
