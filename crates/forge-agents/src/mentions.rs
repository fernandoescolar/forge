//! Prompt context: `@file` mentions (with completion in the composer), and the active
//! editor's file and selection, turned into ACP content blocks.

use editor::{CompletionProvider, Editor};
use fuzzy::StringMatchCandidate;
use gpui::{App, AppContext as _, Context, Entity, Task, WeakEntity, Window};
use language::{Buffer, ToOffset as _};
use multi_buffer::ToPoint as _;
use project::{CompletionDisplayOptions, CompletionResponse, Project};
use serde_json::{Value, json};
use std::{
    ops::Range,
    path::{Path, PathBuf},
};
use workspace::Workspace;

const MAX_RESULTS: usize = 50;

/// The file (and selection) in the active editor.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveContext {
    pub path: PathBuf,
    /// 1-based inclusive line range and the selected text, when something is selected.
    pub selection: Option<(Range<u32>, String)>,
}

impl ActiveContext {
    pub fn label(&self, root: &Path) -> String {
        let name = self.path.strip_prefix(root).unwrap_or(&self.path).to_string_lossy().into_owned();
        match &self.selection {
            Some((lines, _)) if lines.start == lines.end => format!("{name}:{}", lines.start),
            Some((lines, _)) => format!("{name}:{}-{}", lines.start, lines.end),
            None => name,
        }
    }
}

pub fn active_context(workspace: &Workspace, cx: &App) -> Option<ActiveContext> {
    // `downcast` checks the type without reading the item: when the active item is the
    // thread being drawn, reading it (as `active_item_as` does) would panic.
    let editor = workspace.active_item(cx)?.downcast::<Editor>()?;
    let editor = editor.read(cx);
    let buffer = editor.buffer().read(cx);
    let path = buffer.as_singleton()?.read(cx).file()?.as_local()?.abs_path(cx);
    let snapshot = buffer.snapshot(cx);
    let selection = editor.selections.newest_anchor();
    let (start, end) = (selection.start.to_point(&snapshot), selection.end.to_point(&snapshot));
    let selection = (start != end).then(|| {
        let text: String = snapshot.text_for_range(start..end).collect();
        // A selection ending at column 0 doesn't include that line.
        let last = if end.column == 0 && end.row > start.row { end.row - 1 } else { end.row };
        (start.row + 1..last + 1, text)
    });
    Some(ActiveContext { path, selection })
}

/// What an `@token` stands for.
#[derive(Debug, Clone, PartialEq)]
pub enum Mention {
    /// A file or folder of the project, at a line when given (`@src/a.rs:42`).
    Path { path: PathBuf, line: Option<u32> },
    /// One of [`SPECIAL_MENTIONS`] (`problems`, `diff`, `terminal`).
    Special(String),
}

/// The message's mentions that resolve to something: special names (unless the project
/// has a file of that name), then existing paths under `root`.
pub fn resolve_mentions(text: &str, root: &Path) -> Vec<Mention> {
    mentioned_paths(text)
        .into_iter()
        .filter_map(|token| {
            if SPECIAL_MENTIONS.iter().any(|(name, _)| *name == token) && !root.join(&token).exists() {
                return Some(Mention::Special(token));
            }
            let (path, line) = match token.rsplit_once(':') {
                Some((path, line)) if !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()) => (path.to_string(), line.parse().ok()),
                _ => (token.clone(), None),
            };
            let path = root.join(path.trim_end_matches('/'));
            path.exists().then_some(Mention::Path { path, line })
        })
        .collect()
}

/// `@tokens` in the message, in order, without duplicates.
pub fn mentioned_paths(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        let Some(path) = word.strip_prefix('@') else { continue };
        let path = path.trim_end_matches([',', '.', ';', ':', ')', '!', '?']);
        if !path.is_empty() && !out.iter().any(|p| p == path) {
            out.push(path.to_string());
        }
    }
    out
}

fn file_uri(path: &Path) -> String {
    url::Url::from_file_path(path).map(|u| u.to_string()).unwrap_or_else(|_| format!("file://{}", path.display()))
}

