//! Standing instructions for agents, sent with the first message of every new session:
//! the user's own (`<config>/AGENTS.md`, for every project) and the project's, from the
//! files `instructions_files` in agents.json lists (by default `.forge/AGENTS.md` and
//! `AGENTS.md`, relative to the project's root). Agents' notes (`remember`) go to the
//! project's first file that exists.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

/// The user's instructions for every project.
pub fn user_file() -> PathBuf {
    paths::config_dir().join("AGENTS.md")
}

/// The project's instructions file to edit and add notes to: the first of `files` that exists
/// in `root`, else the first of them (created when written).
pub fn project_file(root: &Path, files: &[String]) -> PathBuf {
    let candidates: Vec<PathBuf> = files.iter().map(|f| root.join(f)).collect();
    candidates.iter().find(|p| p.is_file()).or(candidates.first()).cloned().unwrap_or_else(|| root.join(".forge/AGENTS.md"))
}

/// The project's instructions files as configured (agents.json `instructions_files`).
pub fn configured_files(cx: &gpui::App) -> Vec<String> {
    crate::settings::try_global(cx).map(|s| s.read(cx).config().instructions_files.clone()).unwrap_or_else(crate::config::default_instructions_files)
}

/// The heading agents' notes go under, in the project's instructions file.
const NOTES_HEADING: &str = "## Notes from agents";

/// `file` (the instructions file's text) with `note` added as a bullet under
/// [`NOTES_HEADING`], which is added at the end the first time.
pub fn with_note(file: &str, note: &str) -> String {
    let bullet = format!("- {}", note.trim().replace('\n', "\n  "));
    let mut text = file.trim_end().to_string();
    match text.find(NOTES_HEADING) {
        Some(at) => {
            // The end of that section: the next heading of its level or higher, or the end.
            let after = at + NOTES_HEADING.len();
            let end = text[after..].find("\n#").map(|i| after + i).unwrap_or(text.len());
            let section = text[..end].trim_end().len();
            text.insert_str(section, &format!("\n{bullet}"));
        }
        None => {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!("{NOTES_HEADING}\n\n{bullet}"));
        }
    }
    text.push('\n');
    text
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

/// The instructions that apply to the project in `root`, if any file has some:
/// `user_file`, then the project's `project_files` (relative to `root`).
pub async fn load(fs: Arc<dyn fs::Fs>, root: PathBuf, user_file: PathBuf, project_files: Vec<String>) -> Option<Rules> {
    let mut sources = Vec::new();
    let files = std::iter::once(("your AGENTS.md".to_string(), user_file)).chain(project_files.into_iter().map(|f| (f.clone(), root.join(&f))));
    for (name, path) in files {
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

    #[test]
    fn adds_notes_under_their_heading() {
        assert_eq!(with_note("", "Run `cargo test` first."), "## Notes from agents\n\n- Run `cargo test` first.\n");
        let once = with_note("# Project\n\nUse tabs.\n", "Tests need Docker.");
        assert_eq!(once, "# Project\n\nUse tabs.\n\n## Notes from agents\n\n- Tests need Docker.\n");
        let twice = with_note(&(once + "\n## Later\n\nMore.\n"), "Two lines\nof note");
        assert_eq!(twice, "# Project\n\nUse tabs.\n\n## Notes from agents\n\n- Tests need Docker.\n- Two lines\n  of note\n\n## Later\n\nMore.\n");
    }

    #[gpui::test]
    async fn loads_the_user_and_project_files(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/config", serde_json::json!({ "AGENTS.md": "Answer in Spanish.\n" })).await;
        fs.insert_tree("/p", serde_json::json!({ ".forge": { "AGENTS.md": "Run `cargo test` before finishing." } })).await;
        fs.insert_tree("/empty", serde_json::json!({ ".forge": { "AGENTS.md": "  \n" } })).await;

        let files = crate::config::default_instructions_files();
        let rules = load(fs.clone(), "/p".into(), "/config/AGENTS.md".into(), files.clone()).await.unwrap();
        assert_eq!(rules.label(), "your AGENTS.md and .forge/AGENTS.md");
        let text = rules.block()["text"].as_str().unwrap().to_string();
        assert!(text.contains("<instructions source=\"your AGENTS.md\">\nAnswer in Spanish.\n</instructions>"), "{text}");
        assert!(text.contains("<instructions source=\".forge/AGENTS.md\">\nRun `cargo test` before finishing.\n</instructions>"), "{text}");

        let project_only = load(fs.clone(), "/p".into(), "/nowhere/AGENTS.md".into(), files.clone()).await.unwrap();
        assert_eq!(project_only.label(), ".forge/AGENTS.md");
        assert_eq!(load(fs.clone(), "/empty".into(), "/nowhere/AGENTS.md".into(), files.clone()).await, None, "blank files don't count");

        // The repository's AGENTS.md too, by default; other files when configured.
        fs.insert_tree("/both", serde_json::json!({ ".forge": { "AGENTS.md": "Forge's." }, "AGENTS.md": "The repo's.", "docs": { "agents.md": "Docs'." } })).await;
        assert_eq!(load(fs.clone(), "/both".into(), "/nowhere/AGENTS.md".into(), files).await.unwrap().label(), ".forge/AGENTS.md and AGENTS.md");
        let custom = load(fs, "/both".into(), "/nowhere/AGENTS.md".into(), vec!["docs/agents.md".into()]).await.unwrap();
        assert_eq!(custom.label(), "docs/agents.md");
    }
}
