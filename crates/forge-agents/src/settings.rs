//! The agents configuration as the app sees it: loaded once from `agents.json`, edited
//! from the Agents settings page (which saves the file), observed by everything that
//! creates threads or lists agents.

use std::path::PathBuf;

use gpui::{App, AppContext as _, Context, Entity, Global};
use ide_api::AgentSpec;

use crate::config::AgentsConfig;

pub struct AgentSettings {
    config: AgentsConfig,
    /// Why `agents.json` couldn't be read, if it couldn't.
    error: Option<String>,
    /// Where conversations are saved.
    history_dir: PathBuf,
    /// Saved edits go to `agents.json`; tests keep them in memory.
    persist: bool,
}

struct GlobalAgentSettings(Entity<AgentSettings>);
impl Global for GlobalAgentSettings {}

pub fn init(cx: &mut App) {
    if cx.has_global::<GlobalAgentSettings>() {
        return;
    }
    let (config, error) = match crate::config::load() {
        Ok(config) => (config, None),
        Err(e) => {
            log::error!("{e:#}");
            (empty(), Some(format!("{e:#}")))
        }
    };
    let settings = cx.new(|_| AgentSettings { config, error, history_dir: paths::data_dir().join("agent-sessions"), persist: true });
    cx.set_global(GlobalAgentSettings(settings.clone()));
    register_settings_page(&settings, cx);
}

/// The Agents page of the Settings tab, kept in step with the agents list both ways.
fn register_settings_page(settings: &Entity<AgentSettings>, cx: &mut App) {
    use forge_ui::settings_registry::{SettingChanged, SettingsFile, SettingsRegistry};
    let registry = SettingsRegistry::global(cx);
    let update = |settings: &Entity<AgentSettings>, registry: &Entity<SettingsRegistry>, cx: &mut App| {
        let page = settings_page(settings.read(cx).config());
        registry.update(cx, |registry, cx| registry.register(page, cx));
    };
    update(settings, &registry, cx);
    let file = SettingsFile::Config("agents.json".into());
    let weak = settings.downgrade();
    cx.subscribe(&registry, move |_, event: &SettingChanged, cx| {
        if event.file == file {
            weak.update(cx, |settings, cx| settings.reload(cx)).ok();
        }
    })
    .detach();
    // A new agent, a removed one: the default-agent choices change.
    let mut agent_ids: Vec<String> = settings.read(cx).agents().iter().map(|a| a.id.clone()).collect();
    cx.observe(settings, move |settings, cx| {
        let ids: Vec<String> = settings.read(cx).agents().iter().map(|a| a.id.clone()).collect();
        if ids != agent_ids {
            agent_ids = ids;
            let registry = SettingsRegistry::global(cx);
            update(&settings, &registry, cx);
        }
    })
    .detach();
}

fn settings_page(config: &AgentsConfig) -> forge_ui::settings_registry::SettingsPage {
    use crate::permissions::PermissionMode;
    use serde_json::json;
    let ids: Vec<&str> = config.agents.iter().map(|a| a.id.as_str()).collect();
    let modes: Vec<serde_json::Value> = PermissionMode::ALL.iter().map(|m| serde_json::to_value(m).unwrap_or_default()).collect();
    let mode_labels: Vec<&str> = PermissionMode::ALL.iter().map(|m| m.label()).collect();
    let schema = json!({
        "properties": {
            "default_agent": { "enum": ids, "title": "Default agent", "description": "The agent new threads start with." },
            "review_writes": { "type": "boolean", "title": "Review changes before writing", "description": "Show every file the agent writes as a diff and wait for approval (unless you already approved the same change)." },
            "verify_changes": { "type": "boolean", "title": "Check the changed files", "description": "After a turn that changed files, list their errors and warnings in the thread, with a button to send them back to the agent." },
            "permissions": {
                "type": "object",
                "title": "Permissions",
                "properties": {
                    "mode": { "enum": modes, "enumDescriptions": mode_labels, "title": "What agents may do without asking", "description": "Requests the mode doesn't cover still ask. A thread can switch modes for itself." },
                    "files_outside_workspace": { "type": "boolean", "title": "Files outside the workspace", "description": "Agents may read and write files outside the project's folders." },
                    "allow_commands": { "type": "array", "title": "Commands that never ask", "description": "By prefix, such as `dotnet build` or `git status`." }
                }
            },
            "instructions_files": { "type": "array", "title": "Project instructions files", "description": "The project's instructions for agents, relative to its root, sent with every new session's first message (all that exist). Agents' notes go to the first that exists." },
            "agent_ignore_files": { "type": "array", "title": "Files agents don't get", "description": "Files listing what agents may not read or change, in .gitignore's syntax. A name with a folder (`.forge/agentignore`) is relative to the project's root; a bare name (`.agentignore`) counts in every folder, for the files below it." },
            "mcp_servers": { "type": "array", "title": "MCP servers", "description": "Tool servers every agent session gets: name, command, args and env." },
            "agents": { "type": "array", "title": "Agents", "description": "The ACP agents threads can talk to. Manage agents adds them from a list." }
        }
    });
    let defaults = json!({
        "default_agent": ids.first(),
        "review_writes": true,
        "verify_changes": true,
        "instructions_files": crate::config::default_instructions_files(),
        "agent_ignore_files": crate::config::default_agent_ignore_files(),
        "permissions": { "mode": serde_json::to_value(PermissionMode::default()).unwrap_or_default(), "files_outside_workspace": false },
    });
    forge_ui::settings_registry::SettingsPage {
        id: "agents".into(),
        title: "Agents".into(),
        file: forge_ui::settings_registry::SettingsFile::Config("agents.json".into()),
        schema,
        keys: None,
        defaults,
        order: 10,
        actions: vec![("Manage Agents…".into(), "forge_agent::OpenAgentSettings".into())],
    }
}

