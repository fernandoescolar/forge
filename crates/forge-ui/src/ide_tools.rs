//! Tools Forge's own crates offer agents through the `forge` MCP server (`run_tests` from
//! the Tests panel, `run_app` from Run, `http_request` from the HTTP client…), and the
//! hooks those tools share. Each crate registers its tools in `init`; Forge's MCP server
//! (crates/forge-agents) lists and calls them without depending on the crates that make
//! them, as it does for extensions' tools (`agent_tools`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use futures::future::LocalBoxFuture;
use gpui::{App, AsyncWindowContext, WeakEntity};
use project::Project;
use serde_json::Value;
use workspace::Workspace;

/// What a tool call answers the agent: text, or what went wrong.
pub type ToolReply = Result<String, String>;

/// What a tool works on: the thread's window and project.
#[derive(Clone)]
pub struct ToolContext {
    pub project: WeakEntity<Project>,
    pub workspace: WeakEntity<Workspace>,
    /// The thread's folder: relative paths start there, and paths are shown from it.
    pub root: PathBuf,
    /// The workspace's folders (`root` first): paths outside them are refused…
    pub roots: Vec<PathBuf>,
    /// …unless the user lets agents touch files outside the workspace.
    pub outside_ok: bool,
}

impl ToolContext {
    /// `path` (relative to `root`, or absolute) as an absolute path, refused when it leaves
    /// the workspace and the user doesn't allow that.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let abs = normalize(&self.root.join(path));
        if self.outside_ok || within(&abs, &self.roots) || in_temp(&abs) {
            Ok(abs)
        } else {
            Err(format!(
                "{path} is outside the project: ask the user if you need it (they can let agents reach files outside the workspace in Settings › Agents). For scratch files, use the system's folder for temporary files."
            ))
        }
    }

    pub fn show(&self, path: &Path) -> String {
        path.strip_prefix(&self.root).unwrap_or(path).to_string_lossy().into_owned()
    }
}

/// `path` without `.` and `..` (without touching the disk): `/p/a/../b` is `/p/b`.
pub fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `path` (absolute, normalized) is in one of `roots`.
pub fn within(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(normalize(root)))
}

/// The system's folders for temporary files, as they really are: the user's (`$TMPDIR`;
/// `%TEMP%` on Windows) and `/tmp` (which on macOS is `/private/tmp`).
fn temp_dirs() -> &'static [PathBuf] {
    static DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut dirs = vec![std::env::temp_dir()];
        if cfg!(unix) {
            dirs.push(PathBuf::from("/tmp"));
        }
        let mut dirs: Vec<PathBuf> = dirs.iter().filter_map(|d| std::fs::canonicalize(d).ok()).filter(|d| d.parent().is_some()).collect();
        dirs.sort();
        dirs.dedup();
        dirs
    })
}

/// Where `path` (absolute) really is: the deepest part of it that exists with its links
/// followed (on Windows also long names and the `\\?\` form), and the rest.
fn real_path(path: &Path) -> Option<PathBuf> {
    let path = normalize(path);
    path.ancestors().find_map(|a| Some(std::fs::canonicalize(a).ok()?.join(path.strip_prefix(a).ok()?)))
}

/// `path` is `dir` or below it; on Windows (`windows`) whatever the case.
fn under(path: &Path, dir: &Path, windows: bool) -> bool {
    if windows {
        let (path, dir) = (path.to_string_lossy().to_lowercase(), dir.to_string_lossy().to_lowercase());
        let dir = dir.trim_end_matches(['\\', '/']);
        path == dir || path.starts_with(&format!("{dir}\\"))
    } else {
        path.starts_with(dir)
    }
}

/// `path` (absolute) is in a folder for temporary files, where agents work as in the
/// project: scratch files, scripts they write and run. Judged by where it really is, so a
/// link in there that leads elsewhere doesn't count.
pub fn in_temp(path: &Path) -> bool {
    real_path(path).is_some_and(|real| temp_dirs().iter().any(|dir| under(&real, dir, cfg!(windows))))
}

