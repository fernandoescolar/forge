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
}

impl ToolContext {
    pub fn resolve(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    pub fn show(&self, path: &Path) -> String {
        path.strip_prefix(&self.root).unwrap_or(path).to_string_lossy().into_owned()
    }
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
    fn keeps_the_last_lines() {
        assert_eq!(tail("1\n2\n3\n4", 2), "… 2 earlier lines\n3\n4");
        assert_eq!(tail("1\n2", 5), "1\n2");
    }
}
