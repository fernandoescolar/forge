//! The conversation on disk: saving it after each turn, reopening a saved one and
//! deleting them.

use super::*;

impl Thread {
    /// Saves the conversation (in the background) if there is one worth keeping.
    pub(crate) fn persist(&self, cx: &mut Context<Self>) {
        let Some((agent_id, session_id)) = self.connected.clone() else { return };
        let entries: Vec<RecordEntry> = self
            .entries
            .iter()
            .filter_map(|e| {
                Some(match e {
                    Entry::User(text, context) => RecordEntry::User { text: text.clone(), context: context.clone() },
                    Entry::Agent(md) => RecordEntry::Agent { text: md.read(cx).source().to_string() },
                    Entry::Thought(md) => RecordEntry::Thought { text: md.read(cx).source().to_string() },
                    Entry::Tool { title, kind, status, .. } => RecordEntry::Tool { title: title.clone(), kind: kind.clone(), status: status.clone() },
                    Entry::Plan(items) => RecordEntry::Plan { items: items.clone() },
                    Entry::Permission { title, resolved, .. } => RecordEntry::System { text: format!("Permission: {title} → {}", resolved.clone().unwrap_or_else(|| "pending".into())) },
                    Entry::Review { diff, outcome, .. } => RecordEntry::System { text: format!("Edit {}: {}", diff.edit.path, outcome.unwrap_or("pending")) },
                    Entry::Commit { message, state, .. } => RecordEntry::System { text: format!("Commit \"{}\": {}", message.lines().next().unwrap_or_default(), crate::commit_proposal::outcome(state)) },
                    Entry::Push { plan, state, .. } => RecordEntry::System { text: format!("Push of {} to {}: {}", plan.branch, plan.target(), crate::push_proposal::outcome(plan, state)) },
                    Entry::Question { question, answer, .. } => RecordEntry::System { text: format!("Asked: {question} → {}", answer.as_deref().map(|a| if a.is_empty() { "skipped" } else { a }).unwrap_or("not answered")) },
                    Entry::Remember { note, state, .. } => RecordEntry::System { text: format!("Note \"{}\": {}", note.read(cx).text(cx).trim(), state.outcome()) },
                    Entry::ExtensionTool { tool, state, .. } => RecordEntry::System { text: format!("{} ({}): {}", tool.title, tool.extension, state.outcome()) },
                    Entry::ForgeTool { name, state, .. } => RecordEntry::System { text: format!("{}: {}", forge_tool_title(name), state.outcome()) },
                    Entry::LspEdit { plan, state, .. } => RecordEntry::System { text: format!("Asked to {}: {}", plan.summary(), state.outcome()) },
                    Entry::System(..) | Entry::Auth { .. } | Entry::Check(_) => return None,
                })
            })
            .collect();
        if !entries.iter().any(|e| matches!(e, RecordEntry::User { .. })) {
            return;
        }
        let record = SessionRecord {
            session_id,
            agent_id,
            agent_name: self.agent_name.clone(),
            project_root: self.root.clone(),
            title: history::title_for(&entries),
            started_at: self.started_at,
            updated_at: history::now(),
            entries,
        };
        let dir = self.history_dir.clone();
        cx.background_spawn(async move {
            if let Err(e) = history::save(&dir, &record) {
                log::error!("failed to save agent session: {e:#}");
            }
        })
        .detach();
    }

    /// Saved conversations of this project, newest first.
    pub fn history(&self) -> Vec<SessionSummary> {
        history::list(&self.history_dir, &self.root)
    }

    /// Shows a saved conversation; connecting resumes it.
    pub fn open_session(&mut self, summary: &SessionSummary, cx: &mut Context<Self>) {
        let record = match history::load(&summary.file) {
            Ok(r) => r,
            Err(e) => {
                self.system(format!("{e:#}"), Color::Error);
                self.changed(cx);
                return;
            }
        };
        if self.status != Status::Disconnected {
            self.disconnect(cx);
        }
        if let Some(i) = self.agents.iter().position(|a| a.id == record.agent_id) {
            self.selected = i;
        }
        self.entries = record
            .entries
            .iter()
            .map(|e| match e {
                RecordEntry::User { text, context } => Entry::User(text.clone(), context.clone()),
                RecordEntry::Agent { text } => Entry::Agent(cx.new(|cx| Markdown::new(text.clone().into(), Some(self.languages.clone()), None, cx))),
                RecordEntry::Thought { text } => Entry::Thought(cx.new(|cx| Markdown::new(text.clone().into(), Some(self.languages.clone()), None, cx))),
                RecordEntry::Tool { title, kind, status } => {
                    Entry::Tool { id: String::new(), title: title.clone(), kind: kind.clone(), status: status.clone(), detail: None, terminal: None, diffs: vec![], command: None }
                }
                RecordEntry::Plan { items } => Entry::Plan(items.clone()),
                RecordEntry::System { text } => Entry::System(text.clone(), Color::Muted),
            })
            .collect();
        self.system(format!("Saved conversation from {}. Connect to continue it.", history::relative_time(record.updated_at, history::now())), Color::Muted);
        self.started_at = record.started_at;
        self.resume = Some(record);
        self.changed(cx);
    }

    /// Stops the agent and deletes the saved conversation for good, unlike `disconnect`
    /// (which saves it).
    pub fn delete_conversation(&mut self, cx: &mut Context<Self>) {
        let file = self.session_id().map(|id| history::session_file(&self.history_dir, &self.root, id));
        if let Some((id, _)) = self.connected.take() {
            let rt = self.runtime.clone();
            Tokio::spawn(cx, async move { rt.stop(&id).await }).detach();
        }
        self.resume = None;
        self.status = Status::Disconnected;
        if let Some(file) = file.filter(|f| f.exists()) {
            if let Err(e) = history::delete(&file) {
                log::error!("{e:#}");
            }
        }
        self.changed(cx);
    }

    pub fn delete_session(&mut self, summary: &SessionSummary, cx: &mut Context<Self>) {
        if let Err(e) = history::delete(&summary.file) {
            log::error!("{e:#}");
        }
        self.changed(cx);
    }
}
