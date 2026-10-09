//! Contracts between the ACP agent runtime (`acp-client`) and the editor that hosts it.
//!
//! The runtime runs on tokio and knows nothing about GPUI or Zed; the host (forge-agents)
//! implements [`WorkspaceEngine`] and [`TerminalHost`] on top of Zed's project.

mod program;
pub use program::{find_program, program_path};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::sync::broadcast;

#[derive(Debug, Error)]
pub enum IdeError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("service unavailable: {0}")]
    Unavailable(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<std::io::Error> for IdeError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => IdeError::NotFound(e.to_string()),
            _ => IdeError::Io(e.to_string()),
        }
    }
}

pub type IdeResult<T> = Result<T, IdeError>;

/// How to launch an external ACP agent (an entry of `agents.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    Started { agent_id: String },
    /// A JSON-RPC notification from the agent (e.g. `session/update`).
    Notification { agent_id: String, method: String, params: Value },
    /// The agent asked the user to approve a tool call (`session/request_permission`).
    /// Answer it with [`AgentService::respond_permission`].
    PermissionRequested { agent_id: String, request_id: String, params: Value },
    /// The agent created a terminal (`terminal/create`); tool calls reference it by id.
    TerminalCreated { agent_id: String, terminal_id: String },
    Stderr { agent_id: String, line: String },
    Exited { agent_id: String, code: Option<i32> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IdeEvent {
    Agent(AgentEvent),
}

pub type EventBus = broadcast::Sender<IdeEvent>;

/// File access granted to agents (`fs/read_text_file`, `fs/write_text_file`).
#[async_trait]
pub trait WorkspaceEngine: Send + Sync {
    fn root(&self) -> &Path;
    /// Reads a file, preferring the unsaved editor buffer when the file is open.
    async fn read_file(&self, path: &Path) -> IdeResult<String>;
    /// Writes a file, updating the open editor buffer if there is one.
    async fn write_file(&self, path: &Path, text: String) -> IdeResult<()>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalRequest {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    /// Keep at most this many bytes of output (from the end).
    pub output_byte_limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TerminalExit {
    pub exit_code: Option<u32>,
    pub signal: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalOutput {
    pub output: String,
    pub truncated: bool,
    /// Set once the command has finished.
    pub exit: Option<TerminalExit>,
}

/// Terminals for ACP's `terminal/*` client methods.
#[async_trait]
pub trait TerminalHost: Send + Sync {
    /// Starts `request` and returns its terminal id.
    async fn create(&self, request: TerminalRequest) -> IdeResult<String>;
    async fn output(&self, terminal_id: &str) -> IdeResult<TerminalOutput>;
    async fn wait_for_exit(&self, terminal_id: &str) -> IdeResult<TerminalExit>;
    async fn kill(&self, terminal_id: &str) -> IdeResult<()>;
    /// The agent no longer needs the terminal; kill it if running and forget it.
    async fn release(&self, terminal_id: &str) -> IdeResult<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionOutcome {
    Selected {
        #[serde(rename = "optionId")]
        option_id: String,
    },
    Cancelled,
}

#[async_trait]
pub trait AgentService: Send + Sync {
    async fn start(&self, spec: AgentSpec) -> IdeResult<()>;
    async fn stop(&self, agent_id: &str) -> IdeResult<()>;
    fn running(&self) -> Vec<String>;
    async fn rpc(&self, agent_id: &str, method: &str, params: Value) -> IdeResult<Value>;
    async fn initialize(&self, agent_id: &str, protocol_version: u32) -> IdeResult<Value>;
    /// `session/new`. `mcp_servers` are ACP `McpServer` objects the agent should connect to.
    async fn new_session(&self, agent_id: &str, cwd: &Path, mcp_servers: &[Value]) -> IdeResult<Value>;
    /// `session/load`: resume a previous session (the agent replays it as `session/update`s).
    /// Only valid when the agent advertised `agentCapabilities.loadSession`.
    async fn load_session(&self, agent_id: &str, session_id: &str, cwd: &Path, mcp_servers: &[Value]) -> IdeResult<Value>;
    async fn prompt(&self, agent_id: &str, session_id: &str, text: &str) -> IdeResult<Value>;
    /// `session/prompt` with arbitrary ACP content blocks (text, resource_link, resource…).
    async fn prompt_content(&self, agent_id: &str, session_id: &str, content: Vec<Value>) -> IdeResult<Value>;
    async fn cancel(&self, agent_id: &str, session_id: &str) -> IdeResult<()>;
    async fn respond_permission(&self, agent_id: &str, request_id: &str, outcome: PermissionOutcome) -> IdeResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn events_serialize_with_flat_tags() {
        let ev = IdeEvent::Agent(AgentEvent::Exited { agent_id: "a".into(), code: Some(0) });
        assert_eq!(serde_json::to_value(&ev).unwrap(), json!({"type":"agent","event":"exited","agent_id":"a","code":0}));
    }

    #[test]
    fn acp_shapes() {
        let o = PermissionOutcome::Selected { option_id: "allow".into() };
        assert_eq!(serde_json::to_value(&o).unwrap(), json!({"outcome":"selected","optionId":"allow"}));
        assert_eq!(serde_json::to_value(PermissionOutcome::Cancelled).unwrap(), json!({"outcome":"cancelled"}));
        let exit = TerminalExit { exit_code: Some(1), signal: None };
        assert_eq!(serde_json::to_value(&exit).unwrap(), json!({"exitCode":1,"signal":null}));
    }
}
