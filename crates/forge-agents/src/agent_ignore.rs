//! Files agents don't get: secrets (`.env`, keys, certificates) by default, plus what the
//! project lists in `.gitignore`'s syntax (`!pattern` gives one back), in the files
//! `agent_ignore_files` in agents.json names: by default `.forge/agentignore`, relative to the
//! project's root, and `.agentignore` in any folder, for the files below it (like `.gitignore`:
//! the deepest file that matches decides).
//!
//! Forge keeps them from agents where it is in the way: reading and writing through Forge,
//! `@` mentions, `@diff`, and permission requests for tools that name them (refused, whatever
//! the permission mode). Agents that read files with tools of their own, without asking, are
//! only told not to: the list is in the instructions they get.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Secrets, kept from agents unless the project's file gives them back (`!.env`).
pub const DEFAULTS: &[&str] = &[
    ".env",
    ".env.*",
    "!.env.example",
    "!.env.sample",
    "!.env.template",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.keystore",
    "*.jks",
    "id_rsa*",
    "id_ecdsa*",
    "id_ed25519*",
    "*.kdbx",
];

static FILES: RwLock<Option<Vec<String>>> = RwLock::new(None);

/// The files that list what agents don't get (agents.json `agent_ignore_files`).
pub fn set_files(files: Vec<String>) {
    *FILES.write().unwrap_or_else(|e| e.into_inner()) = Some(files);
}

pub fn files() -> Vec<String> {
    FILES.read().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(crate::config::default_agent_ignore_files)
}

/// The files, as told to the user and agents ("`.forge/agentignore` or `.agentignore`").
pub fn files_text() -> String {
    files().iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join(" or ")
}

/// A name with a folder in it is the project's file; a bare one counts in every folder.
fn is_nested(name: &str) -> bool {
    !name.contains('/') && !name.contains('\\')
}

pub struct AgentIgnore {
    root: PathBuf,
    /// The defaults and the project's files at the root.
    matcher: Gitignore,
    /// The patterns in effect at the root, as written (defaults first).
    patterns: Vec<String>,
    /// Names of the files that count in every folder.
    nested: Vec<String>,
    /// Folders' own files, read once per load (`None`: the folder has none).
    folders: Mutex<HashMap<PathBuf, Option<Gitignore>>>,
}

impl AgentIgnore {
    /// The defaults and the project's files in `root`, read now (so edits to them apply to
    /// the next request).
    pub fn load(root: &Path) -> Self {
        Self::load_with(root, files())
    }

    /// Same, with these file names.
    pub fn load_with(root: &Path, files: Vec<String>) -> Self {
        let own: Vec<String> = files.iter().filter(|f| !is_nested(f)).filter_map(|f| std::fs::read_to_string(root.join(f)).ok()).collect();
        let mut ignore = Self::from_text(root, &own.join("\n"));
        ignore.nested = files.into_iter().filter(|f| is_nested(f)).collect();
        // The root's nested files are the project's too: listed with the rest.
        for name in &ignore.nested {
            if let Ok(text) = std::fs::read_to_string(root.join(name)) {
                ignore.patterns.extend(lines(&text).map(String::from));
            }
        }
        ignore
    }

    /// The defaults and `own` (patterns relative to `root`), without looking in folders.
    pub fn from_text(root: &Path, own: &str) -> Self {
        let mut builder = GitignoreBuilder::new(root);
        let mut patterns = Vec::new();
        for line in DEFAULTS.iter().copied().chain(lines(own)) {
            if builder.add_line(None, line).is_ok() {
                patterns.push(line.to_string());
            } else {
                log::warn!("agent ignore file: `{line}` isn't a pattern");
            }
        }
        let matcher = builder.build().unwrap_or_else(|_| Gitignore::empty());
        Self { root: root.to_path_buf(), matcher, patterns, nested: Vec::new(), folders: Mutex::new(HashMap::new()) }
    }

    /// Whether agents are kept from `path` (absolute, or relative to the root). Paths
    /// outside the project are other rules' business.
    pub fn denies(&self, path: &Path) -> bool {
        let path = if path.is_absolute() { path.to_path_buf() } else { self.root.join(path) };
        let Ok(relative) = path.strip_prefix(&self.root) else { return false };
        if relative.as_os_str().is_empty() {
            return false;
        }
        // The deepest folder's file that says something about the path decides.
        if !self.nested.is_empty() {
            let mut folder = path.parent();
            while let Some(dir) = folder {
                if !dir.starts_with(&self.root) {
                    break;
                }
                if let Some(matcher) = self.folder(dir) {
                    let found = matcher.matched_path_or_any_parents(path.strip_prefix(dir).unwrap_or(relative), false);
                    if !found.is_none() {
                        return found.is_ignore();
                    }
                }
                if dir == self.root {
                    break;
                }
                folder = dir.parent();
            }
        }
        self.matcher.matched_path_or_any_parents(relative, false).is_ignore()
    }

