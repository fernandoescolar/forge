//! An agent of a remote project, running on its host: `forge-remote-server forge-host agent`.
//!
//! Agents read, edit and run commands with their own tools, on the disk of the machine they
//! run on, so in a remote project they must run on its host. The thread still talks ACP to
//! the agent over the agent's stdin and stdout, now carried by the window's SSH connection,
//! and Forge's own tools still answer on the thread's MCP server, here: the host side opens a
//! port on the host's loopback for the agent and carries each connection to it back over the
//! same channel, to Forge's server on this machine.
//!
//! Both ways the channel carries [`Frame`]s, one JSON line each. The client sends a
//! [`Launch`] first; the host answers [`Frame::Ready`] with the port, then both send ACP lines
//! and the bytes of the MCP connections until the agent exits ([`Frame::Exit`]) or the client
//! goes away (the host's stdin closes, and the agent is stopped with what it started).

use std::collections::HashMap;
use std::io::{BufRead as _, Read as _, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

/// The `forge-host` method that runs an agent (served by [`serve`], not as a call).
pub const AGENT: &str = "agent";

/// The agent to start on the host: the first line the client sends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Launch {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// The project folder on the host.
    pub cwd: PathBuf,
}

/// What the channel carries, one JSON line each.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Frame {
    /// The agent started; Forge's MCP server is at this port of the host's loopback.
    Ready { port: u16 },
    /// A line of ACP, to the agent or from it.
    Acp(String),
    /// A line the agent wrote to stderr.
    Stderr(String),
    /// The agent opened connection `id` to the MCP port.
    Open(u64),
    /// Bytes of connection `id`, base64.
    Data(u64, String),
    /// Connection `id` closed on the side that sends this.
    Close(u64),
    /// The agent exited, with its code.
    Exit(Option<i32>),
    /// The agent couldn't start.
    Failed(String),
}

/// Writes frames to stdout, from any thread.
#[derive(Clone)]
struct Out(Arc<Mutex<std::io::Stdout>>);

impl Out {
    fn send(&self, frame: &Frame) {
        let Ok(line) = serde_json::to_string(frame) else { return };
        let Ok(mut stdout) = self.0.lock() else { return };
        writeln!(stdout, "{line}").and_then(|()| stdout.flush()).ok();
    }
}

/// Runs the agent the client asks for, on this host, until it exits or the client goes away.
pub fn serve() -> ! {
    let out = Out(Arc::new(Mutex::new(std::io::stdout())));
    let code = match run(&out) {
        Ok(code) => code,
        Err(error) => {
            out.send(&Frame::Failed(format!("{error:#}")));
            None
        }
    };
    out.send(&Frame::Exit(code));
    crate::stop_descendants();
    std::process::exit(0);
}

fn run(out: &Out) -> Result<Option<i32>> {
    let mut stdin = std::io::BufReader::new(std::io::stdin());
    let mut line = String::new();
    stdin.read_line(&mut line).context("reading the agent to start")?;
    let launch: Launch = serde_json::from_str(&line).context("reading the agent to start")?;

    let listener = TcpListener::bind("127.0.0.1:0").context("opening the port for Forge's tools")?;
    let port = listener.local_addr()?.port();
    // Found on the PATH the client sent (the project's shell environment), not this process's.
    let path = launch.env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.as_str());
    let program = path.and_then(|path| which_in(&launch.command, path)).unwrap_or_else(|| PathBuf::from(&launch.command));
    let mut command = std::process::Command::new(program);
    command.args(&launch.args).envs(launch.env.iter().map(|(k, v)| (k, v))).current_dir(&launch.cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().with_context(|| format!("starting {} on the host", launch.command))?;
    let mut agent_in = child.stdin.take().context("no stdin")?;
    let agent_out = child.stdout.take().context("no stdout")?;
    let agent_err = child.stderr.take().context("no stderr")?;
    out.send(&Frame::Ready { port });

    let connections: Arc<Mutex<HashMap<u64, TcpStream>>> = Arc::default();
    {
        let (out, connections) = (out.clone(), connections.clone());
        std::thread::spawn(move || accept(listener, out, connections));
    }
    {
        let out = out.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(agent_out).lines() {
                let Ok(line) = line else { break };
                out.send(&Frame::Acp(line));
            }
        });
    }
    {
        let out = out.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(agent_err).lines() {
                let Ok(line) = line else { break };
                out.send(&Frame::Stderr(line));
            }
        });
    }
    // What the client sends. When it goes away, so does the agent, with what it started.
    std::thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            match serde_json::from_str::<Frame>(&line) {
                Ok(Frame::Acp(text)) => {
                    if agent_in.write_all(text.as_bytes()).and_then(|()| agent_in.write_all(b"\n")).and_then(|()| agent_in.flush()).is_err() {
                        break;
                    }
                }
                Ok(Frame::Data(id, data)) => {
                    let Ok(bytes) = BASE64.decode(data) else { continue };
                    if let Some(stream) = connections.lock().unwrap().get_mut(&id) {
                        stream.write_all(&bytes).ok();
                    }
                }
                Ok(Frame::Close(id)) => {
                    if let Some(stream) = connections.lock().unwrap().remove(&id) {
                        stream.shutdown(Shutdown::Both).ok();
                    }
                }
                _ => {}
            }
        }
        crate::stop_descendants();
        std::process::exit(1);
    });
    let status = child.wait().context("waiting for the agent")?;
    Ok(status.code())
}

