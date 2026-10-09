//! Runtime for external ACP (Agent Client Protocol) agents speaking newline-delimited
//! JSON-RPC over stdio. Forge acts as the ACP *client*: it serves file reads/writes from
//! the workspace (including unsaved buffers), runs terminals for the agent through a
//! [`TerminalHost`], and relays permission requests to the UI.

use async_trait::async_trait;
use dashmap::DashMap;
use ide_api::*;
use jsonrpc::{Framing, Handler, Peer, RpcError};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, oneshot},
};
use tracing::{info, warn};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 1;
/// Prompts may legitimately run for a long time (the agent is doing real work).
const PROMPT_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

type PermissionWaiters = DashMap<(String, String), oneshot::Sender<PermissionOutcome>>;

struct AgentProcess {
    peer: Arc<Peer>,
    child: Mutex<Child>,
}

struct AcpHandler {
    agent_id: String,
    events: EventBus,
    workspace: Arc<dyn WorkspaceEngine>,
    terminals: Option<Arc<dyn TerminalHost>>,
    permissions: Arc<PermissionWaiters>,
}

impl AcpHandler {
    fn terminals(&self) -> Result<&Arc<dyn TerminalHost>, RpcError> {
        self.terminals.as_ref().ok_or_else(|| RpcError::method_not_found("terminal/*"))
    }

    fn terminal_id(params: &Value) -> Result<String, RpcError> {
        params.get("terminalId").and_then(Value::as_str).map(str::to_string).ok_or_else(|| RpcError::invalid_params("missing `terminalId`"))
    }

    async fn create_terminal(&self, params: Value) -> Result<Value, RpcError> {
        let command = params.get("command").and_then(Value::as_str).ok_or_else(|| RpcError::invalid_params("missing `command`"))?.to_string();
        let strings = |v: Option<&Value>| -> Vec<String> { v.and_then(Value::as_array).map(|xs| xs.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default() };
        let env = params
            .get("env")
            .and_then(Value::as_array)
            .map(|xs| xs.iter().filter_map(|e| Some((e.get("name")?.as_str()?.to_string(), e.get("value")?.as_str()?.to_string()))).collect())
            .unwrap_or_default();
        let request = TerminalRequest {
            command,
            args: strings(params.get("args")),
            env,
            cwd: params.get("cwd").and_then(Value::as_str).map(PathBuf::from).or_else(|| Some(self.workspace.root().to_path_buf())),
            output_byte_limit: params.get("outputByteLimit").and_then(Value::as_u64).map(|n| n as usize),
        };
        let terminal_id = self.terminals()?.create(request).await?;
        let _ = self.events.send(IdeEvent::Agent(AgentEvent::TerminalCreated { agent_id: self.agent_id.clone(), terminal_id: terminal_id.clone() }));
        Ok(json!({ "terminalId": terminal_id }))
    }

    async fn terminal_output(&self, params: Value) -> Result<Value, RpcError> {
        let out = self.terminals()?.output(&Self::terminal_id(&params)?).await?;
        let mut result = json!({ "output": out.output, "truncated": out.truncated });
        if let Some(exit) = out.exit {
            result["exitStatus"] = json!(exit);
        }
        Ok(result)
    }
    fn path_param(params: &Value) -> Result<PathBuf, RpcError> {
        params.get("path").and_then(Value::as_str).map(PathBuf::from).ok_or_else(|| RpcError::invalid_params("missing `path`"))
    }

    async fn read_text_file(&self, params: Value) -> Result<Value, RpcError> {
        let text = self.workspace.read_file(&Self::path_param(&params)?).await?;
        // ACP lines are 1-based.
        let line = params.get("line").and_then(Value::as_u64).map(|l| l.max(1) as usize);
        let limit = params.get("limit").and_then(Value::as_u64).map(|l| l as usize);
        let content = if line.is_some() || limit.is_some() {
            let lines = text.split_inclusive('\n').skip(line.unwrap_or(1) - 1);
            match limit {
                Some(n) => lines.take(n).collect(),
                None => lines.collect(),
            }
        } else {
            text
        };
        Ok(json!({ "content": content }))
    }

    async fn write_text_file(&self, params: Value) -> Result<Value, RpcError> {
        let content = params.get("content").and_then(Value::as_str).ok_or_else(|| RpcError::invalid_params("missing `content`"))?;
        self.workspace.write_file(&Self::path_param(&params)?, content.to_string()).await?;
        Ok(Value::Null)
    }

    async fn request_permission(&self, params: Value) -> Result<Value, RpcError> {
        let request_id = Uuid::new_v4().to_string();
        let key = (self.agent_id.clone(), request_id.clone());
        let (tx, rx) = oneshot::channel();
        self.permissions.insert(key.clone(), tx);
        let _ = self.events.send(IdeEvent::Agent(AgentEvent::PermissionRequested { agent_id: self.agent_id.clone(), request_id, params }));
        let outcome = match tokio::time::timeout(PERMISSION_TIMEOUT, rx).await {
            Ok(Ok(outcome)) => outcome,
            _ => {
                self.permissions.remove(&key);
                PermissionOutcome::Cancelled
            }
        };
        Ok(json!({ "outcome": outcome }))
    }
}

#[async_trait]
impl Handler for AcpHandler {
    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "fs/read_text_file" => self.read_text_file(params).await,
            "fs/write_text_file" => self.write_text_file(params).await,
            "session/request_permission" => self.request_permission(params).await,
            "terminal/create" => self.create_terminal(params).await,
            "terminal/output" => self.terminal_output(params).await,
            "terminal/wait_for_exit" => Ok(json!(self.terminals()?.wait_for_exit(&Self::terminal_id(&params)?).await?)),
            "terminal/kill" => {
                self.terminals()?.kill(&Self::terminal_id(&params)?).await?;
                Ok(json!({}))
            }
            "terminal/release" => {
                self.terminals()?.release(&Self::terminal_id(&params)?).await?;
                Ok(json!({}))
            }
            other => Err(RpcError::method_not_found(other)),
        }
    }

    async fn notification(&self, method: &str, params: Value) {
        let _ = self.events.send(IdeEvent::Agent(AgentEvent::Notification { agent_id: self.agent_id.clone(), method: method.to_string(), params }));
    }
}

