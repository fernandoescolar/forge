//! Tools extensions offer agents (`forge.agents.registerTool`). The extension host keeps
//! them here, and Forge's MCP server (crates/forge-agents) lists them to agents and hands
//! their calls back; neither crate depends on the other. Read from any thread: the MCP
//! server answers on tokio.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use futures::channel::oneshot;
use serde_json::Value;

/// One tool, as an extension registered it.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentTool {
    /// Its MCP name: the extension's id and the tool's name (`db_explorer__query`), so no
    /// extension can take Forge's tools' names or another extension's.
    pub name: String,
    /// The name the extension gave it.
    pub tool: String,
    pub extension: String,
    pub title: String,
    pub description: String,
    pub input_schema: Value,
    /// Only reads: it runs without asking the user.
    pub read_only: bool,
}

/// What a call answers the agent: text, and whether it is an error.
pub type AgentToolReply = Result<String, String>;

/// Runs extension tools (the extension host).
pub trait AgentToolRunner: Send + Sync {
    /// Runs `tool` with `args` for an agent working in `cwd`.
    fn run(&self, tool: &AgentTool, args: Value, cwd: PathBuf) -> oneshot::Receiver<AgentToolReply>;
}

#[derive(Default)]
pub struct AgentTools {
    tools: RwLock<Vec<AgentTool>>,
    runner: RwLock<Option<Arc<dyn AgentToolRunner>>>,
}

/// The tools of every loaded extension.
pub fn agent_tools() -> &'static AgentTools {
    static TOOLS: OnceLock<AgentTools> = OnceLock::new();
    TOOLS.get_or_init(AgentTools::default)
}

/// The MCP name of `tool` from `extension`: letters, digits and `_` only (MCP clients
/// accept little else), at most 64 characters.
pub fn mcp_name(extension: &str, tool: &str) -> String {
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect::<String>();
    let name = format!("{}__{}", clean(extension), clean(tool));
    name.chars().take(64).collect()
}

impl AgentTools {
    pub fn list(&self) -> Vec<AgentTool> {
        self.tools.read().unwrap().clone()
    }

    pub fn get(&self, name: &str) -> Option<AgentTool> {
        self.tools.read().unwrap().iter().find(|t| t.name == name).cloned()
    }

    /// Adds `tool`, replacing one of the same name.
    pub fn register(&self, tool: AgentTool) {
        let mut tools = self.tools.write().unwrap();
        tools.retain(|t| t.name != tool.name);
        tools.push(tool);
    }

    pub fn unregister(&self, name: &str) {
        self.tools.write().unwrap().retain(|t| t.name != name);
    }

    /// Takes away every tool of `extension` (it unloaded).
    pub fn unregister_extension(&self, extension: &str) {
        self.tools.write().unwrap().retain(|t| t.extension != extension);
    }

    pub fn set_runner(&self, runner: Arc<dyn AgentToolRunner>) {
        *self.runner.write().unwrap() = Some(runner);
    }

    /// Runs the tool named `name` (its MCP name).
    pub async fn run(&self, name: &str, args: Value, cwd: PathBuf) -> AgentToolReply {
        let tool = self.get(name).ok_or_else(|| format!("Unknown tool: {name}"))?;
        let runner = self.runner.read().unwrap().clone().ok_or("Extensions aren't running.")?;
        runner.run(&tool, args, cwd).await.unwrap_or_else(|_| Err(format!("{} stopped before answering.", tool.extension)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_tools_by_extension() {
        assert_eq!(mcp_name("db-explorer", "query"), "db_explorer__query");
        assert_eq!(mcp_name("acme.tools", "find-users"), "acme_tools__find_users");
        assert_eq!(mcp_name(&"x".repeat(80), "t").len(), 64);
    }

    #[test]
    fn keeps_one_tool_per_name() {
        let tools = AgentTools::default();
        let tool = |name: &str, extension: &str| AgentTool {
            name: mcp_name(extension, name),
            tool: name.into(),
            extension: extension.into(),
            title: name.into(),
            description: String::new(),
            input_schema: Value::Null,
            read_only: true,
        };
        tools.register(tool("query", "db"));
        tools.register(tool("query", "db"));
        tools.register(tool("schema", "db"));
        tools.register(tool("lint", "other"));
        assert_eq!(tools.list().len(), 3);
        tools.unregister_extension("db");
        assert_eq!(tools.list().iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["other__lint"]);
    }
}