    /// The patterns of `dir`'s own files (those that count in every folder).
    fn folder(&self, dir: &Path) -> Option<Gitignore> {
        let mut folders = self.folders.lock().unwrap_or_else(|e| e.into_inner());
        folders
            .entry(dir.to_path_buf())
            .or_insert_with(|| {
                let mut builder = GitignoreBuilder::new(dir);
                let mut any = false;
                for name in &self.nested {
                    if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
                        for line in lines(&text) {
                            any |= builder.add_line(None, line).is_ok();
                        }
                    }
                }
                any.then(|| builder.build().ok()).flatten()
            })
            .clone()
    }

    /// What an agent is told when it asks for `path`.
    pub fn refusal(&self, path: &Path) -> String {
        let shown = path.strip_prefix(&self.root).unwrap_or(path).display();
        format!("{shown} is kept from agents in this project (secrets, or listed in {}): don't read or change it, and ask the user if you need something from it.", files_text())
    }

    /// The patterns at the root (folders' own files aren't read up front).
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }
}

/// A file's patterns: lines that aren't blank or comments.
fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'))
}

/// `git diff` output without the files agents are kept from.
pub fn filter_diff(diff: &str, ignore: &AgentIgnore) -> String {
    let mut out = String::new();
    let mut keep = true;
    for line in diff.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            let path = rest.split(" b/").next().unwrap_or_default();
            keep = !ignore.denies(Path::new(path));
        }
        if keep {
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_kept_from_agents_by_default() {
        let ignore = AgentIgnore::from_text(Path::new("/p"), "");
        for denied in [".env", ".env.local", "config/.env.production", "certs/server.pem", "deploy/id_rsa", "keys/app.key"] {
            assert!(ignore.denies(Path::new(denied)), "{denied}");
        }
        for allowed in ["src/main.rs", ".env.example", "README.md", "keys.md", "/elsewhere/.env"] {
            assert!(!ignore.denies(Path::new(allowed)), "{allowed}");
        }
        assert!(ignore.denies(Path::new("/p/.env")), "absolute paths in the project too");
    }

    #[test]
    fn the_project_adds_and_gives_back() {
        let own = "# customer data\ndata/customers/\n*.sqlite\n!.env.test\n";
        let ignore = AgentIgnore::from_text(Path::new("/p"), own);
        assert!(ignore.denies(Path::new("data/customers/2024.csv")), "a folder's files");
        assert!(ignore.denies(Path::new("local.sqlite")));
        assert!(!ignore.denies(Path::new(".env.test")), "given back");
        assert!(ignore.denies(Path::new(".env")), "the defaults stay");
        assert!(ignore.patterns().contains(&"data/customers/".to_string()));
    }

    #[test]
    fn folders_have_their_own_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".forge")).unwrap();
        std::fs::create_dir_all(root.join("app/fixtures/deep")).unwrap();
        std::fs::write(root.join(".forge/agentignore"), "*.sqlite\n").unwrap();
        std::fs::write(root.join(".agentignore"), "/notes.md\n").unwrap();
        std::fs::write(root.join("app/.agentignore"), "fixtures/\n!keep.sqlite\n").unwrap();
        std::fs::write(root.join("app/fixtures/deep/.agentignore"), "!open.json\n").unwrap();
        let ignore = AgentIgnore::load(root);
        assert!(ignore.denies(Path::new("notes.md")), "the root's .agentignore");
        assert!(!ignore.denies(Path::new("app/notes.md")), "anchored to its folder");
        assert!(ignore.denies(Path::new("app/fixtures/users.json")), "relative to app/");
        assert!(!ignore.denies(Path::new("fixtures/users.json")), "only below app/");
        assert!(ignore.denies(Path::new("db.sqlite")), "the project's file");
        assert!(!ignore.denies(Path::new("app/keep.sqlite")), "given back deeper down");
        assert!(ignore.denies(Path::new("app/fixtures/deep/other.json")));
        assert!(!ignore.denies(Path::new("app/fixtures/deep/open.json")), "the deepest file decides");
        assert!(ignore.patterns().contains(&"/notes.md".to_string()));

        let renamed = AgentIgnore::load_with(root, vec!["agents.ignore".into()]);
        assert!(!renamed.denies(Path::new("db.sqlite")) && !renamed.denies(Path::new("app/fixtures/users.json")), "other names, other files");
        assert!(renamed.denies(Path::new(".env")), "the defaults stay");
    }

    #[test]
    fn diffs_lose_the_kept_files() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/.env b/.env\n--- a/.env\n+++ b/.env\n@@ -1 +1 @@\n-KEY=old\n+KEY=new\n";
        let filtered = filter_diff(diff, &AgentIgnore::from_text(Path::new("/p"), ""));
        assert!(filtered.contains("src/a.rs") && !filtered.contains("KEY="), "{filtered}");
    }
}