pub struct AcpRuntime {
    events: EventBus,
    workspace: Arc<dyn WorkspaceEngine>,
    terminals: Option<Arc<dyn TerminalHost>>,
    /// The host can run an agent's login command in an interactive terminal.
    terminal_auth: bool,
    display_terminals: bool,
    agents: Arc<DashMap<String, Arc<AgentProcess>>>,
    permissions: Arc<PermissionWaiters>,
}

impl AcpRuntime {
    pub fn new(events: EventBus, workspace: Arc<dyn WorkspaceEngine>) -> Self {
        Self { events, workspace, terminals: None, terminal_auth: false, display_terminals: false, agents: Default::default(), permissions: Default::default() }
    }

    /// Lets agents run commands (`terminal/*`); advertised in `initialize` only when set.
    pub fn with_terminals(mut self, terminals: Arc<dyn TerminalHost>) -> Self {
        self.terminals = Some(terminals);
        self
    }

    /// Advertise interactive terminal login (`auth.terminal`, `_meta["terminal-auth"]`), so
    /// agents describe their login commands in `authMethods`.
    pub fn with_terminal_auth(mut self) -> Self {
        self.terminal_auth = true;
        self
    }

    /// Advertise that the host shows the output of commands agents run themselves
    /// (`_meta["terminal_output"]`): tool calls then carry `_meta.terminal_info`,
    /// `terminal_output` and `terminal_exit` instead of the output as text.
    pub fn with_display_terminals(mut self) -> Self {
        self.display_terminals = true;
        self
    }

    fn agent(&self, agent_id: &str) -> IdeResult<Arc<AgentProcess>> {
        self.agents.get(agent_id).map(|a| a.clone()).ok_or_else(|| IdeError::NotFound(format!("agent {agent_id} is not running")))
    }

    pub async fn shutdown(&self) {
        let ids: Vec<_> = self.agents.iter().map(|a| a.key().clone()).collect();
        for id in ids {
            let _ = self.stop(&id).await;
        }
    }
}

