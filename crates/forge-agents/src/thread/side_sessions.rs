//! Hidden sessions with the connected agent: one-off questions (`ask_aside`) and its
//! account's usage limits (`/usage`).

use super::*;

impl Thread {
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
