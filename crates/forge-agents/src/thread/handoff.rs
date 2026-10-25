//! Handing a thread to another agent, with a summary of what happened so far.

use super::*;

impl Thread {
    /// Hands the thread to agent `ix`: the conversation stays on screen, and the new agent
    /// starts a session of its own with a summary of what was asked and done so far (and the
    /// standing instructions again), then carries on.
    pub fn hand_off(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.agents.len() || ix == self.selected {
            return;
        }
        if matches!(self.status, Status::Busy | Status::Connecting) {
            self.system("Stop the agent before handing the thread to another one.", Color::Warning);
            return self.changed(cx);
        }
        let from = self.agent_label();
        let to = self.agents[ix].id.clone();
        let requests: Vec<String> = self.entries.iter().filter_map(|e| if let Entry::User(text, _) = e { Some(text.clone()) } else { None }).collect();
        let last_answer = self.entries.iter().rev().find_map(|e| if let Entry::Agent(md) = e { Some(md.read(cx).source().to_string()) } else { None });
        let plan = self.entries.iter().rev().find_map(|e| if let Entry::Plan(items) = e { Some(items.clone()) } else { None }).unwrap_or_default();
        let changed: Vec<String> = self.changes.iter().map(|c| c.path.strip_prefix(&self.root).unwrap_or(&c.path).display().to_string()).collect();
        self.handoff_context = Some(handoff_summary(&from, &requests, last_answer.as_deref(), &plan, &changed));
        self.disconnect(cx);
        self.selected = ix;
        self.agent_name = None;
        // A new session: it gets the standing instructions too.
        self.rules_sent = false;
        self.restart_keeping_transcript = true;
        self.queued_prompt = Some(format!("Carry on with this conversation: you take over from {from}."));
        self.system(format!("Handing the thread from {from} to {to}."), Color::Muted);
        self.connect(window, cx);
    }
}

/// What an agent taking a thread over gets: what the user asked, what the previous agent
/// answered last, its plan, and the files changed so far (already on disk).
pub(crate) fn handoff_summary(from: &str, requests: &[String], last_answer: Option<&str>, plan: &[(String, String)], changed: &[String]) -> String {
    let cut = |text: &str, max: usize| if text.chars().count() > max { format!("{}…", text.chars().take(max).collect::<String>()) } else { text.to_string() };
    let mut text = format!("You take over this conversation from another agent ({from}). What happened so far:\n");
    if !requests.is_empty() {
        text.push_str("\nWhat the user asked, in order:\n");
        for request in requests {
            text.push_str(&format!("- {}\n", cut(request.trim(), 600).replace('\n', "\n  ")));
        }
    }
    if let Some(answer) = last_answer.map(str::trim).filter(|a| !a.is_empty()) {
        text.push_str(&format!("\n{from}'s last answer:\n{}\n", cut(answer, 3000)));
    }
    if !plan.is_empty() {
        text.push_str("\nIts plan:\n");
        for (step, status) in plan {
            text.push_str(&format!("- [{status}] {step}\n"));
        }
    }
    if changed.is_empty() {
        text.push_str("\nNo files were changed yet.\n");
    } else {
        text.push_str(&format!("\nFiles it changed (the changes are on disk: read them before going on): {}\n", changed.join(", ")));
    }
    text.push_str("\nCarry on from there. If something is unclear, ask the user with `ask_user`.");
    text
}

#[cfg(test)]
mod handoff_tests {
    use super::handoff_summary;

    #[test]
    fn sums_up_the_thread_for_the_next_agent() {
        let summary = handoff_summary(
            "Claude",
            &["Add a /health endpoint".into(), "Also test it".into()],
            Some("Added `GET /health` in src/api.rs; the test is next."),
            &[("Add the endpoint".into(), "completed".into()), ("Write its test".into(), "pending".into())],
            &["src/api.rs".into()],
        );
        assert_eq!(
            summary,
            "You take over this conversation from another agent (Claude). What happened so far:\n\
             \nWhat the user asked, in order:\n- Add a /health endpoint\n- Also test it\n\
             \nClaude's last answer:\nAdded `GET /health` in src/api.rs; the test is next.\n\
             \nIts plan:\n- [completed] Add the endpoint\n- [pending] Write its test\n\
             \nFiles it changed (the changes are on disk: read them before going on): src/api.rs\n\
             \nCarry on from there. If something is unclear, ask the user with `ask_user`."
        );
        assert!(handoff_summary("Codex", &[], None, &[], &[]).contains("No files were changed yet."));
    }
}