#[async_trait]
impl AgentService for AcpRuntime {
    async fn start(&self, spec: AgentSpec) -> IdeResult<()> {
        if self.agents.contains_key(&spec.id) {
            return Err(IdeError::InvalidInput(format!("agent {} already started", spec.id)));
        }
        // The agent's own PATH if it sets one, else Forge's: `npx` is `npx.cmd` on Windows.
        let path = spec.env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| std::ffi::OsString::from(v));
        let mut cmd = Command::new(ide_api::program_path(&spec.command, path.as_deref()));
        cmd.args(&spec.args)
            .current_dir(spec.cwd.as_deref().unwrap_or(self.workspace.root()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().map_err(|e| IdeError::Unavailable(format!("failed to start agent {} ({}): {e}", spec.id, spec.command)))?;
        let stdin = child.stdin.take().ok_or_else(|| IdeError::Internal("agent stdin unavailable".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| IdeError::Internal("agent stdout unavailable".into()))?;
        if let Some(stderr) = child.stderr.take() {
            let (events, agent_id) = (self.events.clone(), spec.id.clone());
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = events.send(IdeEvent::Agent(AgentEvent::Stderr { agent_id: agent_id.clone(), line }));
                }
            });
        }

        let handler = Arc::new(AcpHandler { agent_id: spec.id.clone(), events: self.events.clone(), workspace: self.workspace.clone(), terminals: self.terminals.clone(), permissions: self.permissions.clone() });
        let (peer, reader) = Peer::start(stdout, stdin, Framing::Newline, handler, Duration::from_secs(120));
        let process = Arc::new(AgentProcess { peer, child: Mutex::new(child) });
        self.agents.insert(spec.id.clone(), process.clone());
        info!(agent = %spec.id, "agent started");
        let _ = self.events.send(IdeEvent::Agent(AgentEvent::Started { agent_id: spec.id.clone() }));