/// Builds `session/prompt` content. Mentions and the active file become `resource_link`s
/// (every ACP agent supports those); a selection is embedded when the agent supports
/// `embeddedContext`, else quoted in a text block.
pub fn prompt_blocks(text: &str, mentions: &[(PathBuf, Option<u32>)], active: Option<&ActiveContext>, embedded_context: bool, root: &Path) -> Vec<Value> {
    let mut blocks = vec![json!({ "type": "text", "text": text })];
    let mut linked: Vec<(PathBuf, Option<u32>)> = Vec::new();
    let mut link = |path: &Path, line: Option<u32>, blocks: &mut Vec<Value>| {
        if linked.iter().any(|(p, l)| p == path && *l == line) {
            return;
        }
        linked.push((path.to_path_buf(), line));
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let uri = match line {
            Some(line) => format!("{}#L{line}", file_uri(path)),
            None => file_uri(path),
        };
        blocks.push(json!({ "type": "resource_link", "uri": uri, "name": name }));
    };
    for (path, line) in mentions {
        link(path, *line, &mut blocks);
    }
    if let Some(active) = active {
        match &active.selection {
            Some((lines, selected)) => {
                let uri = format!("{}#L{}-{}", file_uri(&active.path), lines.start, lines.end);
                if embedded_context {
                    blocks.push(json!({ "type": "resource", "resource": { "uri": uri, "mimeType": "text/plain", "text": selected } }));
                } else {
                    let label = active.label(root);
                    blocks.push(json!({ "type": "text", "text": format!("Selected in {label}:\n```\n{selected}\n```") }));
                }
                link(&active.path, None, &mut blocks);
            }
            None => link(&active.path, None, &mut blocks),
        }
    }
    blocks
}

/// What the composer completes: `@` mentions (files, folders, symbols and the special
/// `@problems`, `@diff`, `@terminal`) and, at the start of the message, the agent's `/`
/// commands.
pub struct FileMentions {
    project: WeakEntity<Project>,
    thread: Option<WeakEntity<crate::thread::Thread>>,
}

/// Mentions that stand for context Forge gathers when the message is sent.
pub const SPECIAL_MENTIONS: &[(&str, &str)] = &[
    ("problems", "The errors and warnings in the project"),
    ("diff", "Your uncommitted changes (git diff)"),
    ("terminal", "The latest output of the terminal"),
];

impl FileMentions {
    pub fn new(project: WeakEntity<Project>) -> Self {
        Self { project, thread: None }
    }

    /// Also completes the thread's slash commands (the user's prompts and the agent's) after a leading `/`.
    pub fn with_commands(mut self, thread: WeakEntity<crate::thread::Thread>) -> Self {
        self.thread = Some(thread);
        self
    }

    /// Start of the `@token` the cursor is in, if any.
    fn mention_start(buffer: &Buffer, position: language::Anchor) -> Option<usize> {
        let offset = position.to_offset(buffer);
        let mut start = offset;
        for ch in buffer.reversed_chars_at(position) {
            if ch == '@' {
                return Some(start - 1);
            }
            if ch.is_whitespace() {
                return None;
            }
            start -= ch.len_utf8();
        }
        None
    }

    /// The cursor is in a `/command` that starts the message.
    fn command_start(buffer: &Buffer, position: language::Anchor) -> bool {
        let offset = position.to_offset(buffer);
        let typed: String = buffer.text_for_range(0..offset).collect();
        typed.starts_with('/') && !typed.contains(char::is_whitespace)
    }

    /// The project's files and folders, relative (prefixed by the folder name when the
    /// project has several), folders ending in `/`.
    fn project_entries(&self, cx: &App) -> Vec<String> {
        let Some(project) = self.project.upgrade() else { return vec![] };
        let worktrees: Vec<_> = project.read(cx).visible_worktrees(cx).collect();
        let prefix_roots = worktrees.len() > 1;
        let mut out = Vec::new();
        for wt in worktrees {
            let wt = wt.read(cx);
            let root = wt.root_name().as_unix_str().to_string();
            // Files kept from agents aren't offered.
            let kept = crate::agent_ignore::AgentIgnore::load(&wt.abs_path());
            for entry in wt.snapshot().entries(false, 0) {
                let rel = entry.path.as_unix_str();
                if rel.is_empty() || kept.denies(std::path::Path::new(rel)) {
                    continue;
                }
                let rel = if entry.is_dir() { format!("{rel}/") } else { rel.to_string() };
                out.push(if prefix_roots { format!("{root}/{rel}") } else { rel });
            }
        }
        out
    }

