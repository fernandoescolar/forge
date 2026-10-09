//! `<config>/agents.json`: the ACP agents threads can talk to, which one new threads use,
//! and how their writes are reviewed.

use anyhow::{Context as _, Result};
use ide_api::AgentSpec;
use serde::{Deserialize, Serialize};

const DEFAULT_AGENTS: &str = include_str!("../assets/agents.json");

fn yes() -> bool {
    true
}

/// Where Forge looks for a project's standing instructions for agents, relative to its root:
/// every one that exists is sent (see `rules`), and agents' notes go to the first that does.
pub fn default_instructions_files() -> Vec<String> {
    vec![".forge/AGENTS.md".into(), "AGENTS.md".into()]
}

/// The files listing what agents don't get (see `agent_ignore`): a name with a folder in it is
/// the project's, relative to its root; a bare name counts in every folder, like `.gitignore`.
pub fn default_agent_ignore_files() -> Vec<String> {
    vec![".forge/agentignore".into(), ".agentignore".into()]
}

/// An MCP server offered to every agent session (ACP `McpServer`, stdio transport).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables as an object: `{ "TOKEN": "…" }`.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

impl McpServerConfig {
    pub fn to_acp(&self) -> serde_json::Value {
        let env: Vec<_> = self.env.iter().map(|(k, v)| serde_json::json!({ "name": k, "value": v })).collect();
        serde_json::json!({ "name": self.name, "command": self.command, "args": self.args, "env": env })
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AgentsConfig {
    pub agents: Vec<AgentSpec>,
    /// The agent new threads start with (an `id` from `agents`); the first one if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    /// Show every file write as a diff and wait for approval (unless the same diff was
    /// already approved in a permission request).
    #[serde(default = "yes")]
    pub review_writes: bool,
    /// After a turn that changed files, check them for errors and warnings and offer to
    /// send them back to the agent.
    #[serde(default = "yes")]
    pub verify_changes: bool,
    /// MCP servers passed to agents in `session/new` / `session/load`.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// What agents may do without asking.
    #[serde(default)]
    pub permissions: crate::permissions::Permissions,
    /// The project's instructions files, relative to its root, in order (see `rules`).
    #[serde(default = "default_instructions_files")]
    pub instructions_files: Vec<String>,
    /// The files listing what agents don't get (see `default_agent_ignore_files`).
    #[serde(default = "default_agent_ignore_files")]
    pub agent_ignore_files: Vec<String>,
}

impl AgentsConfig {
    /// Index in `agents` of the agent new threads use.
    pub fn default_index(&self) -> usize {
        self.default_agent.as_ref().and_then(|id| self.agents.iter().position(|a| &a.id == id)).unwrap_or(0)
    }
}

/// Where the user's agents file lives.
pub fn path() -> std::path::PathBuf {
    paths::config_dir().join("agents.json")
}

/// Agents Forge adds in development builds (the mock agents); never written to the file.
pub(crate) fn is_builtin_dev_agent(agent: &AgentSpec) -> bool {
    agent.args.iter().any(|a| a.ends_with("mock-acp-agent.py")) && (agent.id == "mock" || agent.id == "mock-auth")
}

/// The file as Forge writes it: the same explanations as the default file, then the values.
pub fn to_file_text(config: &AgentsConfig) -> String {
    let mut saved = config.clone();
    saved.agents.retain(|a| !is_builtin_dev_agent(a));
    let body = serde_json::to_string_pretty(&serde_json::json!({
        "default_agent": saved.default_agent,
        "review_writes": saved.review_writes,
        "verify_changes": saved.verify_changes,
        "permissions": saved.permissions,
        "instructions_files": saved.instructions_files,
        "agent_ignore_files": saved.agent_ignore_files,
        "mcp_servers": saved.mcp_servers,
        "agents": saved.agents,
    }))
    .unwrap_or_default();
    format!(
        "// ACP agents threads can talk to. Each runs as a subprocess speaking the Agent Client\n\
         // Protocol over stdio and handles its own authentication and models.\n\
         // \"env\" is a list of [name, value] pairs; \"cwd\" defaults to the project root.\n\
         // \"default_agent\": the id new threads start with. \"review_writes\": show every write as\n\
         // a diff and wait for approval. \"verify_changes\": after the agent changes files, check\n\
         // them for errors and warnings. \"permissions\": what agents may do without asking\n\
         // (\"mode\": ask, allow_edits, allow_workspace or super_user; \"allow_commands\": command\n\
         // prefixes that never ask; \"files_outside_workspace\"). \"mcp_servers\": MCP servers\n\
         // every agent session gets. \"instructions_files\": the project's instructions for agents,\n\
         // relative to its root (every one found is sent; agents' notes go to the first found).\n\
         // \"agent_ignore_files\": files listing what agents don't get, in .gitignore's syntax (a\n\
         // name with a folder is relative to the root; a bare name counts in every folder).\n\
         // Forge > Settings > Agents edits this file too.\n{body}\n"
    )
}

/// Writes `config` to the user's agents file.
pub fn save(config: &AgentsConfig) -> Result<()> {
    std::fs::create_dir_all(paths::config_dir())?;
    std::fs::write(path(), to_file_text(config))?;
    Ok(())
}

/// Reads the user's agents file, creating it from the defaults on first run.
pub fn load() -> Result<AgentsConfig> {
    let path = path();
    if !path.exists() {
        std::fs::create_dir_all(paths::config_dir())?;
        std::fs::write(&path, DEFAULT_AGENTS)?;
    }
    let text = std::fs::read_to_string(&path)?;
    let mut config = parse(&text).with_context(|| format!("invalid {}", path.display()))?;
    let agents = &mut config.agents;
    // The mock agent ships with the source tree; offer it in development builds.
    if cfg!(debug_assertions) && !agents.iter().any(|a| a.id == "mock") {
        let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py");
        if mock.exists() {
            let args = vec![mock.to_string_lossy().into_owned()];
            agents.push(AgentSpec { id: "mock".into(), command: "python3".into(), args: args.clone(), env: vec![], cwd: None });
            // Same mock, but it requires signing in (exercises the in-panel login flow).
            let marker = std::env::temp_dir().join("forge-mock-auth").to_string_lossy().into_owned();
            agents.push(AgentSpec {
                id: "mock-auth".into(),
                command: "python3".into(),
                args,
                env: vec![("MOCK_REQUIRE_AUTH".into(), "1".into()), ("MOCK_AUTH_MARKER".into(), marker)],
                cwd: None,
            });
        }
    }
    Ok(config)
}

pub fn parse(text: &str) -> Result<AgentsConfig> {
    let mut config: AgentsConfig = serde_json_lenient::from_str(text)?;
    migrate(&mut config.agents);
    crate::agent_ignore::set_files(config.agent_ignore_files.clone());
    Ok(config)
}

/// npm packages that were renamed upstream; old names stop receiving fixes.
const RENAMED_PACKAGES: &[(&str, &str)] = &[("@zed-industries/claude-code-acp", "@agentclientprotocol/claude-agent-acp")];

fn migrate(agents: &mut [AgentSpec]) {
    for agent in agents {
        for arg in &mut agent.args {
            if let Some((old, new)) = RENAMED_PACKAGES.iter().find(|(old, _)| arg == old || arg.starts_with(&format!("{old}@"))) {
                log::warn!("agent {}: {old} was renamed to {new}; using the new package (update agents.json)", agent.id);
                *arg = (*new).to_string();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn renamed_packages_are_migrated() {
        let config = super::parse(r#"{"agents": [{"id": "claude", "command": "npx", "args": ["-y", "@zed-industries/claude-code-acp@0.16.2"]}]}"#).unwrap();
        assert_eq!(config.agents[0].args, vec!["-y", "@agentclientprotocol/claude-agent-acp"]);
    }

    #[test]
    fn default_agent_round_trips_through_the_file() {
        let mut config = super::parse(super::DEFAULT_AGENTS).unwrap();
        assert_eq!(config.default_index(), 0, "first agent when unset");
        config.default_agent = Some("codex".into());
        config.review_writes = false;
        config.agents.push(ide_api::AgentSpec { id: "mock".into(), command: "python3".into(), args: vec!["/x/tools/mock-acp-agent.py".into()], env: vec![], cwd: None });
        let text = super::to_file_text(&config);
        assert!(text.starts_with("// ACP agents"), "keeps the explanations");
        let back = super::parse(&text).unwrap();
        assert_eq!(back.default_agent.as_deref(), Some("codex"));
        assert_eq!(back.agents[back.default_index()].id, "codex");
        assert!(!back.review_writes);
        assert!(!back.agents.iter().any(|a| a.id == "mock"), "development agents are not written");
    }

    #[test]
    fn default_agents_parse() {
        let config = super::parse(super::DEFAULT_AGENTS).unwrap();
        assert!(config.agents.iter().any(|a| a.id == "claude"));
        assert!(config.review_writes);
        assert!(!super::parse(r#"{"agents": [], "review_writes": false}"#).unwrap().review_writes);
        let with_mcp = super::parse(r#"{"agents": [], "mcp_servers": [{"name": "gh", "command": "gh-mcp", "env": {"TOKEN": "t"}}]}"#).unwrap();
        assert_eq!(
            with_mcp.mcp_servers[0].to_acp(),
            serde_json::json!({"name": "gh", "command": "gh-mcp", "args": [], "env": [{"name": "TOKEN", "value": "t"}]})
        );
    }
}