/// What a tool does beyond reading, which decides whether it asks the user first (see
/// forge-agents' `permissions`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolEffect {
    /// Reads, or shows something: it runs when called.
    Reads,
    /// Runs the project's code (tests, the app, the debugger): it asks where commands do.
    RunsCode,
    /// Reaches the network, possibly with the project's secrets: it asks unless everything
    /// may run.
    Network,
}

pub type RunTool = Arc<dyn Fn(ToolContext, Value, AsyncWindowContext) -> LocalBoxFuture<'static, ToolReply> + Send + Sync>;

/// One tool, as its crate registered it.
#[derive(Clone)]
pub struct IdeTool {
    pub name: &'static str,
    /// What the thread shows for a call ("Run tests").
    pub title: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub effect: ToolEffect,
    pub run: RunTool,
}

impl IdeTool {
    /// A tool whose calls run `run`.
    pub fn new<F>(name: &'static str, title: &'static str, description: &'static str, input_schema: Value, effect: ToolEffect, run: F) -> Self
    where
        F: Fn(ToolContext, Value, AsyncWindowContext) -> LocalBoxFuture<'static, ToolReply> + Send + Sync + 'static,
    {
        Self { name, title, description, input_schema, effect, run: Arc::new(run) }
    }
}

/// A part of `user_context`'s answer (the Tests panel adds the tests that failed).
pub type ContextSection = Arc<dyn Fn(&ToolContext, &Workspace, &App) -> Option<String> + Send + Sync>;

/// What `start_debugging` debugs.
#[derive(Clone, Debug, PartialEq)]
pub enum DebugWhat {
    /// A run target, by name; `None` for the one selected in the title bar.
    Target(Option<String>),
    /// Tests in a file or folder, or whose name contains something.
    Tests { path: Option<PathBuf>, name: Option<String> },
}

/// Starts a debug session (Run registers it); the caller waits for it.
pub type DebugLauncher = Arc<dyn Fn(&ToolContext, DebugWhat, &mut AsyncWindowContext) -> Result<(), String> + Send + Sync>;

#[derive(Default)]
pub struct IdeTools {
    tools: RwLock<Vec<IdeTool>>,
    context: RwLock<Vec<ContextSection>>,
    debug: RwLock<Option<DebugLauncher>>,
}

/// The tools and hooks Forge's crates registered.
pub fn ide_tools() -> &'static IdeTools {
    static TOOLS: OnceLock<IdeTools> = OnceLock::new();
    TOOLS.get_or_init(IdeTools::default)
}

impl IdeTools {
    /// Adds `tool`, replacing one of the same name (`init` may run more than once).
    pub fn register(&self, tool: IdeTool) {
        let mut tools = self.tools.write().unwrap();
        tools.retain(|t| t.name != tool.name);
        tools.push(tool);
    }

    pub fn list(&self) -> Vec<IdeTool> {
        self.tools.read().unwrap().clone()
    }

    pub fn get(&self, name: &str) -> Option<IdeTool> {
        self.tools.read().unwrap().iter().find(|t| t.name == name).cloned()
    }

    /// Adds a part to `user_context` (once per `key`).
    pub fn add_context(&self, key: &'static str, section: ContextSection) {
        static KEYS: OnceLock<RwLock<Vec<&'static str>>> = OnceLock::new();
        let keys = KEYS.get_or_init(Default::default);
        if keys.read().unwrap().contains(&key) {
            return;
        }
        keys.write().unwrap().push(key);
        self.context.write().unwrap().push(section);
    }

    /// The registered parts of `user_context`, in order.
    pub fn context(&self, context: &ToolContext, workspace: &Workspace, cx: &App) -> Vec<String> {
        self.context.read().unwrap().iter().filter_map(|section| section(context, workspace, cx)).collect()
    }