        // Reap the process once its stdout closes, whether it crashed or was stopped.
        let (agents, permissions, events, agent_id) = (self.agents.clone(), self.permissions.clone(), self.events.clone(), spec.id);
        tokio::spawn(async move {
            let _ = reader.await;
            let code = {
                let mut child = process.child.lock().await;
                match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
                    Ok(Ok(status)) => status.code(),
                    _ => {
                        let _ = child.kill().await;
                        None
                    }
                }
            };
            agents.remove_if(&agent_id, |_, p| Arc::ptr_eq(p, &process));
            permissions.retain(|(a, _), _| a != &agent_id);
            info!(agent = %agent_id, ?code, "agent exited");
            let _ = events.send(IdeEvent::Agent(AgentEvent::Exited { agent_id, code }));
        });
        Ok(())
    }

    async fn stop(&self, agent_id: &str) -> IdeResult<()> {
        let agent = self.agent(agent_id)?;
        if let Err(e) = agent.child.lock().await.start_kill() {
            warn!(agent = %agent_id, "kill failed: {e}");
        }
        Ok(())
    }

    fn running(&self) -> Vec<String> {
        let mut ids: Vec<_> = self.agents.iter().map(|a| a.key().clone()).collect();
        ids.sort();
        ids
    }

    async fn rpc(&self, agent_id: &str, method: &str, params: Value) -> IdeResult<Value> {
        self.agent(agent_id)?.peer.request(method, params).await
    }

    async fn initialize(&self, agent_id: &str, protocol_version: u32) -> IdeResult<Value> {
        self.rpc(
            agent_id,
            "initialize",
            json!({
                "protocolVersion": protocol_version,
                "clientCapabilities": {
                    "fs": {"readTextFile": true, "writeTextFile": true},
                    "terminal": self.terminals.is_some(),
                    "auth": {"terminal": self.terminal_auth},
                    "_meta": {"terminal-auth": self.terminal_auth, "terminal_output": self.display_terminals},
                },
                "clientInfo": {"name": "Forge IDE", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await
    }

    async fn new_session(&self, agent_id: &str, cwd: &Path, mcp_servers: &[Value]) -> IdeResult<Value> {
        self.rpc(agent_id, "session/new", json!({"cwd": cwd.to_string_lossy(), "mcpServers": mcp_servers})).await
    }

    async fn load_session(&self, agent_id: &str, session_id: &str, cwd: &Path, mcp_servers: &[Value]) -> IdeResult<Value> {
        self.agent(agent_id)?
            .peer
            .request_with_timeout("session/load", json!({"sessionId": session_id, "cwd": cwd.to_string_lossy(), "mcpServers": mcp_servers}), PROMPT_TIMEOUT)
            .await
    }

    async fn prompt(&self, agent_id: &str, session_id: &str, text: &str) -> IdeResult<Value> {
        self.prompt_content(agent_id, session_id, vec![json!({"type": "text", "text": text})]).await
    }

    async fn prompt_content(&self, agent_id: &str, session_id: &str, content: Vec<Value>) -> IdeResult<Value> {
        self.agent(agent_id)?.peer.request_with_timeout("session/prompt", json!({"sessionId": session_id, "prompt": content}), PROMPT_TIMEOUT).await
    }

    async fn cancel(&self, agent_id: &str, session_id: &str) -> IdeResult<()> {
        // Per ACP, pending permission requests must resolve as cancelled when the turn is cancelled.
        let keys: Vec<_> = self.permissions.iter().filter(|e| e.key().0 == agent_id).map(|e| e.key().clone()).collect();
        for key in keys {
            if let Some((_, tx)) = self.permissions.remove(&key) {
                let _ = tx.send(PermissionOutcome::Cancelled);
            }
        }
        self.agent(agent_id)?.peer.notify("session/cancel", json!({"sessionId": session_id})).await
    }

    async fn respond_permission(&self, agent_id: &str, request_id: &str, outcome: PermissionOutcome) -> IdeResult<()> {
        let (_, tx) = self
            .permissions
            .remove(&(agent_id.to_string(), request_id.to_string()))
            .ok_or_else(|| IdeError::NotFound(format!("permission request {request_id}")))?;
        tx.send(outcome).map_err(|_| IdeError::Unavailable("agent is no longer waiting for this permission".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::broadcast::Receiver;

    /// Files on disk under a temp root, plus "open buffers" that shadow them, like an editor.
    struct TestWorkspace {
        root: PathBuf,
        buffers: std::sync::Mutex<HashMap<PathBuf, String>>,
    }

    impl TestWorkspace {
        fn abs(&self, path: &Path) -> IdeResult<PathBuf> {
            let p = if path.is_absolute() { path.to_path_buf() } else { self.root.join(path) };
            if p.starts_with(&self.root) { Ok(p) } else { Err(IdeError::InvalidInput("outside workspace".into())) }
        }
    }

    #[async_trait]
    impl WorkspaceEngine for TestWorkspace {
        fn root(&self) -> &Path {
            &self.root
        }
        async fn read_file(&self, path: &Path) -> IdeResult<String> {
            let p = self.abs(path)?;
            if let Some(text) = self.buffers.lock().unwrap().get(&p) {
                return Ok(text.clone());
            }
            Ok(std::fs::read_to_string(p)?)
        }
        async fn write_file(&self, path: &Path, text: String) -> IdeResult<()> {
            let p = self.abs(path)?;
            std::fs::write(&p, &text)?;
            self.buffers.lock().unwrap().entry(p).and_modify(|b| *b = text);
            Ok(())
        }
    }

    /// Runs terminal commands as plain processes and captures their output.
    #[derive(Default)]
    struct ProcessTerminals {
        done: tokio::sync::Mutex<HashMap<String, (String, Option<i32>)>>,
        requests: std::sync::Mutex<Vec<TerminalRequest>>,
    }

    #[async_trait]
    impl TerminalHost for ProcessTerminals {
        async fn create(&self, request: TerminalRequest) -> IdeResult<String> {
            let out = Command::new(&request.command).args(&request.args).current_dir(request.cwd.clone().unwrap()).output().await?;
            let id = format!("term-{}", self.requests.lock().unwrap().len());
            let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
            self.done.lock().await.insert(id.clone(), (text, out.status.code()));
            self.requests.lock().unwrap().push(request);
            Ok(id)
        }
        async fn output(&self, id: &str) -> IdeResult<TerminalOutput> {
            let done = self.done.lock().await;
            let (text, code) = done.get(id).ok_or_else(|| IdeError::NotFound(id.into()))?;
            Ok(TerminalOutput { output: text.clone(), truncated: false, exit: Some(TerminalExit { exit_code: code.map(|c| c as u32), signal: None }) })
        }
        async fn wait_for_exit(&self, id: &str) -> IdeResult<TerminalExit> {
            Ok(self.output(id).await?.exit.unwrap())
        }
        async fn kill(&self, _: &str) -> IdeResult<()> {
            Ok(())
        }
        async fn release(&self, id: &str) -> IdeResult<()> {
            self.done.lock().await.remove(id);
            Ok(())
        }
    }

    fn mock_agent() -> AgentSpec {
        AgentSpec {
            id: "mock".into(),
            command: "python3".into(),
            args: vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
            env: vec![],
            cwd: None,
        }
    }

    async fn next_agent_event(rx: &mut Receiver<IdeEvent>, pred: impl Fn(&AgentEvent) -> bool) -> AgentEvent {
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("timed out").unwrap();
            let IdeEvent::Agent(a) = ev;
            if pred(&a) {
                return a;
            }
        }
    }

    struct Setup {
        dir: tempfile::TempDir,
        acp: Arc<AcpRuntime>,
        ws: Arc<TestWorkspace>,
        terminals: Arc<ProcessTerminals>,
        rx: Receiver<IdeEvent>,
        session: String,
        init: Value,
    }

    async fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "line one\nline two\n").unwrap();
        let (events, rx) = tokio::sync::broadcast::channel(256);
        let ws = Arc::new(TestWorkspace { root: dir.path().canonicalize().unwrap(), buffers: Default::default() });
        let terminals = Arc::new(ProcessTerminals::default());
        let acp = Arc::new(AcpRuntime::new(events, ws.clone()).with_terminals(terminals.clone()));
        acp.start(mock_agent()).await.unwrap();
        let init = acp.initialize("mock", PROTOCOL_VERSION).await.unwrap();
        let session = acp.new_session("mock", ws.root(), &[]).await.unwrap()["sessionId"].as_str().unwrap().to_string();
        Setup { dir, acp, ws, terminals, rx, session, init }
    }

    #[tokio::test]
    async fn prompt_streams_session_updates() {
        let Setup { dir: _dir, acp, mut rx, session, init, .. } = setup().await;
        assert_eq!(init["protocolVersion"], 1);
        let result = acp.prompt("mock", &session, "hello").await.unwrap();
        assert_eq!(result["stopReason"], "end_turn");
        // The agent's message (its command list comes first, as Claude's does).
        let ev = next_agent_event(&mut rx, |e| matches!(e, AgentEvent::Notification { params, .. } if params["update"]["sessionUpdate"] == "agent_message_chunk")).await;
        let AgentEvent::Notification { method, params, .. } = ev else { unreachable!() };
        assert_eq!(method, "session/update");
        assert_eq!(params["update"]["content"]["text"], "Echo: hello");
        acp.stop("mock").await.unwrap();
        next_agent_event(&mut rx, |e| matches!(e, AgentEvent::Exited { .. })).await;
        assert!(acp.running().is_empty());
    }

    #[tokio::test]
    async fn agent_reads_unsaved_buffer_through_workspace() {
        let Setup { dir: _dir, acp, ws, mut rx, session, .. } = setup().await;
        ws.buffers.lock().unwrap().insert(ws.root.join("notes.txt"), "unsaved edit\n".into());
        acp.prompt("mock", &session, "read notes.txt").await.unwrap();
        let ev = next_agent_event(&mut rx, |e| matches!(e, AgentEvent::Notification { params, .. } if params.to_string().contains("Read:"))).await;
        assert!(format!("{ev:?}").contains("unsaved edit"), "{ev:?}");
    }

    #[tokio::test]
    async fn write_requires_permission() {
        let Setup { dir, acp, mut rx, session, .. } = setup().await;
        for (choice, expect_written) in [("reject", false), ("allow", true)] {
            let pending = tokio::spawn({
                let (acp, session) = (acp.clone(), session.clone());
                async move { acp.prompt("mock", &session, "write out.txt hi").await }
            });
            let AgentEvent::PermissionRequested { request_id, params, .. } = next_agent_event(&mut rx, |e| matches!(e, AgentEvent::PermissionRequested { .. })).await else { unreachable!() };
            assert_eq!(params["options"][0]["optionId"], "allow");
            acp.respond_permission("mock", &request_id, PermissionOutcome::Selected { option_id: choice.into() }).await.unwrap();
            pending.await.unwrap().unwrap();
            assert_eq!(dir.path().join("out.txt").exists(), expect_written, "{choice}");
        }
        assert_eq!(std::fs::read_to_string(dir.path().join("out.txt")).unwrap(), "hi");
    }

    #[tokio::test]
    async fn cancel_resolves_pending_permission() {
        let Setup { dir: _dir, acp, mut rx, session, .. } = setup().await;
        let pending = tokio::spawn({
            let (acp, session) = (acp.clone(), session.clone());
            async move { acp.prompt("mock", &session, "write out.txt hi").await }
        });
        next_agent_event(&mut rx, |e| matches!(e, AgentEvent::PermissionRequested { .. })).await;
        acp.cancel("mock", &session).await.unwrap();
        assert_eq!(pending.await.unwrap().unwrap()["stopReason"], "cancelled");
    }

    #[tokio::test]
    async fn agent_runs_commands_in_host_terminals() {
        // Keep `dir` alive: dropping the TempDir deletes the agent's working directory.
        let Setup { dir: _dir, acp, ws, terminals, mut rx, session, init } = setup().await;
        assert_eq!(init["echoClientCapabilities"]["terminal"], true, "capability advertised");
        let pending = tokio::spawn({
            let (acp, session) = (acp.clone(), session.clone());
            async move { acp.prompt("mock", &session, "run echo forged; exit 3").await }
        });
        let AgentEvent::PermissionRequested { request_id, .. } = next_agent_event(&mut rx, |e| matches!(e, AgentEvent::PermissionRequested { .. })).await else { unreachable!() };
        acp.respond_permission("mock", &request_id, PermissionOutcome::Selected { option_id: "allow".into() }).await.unwrap();
        let AgentEvent::TerminalCreated { terminal_id, .. } = next_agent_event(&mut rx, |e| matches!(e, AgentEvent::TerminalCreated { .. })).await else { unreachable!() };
        assert_eq!(terminal_id, "term-0");
        pending.await.unwrap().unwrap();
        let reply = next_agent_event(&mut rx, |e| matches!(e, AgentEvent::Notification { params, .. } if params.to_string().contains("Exit code"))).await;
        let text = format!("{reply:?}");
        assert!(text.contains("forged") && text.contains("Exit code 3"), "{text}");
        let req = terminals.requests.lock().unwrap()[0].clone();
        assert_eq!((req.command.as_str(), req.cwd.as_deref()), ("sh", Some(ws.root())));
        assert!(terminals.done.lock().await.is_empty(), "released");
    }

    #[tokio::test]
    async fn terminal_methods_are_refused_without_a_host() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = tokio::sync::broadcast::channel(16);
        let ws = Arc::new(TestWorkspace { root: dir.path().to_path_buf(), buffers: Default::default() });
        let acp = AcpRuntime::new(events, ws);
        acp.start(mock_agent()).await.unwrap();
        let init = acp.initialize("mock", PROTOCOL_VERSION).await.unwrap();
        assert_eq!(init["echoClientCapabilities"]["terminal"], false);
    }

    /// GUI launches don't inherit the login shell's PATH; the host passes the project's shell
    /// environment in `AgentSpec::env`, and the agent binary must be resolved with it. On
    /// Windows the agent is a `.cmd` script, as `npx` is.
    #[tokio::test]
    async fn agent_command_is_resolved_with_the_spec_path() {
        let bin = tempfile::tempdir().unwrap();
        let mock = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let shim = bin.path().join("forge-test-agent");
            std::fs::write(&shim, format!("#!/bin/sh\nexec python3 '{}' \"$@\"\n", mock.display())).unwrap();
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        #[cfg(windows)]
        std::fs::write(bin.path().join("forge-test-agent.cmd"), format!("@python \"{}\" %*\r\n", mock.display())).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = tokio::sync::broadcast::channel(16);
        let ws = Arc::new(TestWorkspace { root: dir.path().to_path_buf(), buffers: Default::default() });
        let acp = AcpRuntime::new(events, ws);
        let system_path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(std::iter::once(bin.path().to_path_buf()).chain(std::env::split_paths(&system_path))).unwrap();
        let spec = AgentSpec { id: "shim".into(), command: "forge-test-agent".into(), args: vec![], env: vec![("PATH".into(), path.to_string_lossy().into_owned())], cwd: None };
        acp.start(spec).await.unwrap();
        assert_eq!(acp.initialize("shim", PROTOCOL_VERSION).await.unwrap()["protocolVersion"], 1);
    }

    #[tokio::test]
    async fn prompt_content_sends_resource_links() {
        let Setup { dir: _dir, acp, mut rx, session, .. } = setup().await;
        let blocks = vec![json!({"type": "text", "text": "hi"}), json!({"type": "resource_link", "uri": "file:///p/a.rs", "name": "a.rs"})];
        acp.prompt_content("mock", &session, blocks).await.unwrap();
        let ev = next_agent_event(&mut rx, |e| matches!(e, AgentEvent::Notification { params, .. } if params.to_string().contains("Context:"))).await;
        assert!(format!("{ev:?}").contains("a.rs"));
    }

    #[tokio::test]
    async fn load_session_replays_history() {
        let Setup { dir: _dir, acp, ws, mut rx, session, init, .. } = setup().await;
        assert_eq!(init["agentCapabilities"]["loadSession"], true);
        acp.prompt("mock", &session, "remember me").await.unwrap();
        next_agent_event(&mut rx, |e| matches!(e, AgentEvent::Notification { params, .. } if params.to_string().contains("Echo: remember me"))).await;

        acp.load_session("mock", &session, ws.root(), &[]).await.unwrap();
        let user = next_agent_event(&mut rx, |e| matches!(e, AgentEvent::Notification { params, .. } if params["update"]["sessionUpdate"] == "user_message_chunk")).await;
        assert!(format!("{user:?}").contains("remember me"));
        next_agent_event(&mut rx, |e| matches!(e, AgentEvent::Notification { params, .. } if params["update"]["sessionUpdate"] == "agent_message_chunk")).await;
        assert!(acp.load_session("mock", "nope", ws.root(), &[]).await.is_err());
    }

    #[tokio::test]
    async fn mcp_servers_are_passed_to_new_sessions() {
        let Setup { dir: _dir, acp, ws, .. } = setup().await;
        let servers = vec![json!({"name": "fs", "command": "mcp-fs", "args": ["--root", "/p"], "env": [{"name": "TOKEN", "value": "x"}]})];
        let session = acp.new_session("mock", ws.root(), &servers).await.unwrap();
        assert_eq!(session["echoMcpServers"], json!(servers));
    }

    #[tokio::test]
    async fn terminal_auth_is_advertised_only_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        for enabled in [false, true] {
            let (events, _rx) = tokio::sync::broadcast::channel(16);
            let ws = Arc::new(TestWorkspace { root: dir.path().to_path_buf(), buffers: Default::default() });
            let acp = AcpRuntime::new(events, ws);
            let acp = if enabled { acp.with_terminal_auth().with_display_terminals() } else { acp };
            acp.start(mock_agent()).await.unwrap();
            let caps = acp.initialize("mock", PROTOCOL_VERSION).await.unwrap()["echoClientCapabilities"].clone();
            assert_eq!(caps["auth"]["terminal"], enabled);
            assert_eq!(caps["_meta"]["terminal-auth"], enabled);
            assert_eq!(caps["_meta"]["terminal_output"], enabled);
        }
    }

    #[tokio::test]
    async fn agents_that_need_login_offer_terminal_auth() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("auth-marker");
        let (events, mut rx) = tokio::sync::broadcast::channel(64);
        let ws = Arc::new(TestWorkspace { root: dir.path().to_path_buf(), buffers: Default::default() });
        let acp = AcpRuntime::new(events, ws.clone()).with_terminal_auth();
        let mut spec = mock_agent();
        spec.env = vec![("MOCK_REQUIRE_AUTH".into(), "1".into()), ("MOCK_AUTH_MARKER".into(), marker.to_string_lossy().into_owned())];
        acp.start(spec).await.unwrap();

        let init = acp.initialize("mock", PROTOCOL_VERSION).await.unwrap();
        assert_eq!(init["authMethods"][0]["id"], "mock-login");
        assert!(init["authMethods"][0]["_meta"]["terminal-auth"]["args"][1] == "--login");

        let session = acp.new_session("mock", ws.root(), &[]).await.unwrap()["sessionId"].as_str().unwrap().to_string();
        next_agent_event(&mut rx, |e| matches!(e, AgentEvent::Notification { method, .. } if method == "_auth/status_update")).await;
        let err = acp.prompt("mock", &session, "hi").await.unwrap_err().to_string();
        assert!(err.contains("Authentication required"), "{err}");

        acp.rpc("mock", "authenticate", json!({"methodId": "mock-login"})).await.unwrap();
        assert!(marker.exists());
        assert_eq!(acp.prompt("mock", &session, "hi").await.unwrap()["stopReason"], "end_turn");
    }
}
