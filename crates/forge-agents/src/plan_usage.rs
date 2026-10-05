//! How much of the subscription the agent's account has used (Claude's 5-hour and weekly
//! limits), shown next to the context in the thread header.
//!
//! ACP has no message for it, but Claude's adapter answers its `/usage` command with
//! those limits, locally (no model turn, no tokens). Forge asks in a hidden session of the
//! thread's agent process, so the conversation stays clean, and only when the agent
//! advertises the command. The answer is Markdown; the limit lines read
//! `**5-hour limit** — **42%** · Resets Oct 3, 5:00 PM GMT+2`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{App, Global};

/// Asking more often than this adds nothing: the limits move slowly.
pub const REFRESH_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq)]
pub struct Limit {
    pub label: String,
    pub percent: f32,
    pub resets: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct PlanUsage {
    /// "Pro", "Max"…
    pub subscription: Option<String>,
    pub limits: Vec<Limit>,
}

impl PlanUsage {
    /// The limit closest to running out.
    pub fn tightest(&self) -> Option<&Limit> {
        self.limits.iter().max_by(|a, b| a.percent.total_cmp(&b.percent))
    }
}

/// Reads the adapter's `/usage` Markdown. `None` when it has no limits (API-key accounts).
pub fn parse(markdown: &str) -> Option<PlanUsage> {
    let mut usage = PlanUsage::default();
    for line in markdown.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("> Claude ").and_then(|r| r.strip_suffix(" subscription usage")) {
            usage.subscription = Some(rest.replace('\\', ""));
            continue;
        }
        // **Label** — **42%** · Resets …
        let Some(rest) = line.strip_prefix("**") else { continue };
        let Some((label, rest)) = rest.split_once("** — **") else { continue };
        let Some((percent, rest)) = rest.split_once("%**") else { continue };
        let Ok(percent) = percent.trim().parse::<f32>() else { continue };
        let resets = rest.trim().strip_prefix("· Resets ").map(|r| r.trim().to_string());
        usage.limits.push(Limit { label: label.replace('\\', ""), percent, resets });
    }
    (!usage.limits.is_empty()).then_some(usage)
}

/// The latest answer per agent (`id` in agents.json), shared by its threads.
#[derive(Default)]
pub struct PlanUsageStore {
    by_agent: HashMap<String, (PlanUsage, Instant)>,
    asked: HashMap<String, Instant>,
}

impl Global for PlanUsageStore {}

pub fn get(agent: &str, cx: &App) -> Option<PlanUsage> {
    cx.try_global::<PlanUsageStore>()?.by_agent.get(agent).map(|(u, _)| u.clone())
}

pub fn set(agent: &str, usage: PlanUsage, cx: &mut App) {
    cx.default_global::<PlanUsageStore>().by_agent.insert(agent.to_string(), (usage, Instant::now()));
}

/// Whether to ask again now; records the attempt.
pub fn should_ask(agent: &str, cx: &mut App) -> bool {
    let store = cx.default_global::<PlanUsageStore>();
    let due = store.asked.get(agent).is_none_or(|t| t.elapsed() >= REFRESH_EVERY);
    if due {
        store.asked.insert(agent.to_string(), Instant::now());
    }
    due
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_adapters_usage_markdown() {
        let md = "## Usage\n\n> Claude Max subscription usage\n\n### Limits\n\n**5-hour limit** — **42%** · Resets Oct 3, 5:00 PM GMT+2\n\n`████████░░░░░░░░░░░░`\n\n**Weekly · all models** — **18%** · Resets Oct 7, 9:00 AM GMT+2\n\n`████░░░░░░░░░░░░░░░░`\n\n**Weekly · Opus** — **7.5%**\n\n---\n\n### This session\n\n| Cost | API time | Active |";
        let usage = parse(md).unwrap();
        assert_eq!(usage.subscription.as_deref(), Some("Max"));
        assert_eq!(usage.limits.len(), 3);
        assert_eq!(usage.limits[0], Limit { label: "5-hour limit".into(), percent: 42.0, resets: Some("Oct 3, 5:00 PM GMT+2".into()) });
        assert_eq!(usage.limits[2].resets, None);
        assert_eq!(usage.tightest().unwrap().label, "5-hour limit");
        assert_eq!(parse("## Usage\n\n### This session\n\n| Cost |"), None, "no limits (API key)");
    }
}