/// Settings that never touch the user's files (tests, embedders).
pub fn set_in_memory(config: AgentsConfig, history_dir: PathBuf, cx: &mut App) {
    crate::agent_ignore::set_files(config.agent_ignore_files.clone());
    let settings = cx.new(|_| AgentSettings { config, error: None, history_dir, persist: false });
    cx.set_global(GlobalAgentSettings(settings));
}

pub fn try_global(cx: &App) -> Option<Entity<AgentSettings>> {
    cx.try_global::<GlobalAgentSettings>().map(|g| g.0.clone())
}

pub fn global(cx: &App) -> Entity<AgentSettings> {
    cx.global::<GlobalAgentSettings>().0.clone()
}

fn empty() -> AgentsConfig {
    AgentsConfig { agent_ignore_files: crate::config::default_agent_ignore_files(), instructions_files: crate::config::default_instructions_files(), agents: vec![], default_agent: None, review_writes: true, verify_changes: true, mcp_servers: vec![], permissions: Default::default() }
}

impl AgentSettings {
    pub fn config(&self) -> &AgentsConfig {
        &self.config
    }

    pub fn agents(&self) -> &[AgentSpec] {
        &self.config.agents
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn history_dir(&self) -> &PathBuf {
        &self.history_dir
    }

    pub fn default_index(&self) -> usize {
        self.config.default_index()
    }

    pub fn default_agent(&self) -> Option<&AgentSpec> {
        self.config.agents.get(self.default_index())
    }

    pub fn set_default(&mut self, id: &str, cx: &mut Context<Self>) {
        self.config.default_agent = Some(id.to_string());
        self.save(cx);
    }

    pub fn set_permissions(&mut self, permissions: crate::permissions::Permissions, cx: &mut Context<Self>) {
        self.config.permissions = permissions;
        self.save(cx);
    }

    pub fn set_review_writes(&mut self, review: bool, cx: &mut Context<Self>) {
        self.config.review_writes = review;
        self.save(cx);
    }

    /// Adds `agent`, replacing one with the same id.
    pub fn upsert_agent(&mut self, agent: AgentSpec, cx: &mut Context<Self>) {
        match self.config.agents.iter_mut().find(|a| a.id == agent.id) {
            Some(existing) => *existing = agent,
            None => self.config.agents.push(agent),
        }
        self.save(cx);
    }

    pub fn remove_agent(&mut self, id: &str, cx: &mut Context<Self>) {
        self.config.agents.retain(|a| a.id != id);
        if self.config.default_agent.as_deref() == Some(id) {
            self.config.default_agent = None;
        }
        self.save(cx);
    }

    /// Re-reads `agents.json` (after editing it by hand).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if !self.persist {
            return;
        }
        match crate::config::load() {
            Ok(config) => {
                self.config = config;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if self.persist {
            if let Err(e) = crate::config::save(&self.config) {
                self.error = Some(format!("Could not save agents.json: {e:#}"));
            } else {
                self.error = None;
            }
        }
        cx.notify();
    }
}

/// Agents Forge knows how to start, offered when adding one.
pub fn presets() -> Vec<AgentSpec> {
    let npx = |id: &str, package: &str| AgentSpec { id: id.into(), command: "npx".into(), args: vec!["-y".into(), package.into()], env: vec![], cwd: None };
    vec![
        npx("claude", "@agentclientprotocol/claude-agent-acp"),
        npx("codex", "@zed-industries/codex-acp"),
        AgentSpec { id: "gemini".into(), command: "gemini".into(), args: vec!["--experimental-acp".into()], env: vec![], cwd: None },
        AgentSpec { id: "copilot".into(), command: "npx".into(), args: ["-y", "@github/copilot", "--acp", "--stdio"].map(String::from).to_vec(), env: vec![], cwd: None },
    ]
}
