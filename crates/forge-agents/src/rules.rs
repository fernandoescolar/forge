//! Standing instructions for agents, sent with the first message of every new session:
//! the user's own (`<config>/AGENTS.md`, for every project) and the project's
//! (`.forge/AGENTS.md`).
//!
//! Agents already read the repository's `AGENTS.md` / `CLAUDE.md` themselves, so those are
//! left to them; these are the instructions only Forge knows about.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

/// Where the project's instructions live, relative to its root.
pub const PROJECT_FILE: &str = ".forge/AGENTS.md";

/// The user's instructions for every project.
pub fn user_file() -> PathBuf {
    paths::config_dir().join("AGENTS.md")
}

#[derive(Debug, PartialEq)]
pub struct Rules {
    /// Each file that has instructions, as shown to the user, with its text.
    pub sources: Vec<(String, String)>,
}

impl Rules {
    /// The files, for a note in the thread.
    pub fn label(&self) -> String {
        self.sources.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(" and ")
    }

    /// The prompt block that carries them (after the user's message, like other context).
    pub fn block(&self) -> Value {
        let mut text = String::from("Standing instructions from the user, set up in Forge. Follow them for the whole session.\n");
        for (name, body) in &self.sources {
            text.push_str(&format!("\n<instructions source=\"{name}\">\n{}\n</instructions>\n", body.trim()));
        }
        json!({ "type": "text", "text": text })
    }
}

/// The instructions that apply to the project in `root`, if any file has some.
pub async fn load(fs: Arc<dyn fs::Fs>, root: PathBuf, user_file: PathBuf) -> Option<Rules> {
    let mut sources = Vec::new();
    for (name, path) in [("your AGENTS.md".to_string(), user_file), (PROJECT_FILE.to_string(), root.join(PROJECT_FILE))] {
        if let Ok(text) = fs.load(Path::new(&path)).await {
            if !text.trim().is_empty() {
                sources.push((name, text));
            }
        }
    }
    (!sources.is_empty()).then_some(Rules { sources })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn loads_the_user_and_project_files(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/config", serde_json::json!({ "AGENTS.md": "Answer in Spanish.\n" })).await;
        fs.insert_tree("/p", serde_json::json!({ ".forge": { "AGENTS.md": "Run `cargo test` before finishing." } })).await;
        fs.insert_tree("/empty", serde_json::json!({ ".forge": { "AGENTS.md": "  \n" } })).await;

        let rules = load(fs.clone(), "/p".into(), "/config/AGENTS.md".into()).await.unwrap();
        assert_eq!(rules.label(), "your AGENTS.md and .forge/AGENTS.md");
        let text = rules.block()["text"].as_str().unwrap().to_string();
        assert!(text.contains("<instructions source=\"your AGENTS.md\">\nAnswer in Spanish.\n</instructions>"), "{text}");
        assert!(text.contains("<instructions source=\".forge/AGENTS.md\">\nRun `cargo test` before finishing.\n</instructions>"), "{text}");

        let project_only = load(fs.clone(), "/p".into(), "/nowhere/AGENTS.md".into()).await.unwrap();
        assert_eq!(project_only.label(), ".forge/AGENTS.md");
        assert_eq!(load(fs, "/empty".into(), "/nowhere/AGENTS.md".into()).await, None, "blank files don't count");
    }
}