/// Carries each connection the agent opens to the MCP port over to the client.
fn accept(listener: TcpListener, out: Out, connections: Arc<Mutex<HashMap<u64, TcpStream>>>) {
    for (id, stream) in (1u64..).zip(listener.incoming()) {
        let Ok(stream) = stream else { continue };
        let Ok(reader) = stream.try_clone() else { continue };
        connections.lock().unwrap().insert(id, stream);
        out.send(&Frame::Open(id));
        let (out, connections) = (out.clone(), connections.clone());
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buffer = vec![0; 64 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => out.send(&Frame::Data(id, BASE64.encode(&buffer[..n]))),
                }
            }
            if connections.lock().unwrap().remove(&id).is_some() {
                out.send(&Frame::Close(id));
            }
        });
    }
}

/// `program` found in the directories of `path`, when it isn't a path already.
fn which_in(program: &str, path: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return None;
    }
    std::env::split_paths(path).map(|dir| dir.join(program)).find(|candidate| candidate.is_file())
}

/// An agent started on a project's host, as the thread's ACP runtime takes it: the streams it
/// speaks ACP on, its stderr, and the local `ssh` that carries them (stopping it stops the
/// agent).
pub struct Remote {
    pub child: tokio::process::Child,
    pub stdout: tokio::io::DuplexStream,
    pub stdin: tokio::io::DuplexStream,
    pub stderr: tokio::io::DuplexStream,
    /// The MCP port on the host's loopback: the thread's MCP URL there.
    pub port: u16,
}

/// The command that starts the host side, from [`crate::Host::agent_command`]: run with
/// `preamble` written to its stdin first.
pub struct AgentCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub preamble: Vec<u8>,
}

