//! An agent on the host, as a thread starts it (`forge_host::agent::start`), here without
//! ssh: its ACP lines and stderr come back, and the connections it opens on the host's MCP
//! port reach the server on this side.
#![cfg(unix)]

use std::collections::HashMap;

use forge_host::agent::{AgentCommand, Launch};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};

/// Echoes a line, writes to stderr, then asks the MCP port it is told for an answer.
const AGENT: &str = r#"
read line; echo "got:$line"; echo "to stderr" >&2
read port; exec 3<>"/dev/tcp/127.0.0.1/$port"
printf 'ping' >&3; read -r -n 4 reply <&3; echo "reply:$reply"
"#;

#[tokio::test]
async fn carries_acp_stderr_and_the_mcp_port() {
    let (project, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // Forge's MCP server on this side: answers `pong` to `ping`.
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_port = server.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut stream, _) = server.accept().await.unwrap();
        let mut ping = [0; 4];
        stream.read_exact(&mut ping).await.unwrap();
        assert_eq!(&ping, b"ping");
        stream.write_all(b"pong").await.unwrap();
    });

    let command = AgentCommand {
        program: env!("CARGO_BIN_EXE_forge-remote-server").into(),
        args: vec!["forge-host".into(), "agent".into()],
        env: HashMap::from([("XDG_DATA_HOME".to_string(), data.path().to_string_lossy().into_owned())]),
        preamble: vec![],
    };
    let launch = Launch { command: "bash".into(), args: vec!["-c".into(), AGENT.into()], env: vec![("PATH".into(), std::env::var("PATH").unwrap())], cwd: project.path().to_path_buf() };
    let mut remote = forge_host::agent::start(command, &launch, Some(local_port)).await.unwrap();
    assert_ne!(remote.port, local_port, "the host has a port of its own");

    let mut stdout = BufReader::new(remote.stdout).lines();
    let mut stderr = BufReader::new(remote.stderr).lines();
    remote.stdin.write_all(b"hello\n").await.unwrap();
    assert_eq!(stdout.next_line().await.unwrap().unwrap(), "got:hello");
    assert_eq!(stderr.next_line().await.unwrap().unwrap(), "to stderr");
    remote.stdin.write_all(format!("{}\n", remote.port).as_bytes()).await.unwrap();
    assert_eq!(stdout.next_line().await.unwrap().unwrap(), "reply:pong");
    assert_eq!(stdout.next_line().await.unwrap(), None, "the agent exited");
    assert!(remote.child.wait().await.unwrap().success());
}

#[tokio::test]
async fn says_why_an_agent_does_not_start() {
    let (project, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let command = AgentCommand {
        program: env!("CARGO_BIN_EXE_forge-remote-server").into(),
        args: vec!["forge-host".into(), "agent".into()],
        env: HashMap::from([("XDG_DATA_HOME".to_string(), data.path().to_string_lossy().into_owned())]),
        preamble: vec![],
    };
    let launch = Launch { command: "no-such-agent".into(), args: vec![], env: vec![], cwd: project.path().to_path_buf() };
    let error = forge_host::agent::start(command, &launch, None).await.err().unwrap();
    assert!(format!("{error:#}").contains("no-such-agent"), "{error:#}");
}

/// The same over a real ssh, to a host with this server installed: `FORGE_TEST_SSH` is the
/// ssh command line ("ssh -F config -p 2222 me@host") and `FORGE_TEST_SERVER` the server
/// there. Runs the mock agent (`tools/mock-acp-agent.py`, which the host must have at the same
/// path) with Forge's tools on a stand-in MCP server here, then drops the connection: the
/// agent must be gone from the host.
#[tokio::test]
#[ignore = "needs an ssh host: FORGE_TEST_SSH, FORGE_TEST_SERVER"]
async fn runs_an_agent_over_ssh() {
    let ssh: Vec<String> = std::env::var("FORGE_TEST_SSH").unwrap().split_whitespace().map(str::to_string).collect();
    let server = std::env::var("FORGE_TEST_SERVER").unwrap();
    let project = tempfile::tempdir().unwrap();
    let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").canonicalize().unwrap();

    // Forge's MCP server here: answers `initialize` and lists one tool.
    let mcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mcp_port = mcp.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (stream, _) = mcp.accept().await.unwrap();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut read = BufReader::new(read);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    read.read_line(&mut line).await.unwrap();
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; length];
                read.read_exact(&mut body).await.unwrap();
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let result = match request["method"].as_str() {
                    Some("tools/list") => serde_json::json!({ "tools": [{ "name": "forge_read", "inputSchema": { "type": "object" } }] }),
                    _ => serde_json::json!({ "protocolVersion": "2025-06-18", "capabilities": { "tools": {} } }),
                };
                let answer = serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }).to_string();
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len());
                write.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });

    let command = AgentCommand { program: ssh[0].clone(), args: ssh[1..].iter().cloned().chain([format!("{server} forge-host agent")]).collect(), env: HashMap::new(), preamble: vec![] };
    let env = vec![("PATH".into(), std::env::var("PATH").unwrap()), ("MOCK_MCP".into(), "1".into())];
    let launch = Launch { command: "python3".into(), args: vec![mock.to_string_lossy().into_owned()], env, cwd: project.path().to_path_buf() };
    let mut remote = forge_host::agent::start(command, &launch, Some(mcp_port)).await.unwrap();
    let mut stdout = BufReader::new(remote.stdout).lines();
    let mut send = async |message: serde_json::Value| remote.stdin.write_all(format!("{message}\n").as_bytes()).await.unwrap();
    let url = format!("http://127.0.0.1:{}/mcp/token", remote.port);

    send(serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": 1 } })).await;
    send(serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": { "cwd": project.path(), "mcpServers": [{ "type": "http", "name": "forge", "url": url, "headers": [] }] } })).await;
    let mut session = None;
    while session.is_none() {
        let message: serde_json::Value = serde_json::from_str(&stdout.next_line().await.unwrap().unwrap()).unwrap();
        if message["id"] == 2 {
            session = message["result"]["sessionId"].as_str().map(str::to_string);
        }
    }
    send(serde_json::json!({ "jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": { "sessionId": session, "prompt": [{ "type": "text", "text": "tools" }] } })).await;
    let mut said = String::new();
    loop {
        let message: serde_json::Value = serde_json::from_str(&stdout.next_line().await.unwrap().unwrap()).unwrap();
        if let Some(text) = message.pointer("/params/update/content/text").and_then(|t| t.as_str()) {
            said.push_str(text);
        }
        if message["id"] == 3 {
            break;
        }
    }
    assert_eq!(said, "1 MCP tools: forge_read", "the agent on the host reached the tools here");

    let running = || std::process::Command::new("pgrep").args(["-f", &mock.to_string_lossy()]).output().unwrap().status.success();
    assert!(running(), "the agent runs");
    remote.child.kill().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(!running(), "the agent stopped with the connection");
}