    fn command_completions(&self, buffer: &Entity<Buffer>, position: language::Anchor, cx: &mut Context<Editor>) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        let commands = self.thread.as_ref().and_then(|t| t.upgrade()).map(|t| t.read(cx).commands()).unwrap_or_default();
        let buffer = buffer.read(cx);
        let offset = position.to_offset(buffer);
        let query: String = buffer.text_for_range(1..offset).collect();
        let replace_range = buffer.anchor_before(0)..position;
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let candidates: Vec<_> = commands.iter().enumerate().map(|(i, c)| StringMatchCandidate::new(i, &c.name)).collect();
            let matches = fuzzy::match_strings(&candidates, &query, false, true, MAX_RESULTS, &Default::default(), executor).await;
            let completions = matches
                .into_iter()
                .map(|m| {
                    let command = &commands[m.candidate_id];
                    let label = match &command.hint {
                        Some(hint) => format!("/{} {hint}", command.name),
                        None => format!("/{}", command.name),
                    };
                    let documentation = (!command.description.is_empty()).then(|| project::lsp_store::CompletionDocumentation::SingleLine(command.description.clone().into()));
                    completion(replace_range.clone(), label, format!("/{} ", command.name), documentation)
                })
                .collect();
            Ok(vec![CompletionResponse { completions, display_options: CompletionDisplayOptions { dynamic_width: true }, is_incomplete: true }])
        })
    }
}

fn completion(replace_range: std::ops::Range<language::Anchor>, label: String, new_text: String, documentation: Option<project::lsp_store::CompletionDocumentation>) -> project::Completion {
    project::Completion {
        replace_range,
        label: language::CodeLabel::plain(label, None),
        new_text,
        documentation,
        source: project::CompletionSource::Custom,
        icon_path: None,
        icon_color: None,
        group: None,
        match_start: None,
        snippet_deduplication_key: None,
        insert_text_mode: None,
        confirm: None,
    }
}