    pub fn set_debug_launcher(&self, launcher: DebugLauncher) {
        *self.debug.write().unwrap() = Some(launcher);
    }

    pub fn debug_launcher(&self) -> Option<DebugLauncher> {
        self.debug.read().unwrap().clone()
    }
}

/// A string argument, trimmed; `None` when missing or empty.
pub fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// A 1-based line argument.
pub fn line_arg(args: &Value, key: &str) -> Option<u32> {
    args.get(key).and_then(Value::as_u64).filter(|l| *l >= 1).map(|l| l as u32)
}

/// The last `lines` lines of `text`, saying how many came before.
pub fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.trim_end().lines().collect();
    match all.len().checked_sub(lines) {
        Some(cut) if cut > 0 => format!("… {cut} earlier lines\n{}", all[cut..].join("\n")),
        _ => all.join("\n"),
    }
}

/// "The window is closed.": what a tool answers when its window went away.
pub fn closed() -> String {
    "The window is closed.".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_paths_compare_whatever_the_case() {
        let temp = Path::new(r"\\?\C:\Users\Ana\AppData\Local\Temp");
        assert!(under(Path::new(r"\\?\c:\users\ana\appdata\local\temp\x.ps1"), temp, true));
        assert!(under(Path::new(r"\\?\C:\Users\Ana\AppData\Local\Temp"), temp, true));
        assert!(!under(Path::new(r"\\?\C:\Users\Ana\AppData\Local\Temporary\x"), temp, true), "a sibling with the same start");
        assert!(!under(Path::new(r"\\?\C:\Windows\x"), temp, true));
        assert!(!under(Path::new("/TMP/x"), Path::new("/tmp"), false), "elsewhere, case counts");
    }

    #[test]
    fn keeps_paths_in_the_workspace() {
        let ide = |outside_ok| ToolContext { project: WeakEntity::new_invalid(), workspace: WeakEntity::new_invalid(), root: "/p/app".into(), roots: vec!["/p/app".into(), "/p/lib".into()], outside_ok };
        assert_eq!(normalize(Path::new("/p/app/./src/../x.rs")), PathBuf::from("/p/app/x.rs"));
        assert_eq!(ide(false).resolve("src/a.rs"), Ok(PathBuf::from("/p/app/src/a.rs")));
        assert_eq!(ide(false).resolve("../lib/b.rs"), Ok(PathBuf::from("/p/lib/b.rs")), "another folder of the workspace");
        assert!(ide(false).resolve("../../etc/passwd").is_err());
        assert!(ide(false).resolve("/etc/passwd").is_err());
        assert!(ide(false).resolve("/p/app/../secret").is_err());
        assert_eq!(ide(true).resolve("/etc/passwd"), Ok(PathBuf::from("/etc/passwd")), "when the user allows it");

        // Scratch files: the folders for temporary files are the agent's too…
        let scratch = std::env::temp_dir().join("forge-scratch.sh");
        assert_eq!(ide(false).resolve(&scratch.to_string_lossy()), Ok(normalize(&scratch)));
        #[cfg(unix)]
        {
            assert!(ide(false).resolve("/tmp/notes.md").is_ok());
            assert!(ide(false).resolve("/tmp/../etc/passwd").is_err());
            // …but not where a link in there leads.
            let dir = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink("/etc", dir.path().join("etc")).unwrap();
            assert!(ide(false).resolve(&dir.path().join("etc/passwd").to_string_lossy()).is_err());
            assert!(ide(false).resolve(&dir.path().join("new.txt").to_string_lossy()).is_ok());
        }
    }

    #[test]
    fn keeps_the_last_lines() {
        assert_eq!(tail("1\n2\n3\n4", 2), "… 2 earlier lines\n3\n4");
        assert_eq!(tail("1\n2", 5), "1\n2");
    }
}