/// Starts `launch` on the host with `command`, carrying the connections the agent opens to
/// Forge's tools to `mcp_port` on this machine's loopback. Needs a Tokio runtime.
pub async fn start(command: AgentCommand, launch: &Launch, mcp_port: Option<u16>) -> Result<Remote> {
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};

    let mut ssh = tokio::process::Command::new(&command.program);
    ssh.args(&command.args).envs(&command.env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(unix)]
    ssh.process_group(0);
    let mut child = ssh.spawn().context("starting ssh")?;
    let mut ssh_in = child.stdin.take().context("no stdin")?;
    let ssh_out = child.stdout.take().context("no stdout")?;
    let mut ssh_err = child.stderr.take().context("no stderr")?;
    let mut first = serde_json::to_string(launch)?;
    first.push('\n');
    ssh_in.write_all(&command.preamble).await?;
    ssh_in.write_all(first.as_bytes()).await?;
    ssh_in.flush().await?;

    let mut lines = tokio::io::BufReader::new(ssh_out).lines();
    let port = loop {
        let Some(line) = lines.next_line().await? else {
            let mut stderr = String::new();
            ssh_err.read_to_string(&mut stderr).await.ok();
            bail!("Forge's server on the host ended before the agent started: {}", stderr.trim());
        };
        match serde_json::from_str::<Frame>(&line) {
            Ok(Frame::Ready { port }) => break port,
            Ok(Frame::Failed(error)) => bail!(error),
            Ok(_) => bail!("Forge's server on the host answered out of turn: {line}"),
            // A shell's greeting, before the server speaks.
            Err(_) => log::debug!("forge-host agent: {line}"),
        }
    };

    let (to_host, mut outgoing) = tokio::sync::mpsc::unbounded_channel::<Frame>();
    let (agent_stdout, mut acp_in) = tokio::io::duplex(1 << 20);
    let (agent_stdin, acp_out) = tokio::io::duplex(1 << 20);
    let (agent_stderr, mut stderr_in) = tokio::io::duplex(1 << 16);

    // Frames to the host, in order.
    tokio::spawn(async move {
        while let Some(frame) = outgoing.recv().await {
            let Ok(mut line) = serde_json::to_string(&frame) else { continue };
            line.push('\n');
            if ssh_in.write_all(line.as_bytes()).await.and(ssh_in.flush().await).is_err() {
                break;
            }
        }
    });
    // ACP the runtime writes, to the agent.
    {
        let to_host = to_host.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(acp_out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if to_host.send(Frame::Acp(line)).is_err() {
                    break;
                }
            }
        });
    }
    // What the host sends: the agent's ACP and stderr, and its MCP connections.
    tokio::spawn(async move {
        let mut connections: HashMap<u64, tokio::sync::mpsc::UnboundedSender<Vec<u8>>> = HashMap::new();
        while let Ok(Some(line)) = lines.next_line().await {
            match serde_json::from_str::<Frame>(&line) {
                Ok(Frame::Acp(mut text)) => {
                    text.push('\n');
                    if acp_in.write_all(text.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Ok(Frame::Stderr(mut text)) => {
                    text.push('\n');
                    stderr_in.write_all(text.as_bytes()).await.ok();
                }
                Ok(Frame::Open(id)) => {
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                    connections.insert(id, tx);
                    tokio::spawn(connect(id, mcp_port, rx, to_host.clone()));
                }
                Ok(Frame::Data(id, data)) => {
                    if let (Some(connection), Ok(bytes)) = (connections.get(&id), BASE64.decode(data)) {
                        connection.send(bytes).ok();
                    }
                }
                Ok(Frame::Close(id)) => {
                    connections.remove(&id);
                }
                Ok(Frame::Exit(code)) => {
                    log::info!("agent on the host exited ({code:?})");
                    break;
                }
                Ok(Frame::Failed(error)) => {
                    stderr_in.write_all(format!("{error}\n").as_bytes()).await.ok();
                }
                Ok(Frame::Ready { .. }) | Err(_) => log::debug!("forge-host agent: {line}"),
            }
        }
        // The runtime sees the agent's stdout close, and reaps it.
    });
    Ok(Remote { child, stdout: agent_stdout, stdin: agent_stdin, stderr: agent_stderr, port })
}

/// One connection the agent opened on the host: to `port` here, both ways, until either
/// side closes it.
async fn connect(id: u64, port: Option<u16>, mut from_host: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>, to_host: tokio::sync::mpsc::UnboundedSender<Frame>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let stream = match port {
        Some(port) => tokio::net::TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| anyhow!(e)),
        None => Err(anyhow!("Forge's MCP server isn't running")),
    };
    let stream = match stream {
        Ok(stream) => stream,
        Err(error) => {
            log::warn!("couldn't reach Forge's tools for the agent on the host: {error:#}");
            to_host.send(Frame::Close(id)).ok();
            return;
        }
    };
    let (mut read, mut write) = stream.into_split();
    let writer = tokio::spawn(async move {
        while let Some(bytes) = from_host.recv().await {
            if write.write_all(&bytes).await.is_err() {
                break;
            }
        }
        write.shutdown().await.ok();
    });
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match read.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to_host.send(Frame::Data(id, BASE64.encode(&buffer[..n]))).is_err() {
                    break;
                }
            }
        }
    }
    to_host.send(Frame::Close(id)).ok();
    writer.abort();
}