impl CompletionProvider for FileMentions {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        _: editor::CompletionContext,
        _: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        if self.thread.is_some() && Self::command_start(buffer.read(cx), position) {
            return self.command_completions(buffer, position, cx);
        }
        let b = buffer.read(cx);
        let Some(at) = Self::mention_start(b, position) else { return Task::ready(Ok(vec![])) };
        let replace_range = b.anchor_before(at)..position;
        let query: String = b.text_for_range(at + 1..position.to_offset(b)).collect();
        let entries = self.project_entries(cx);
        let executor = cx.background_executor().clone();
        // Symbols by name, from the language servers, for queries long enough to mean one.
        let project = self.project.upgrade();
        let symbols = match (&project, query.len() >= 3) {
            (Some(project), true) => Some(project.update(cx, |p, cx| p.symbols(&query, cx))),
            _ => None,
        };
        cx.spawn(async move |_, cx| {
            let mut completions = Vec::new();
            for (name, description) in SPECIAL_MENTIONS {
                if name.starts_with(&query.to_lowercase()) {
                    completions.push(completion(replace_range.clone(), format!("@{name}"), format!("@{name} "), Some(project::lsp_store::CompletionDocumentation::SingleLine((*description).into()))));
                }
            }
            let candidates: Vec<_> = entries.iter().enumerate().map(|(i, f)| StringMatchCandidate::new(i, f)).collect();
            let matches = fuzzy::match_strings(&candidates, &query, false, true, MAX_RESULTS, &Default::default(), executor).await;
            completions.extend(matches.into_iter().map(|m| {
                let path = &entries[m.candidate_id];
                completion(replace_range.clone(), path.clone(), format!("@{path} "), None)
            }));
            if let (Some(symbols), Some(project)) = (symbols, project) {
                let symbols = symbols.await.unwrap_or_default();
                let found = cx.update(|cx| {
                    let project = project.read(cx);
                    let several = project.visible_worktrees(cx).count() > 1;
                    symbols
                        .into_iter()
                        .take(20)
                        .filter_map(|symbol| {
                            let project::lsp_store::SymbolLocation::InProject(path) = &symbol.path else { return None };
                            let worktree = project.worktree_for_id(path.worktree_id, cx)?;
                            let rel = path.path.as_unix_str();
                            let rel = if several { format!("{}/{rel}", worktree.read(cx).root_name().as_unix_str()) } else { rel.to_string() };
                            let line = symbol.range.start.0.row + 1;
                            Some((format!("{} — {rel}:{line}", symbol.name), format!("@{rel}:{line} ")))
                        })
                        .collect::<Vec<_>>()
                });
                completions.extend(found.into_iter().map(|(label, text)| completion(replace_range.clone(), label, text, None)));
            }
            Ok(vec![CompletionResponse { completions, display_options: CompletionDisplayOptions { dynamic_width: true }, is_incomplete: true }])
        })
    }

    fn is_completion_trigger(&self, buffer: &Entity<Buffer>, position: language::Anchor, text: &str, _: bool, cx: &mut Context<Editor>) -> bool {
        let b = buffer.read(cx);
        text == "@" || Self::mention_start(b, position).is_some() || (self.thread.is_some() && Self::command_start(b, position))
    }

    fn sort_completions(&self) -> bool {
        false
    }

    fn filter_completions(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_mentions() {
        assert_eq!(mentioned_paths("look at @src/main.rs, and @README.md. not mail@x"), vec!["src/main.rs", "README.md"]);
        assert_eq!(mentioned_paths("@a @a @"), vec!["a"]);
    }

    #[test]
    fn builds_acp_blocks() {
        let root = Path::new("/p");
        let active = ActiveContext { path: "/p/src/lib.rs".into(), selection: Some((3..4, "fn x() {}".into())) };
        let blocks = prompt_blocks("why?", &[("/p/a.rs".into(), None), ("/p/src/lib.rs".into(), None)], Some(&active), true, root);
        assert_eq!(blocks[0], json!({"type": "text", "text": "why?"}));
        assert_eq!(blocks[1]["type"], "resource_link");
        assert_eq!(blocks[1]["uri"], "file:///p/a.rs");
        assert_eq!(blocks[3]["resource"]["uri"], "file:///p/src/lib.rs#L3-4");
        assert_eq!(blocks[3]["resource"]["text"], "fn x() {}");
        assert_eq!(blocks.len(), 4, "lib.rs linked once even though mentioned and active");

        let plain = prompt_blocks("why?", &[], Some(&active), false, root);
        assert!(plain[1]["text"].as_str().unwrap().starts_with("Selected in src/lib.rs:3-4"));
        assert_eq!(plain[2]["type"], "resource_link");
    }

    #[test]
    fn resolves_mentions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "").unwrap();
        let found = resolve_mentions("see @src/a.rs:12 @src/ @problems @diff, @missing.rs @terminal", dir.path());
        assert_eq!(
            found,
            vec![
                Mention::Path { path: dir.path().join("src/a.rs"), line: Some(12) },
                Mention::Path { path: dir.path().join("src"), line: None },
                Mention::Special("problems".into()),
                Mention::Special("diff".into()),
                Mention::Special("terminal".into()),
            ]
        );
    }

    #[test]
    fn labels() {
        let root = Path::new("/p");
        assert_eq!(ActiveContext { path: "/p/a.rs".into(), selection: None }.label(root), "a.rs");
        assert_eq!(ActiveContext { path: "/p/a.rs".into(), selection: Some((7..7, "x".into())) }.label(root), "a.rs:7");
    }

    #[gpui::test]
    async fn mention_detection_and_project_files(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({ "src": { "main.rs": "", "lib.rs": "" }, "README.md": "" })).await;
        let project = Project::test(fs, ["/root".as_ref()], cx).await;
        let provider = FileMentions::new(project.downgrade());

        let mut files = cx.update(|cx| provider.project_entries(cx));
        files.sort();
        assert_eq!(files, vec!["README.md", "src/", "src/lib.rs", "src/main.rs"]);

        let buffer = cx.new(|cx| Buffer::local("see @src/ma and mail@x y", cx));
        cx.update(|cx| {
            let b = buffer.read(cx);
            let at = |offset: usize| b.anchor_before(offset);
            assert_eq!(FileMentions::mention_start(b, at(11)), Some(4), "inside @src/ma");
            assert_eq!(FileMentions::mention_start(b, at(3)), None, "plain word");
            assert_eq!(FileMentions::mention_start(b, at(24)), None, "after whitespace");
        });
        let commands = cx.new(|cx| Buffer::local("/us more", cx));
        cx.update(|cx| {
            let b = commands.read(cx);
            assert!(FileMentions::command_start(b, b.anchor_before(3)), "a command starting the message");
            assert!(!FileMentions::command_start(b, b.anchor_before(8)), "past the command");
        });
    }
}
