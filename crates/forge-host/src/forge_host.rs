//! Forge's own work on the machine a remote project lives on.
//!
//! Zed's remote server already works on the host's files for the editor, language servers,
//! terminals, search, git and the debugger. Forge's own features (test discovery and runs,
//! run targets, the .NET Solution Explorer) read the project with `std::fs` and run its
//! tools as child processes, which must happen on the host too. So the same code runs there,
//! in `forge-remote-server forge-host <method>`: [`Host::call`] starts it over the window's
//! SSH connection (through the server's `exec`, as Zed starts remote agents), sends the
//! parameters as one JSON line on its stdin and reads [`Event`]s back, one JSON line each,
//! from its stdout. The host side is [`serve`], with handlers that the feature crates
//! register (`forge-remote-server/src/main.rs`).
//!
//! Dropping the call's task kills the local `ssh`: the host side sees its stdin close and
//! stops, with everything it started.

pub mod agent;
pub mod files;

use std::io::{BufRead as _, Write as _};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::{AsyncBufReadExt as _, AsyncWriteExt as _, StreamExt as _};
use gpui::{App, AppContext as _, Entity, Task};
use project::Project;
use release_channel::{AppVersion, ReleaseChannel};
use remote::RemoteClient;
use remote::command::RemoteCommand;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The subcommand of `forge-remote-server` that serves one call.
pub const SUBCOMMAND: &str = "forge-host";

/// What the host side writes, one per line.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Event {
    /// Output to show as it comes (a test run's log).
    Log(String),
    /// The call's result: the last line.
    Ok(Value),
    /// Why the call failed: the last line.
    Err(String),
}

/// Live output a handler sends back while it works.
#[derive(Clone)]
pub struct Log(Arc<Mutex<std::io::Stdout>>);

impl Log {
    pub fn write(&self, text: &str) {
        self.send(&Event::Log(text.to_string()));
    }

    fn send(&self, event: &Event) {
        let Ok(line) = serde_json::to_string(event) else { return };
        let Ok(mut stdout) = self.0.lock() else { return };
        // The caller may be gone: nothing to do about a closed pipe but stop, which the
        // stdin watch does.
        writeln!(stdout, "{line}").and_then(|()| stdout.flush()).ok();
    }
}

/// One call the host side answers: its parameters in, its result out.
pub type Handler = fn(Value, &Log) -> Result<Value>;

/// Serves one call of `method` with `handlers`, from this process's stdin and stdout, then
/// exits. What it started is stopped when the caller goes away (its stdin closes).
pub fn serve(method: &str, handlers: &[(&str, Handler)]) -> ! {
    let log = Log(Arc::new(Mutex::new(std::io::stdout())));
    let mut stdin = std::io::stdin().lock();
    let mut line = String::new();
    let result = stdin
        .read_line(&mut line)
        .context("reading the call's parameters")
        .and_then(|_| serde_json::from_str::<Value>(&line).context("parsing the call's parameters"));
    drop(stdin);
    std::thread::spawn(|| {
        let mut rest = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut rest).ok();
        stop_descendants();
        std::process::exit(1);
    });
    let result = result.and_then(|params| match handlers.iter().find(|(name, _)| *name == method) {
        Some((_, handler)) => handler(params, &log),
        None => Err(anyhow!("this server has no `{method}`: is it the same version as Forge?")),
    });
    log.send(&match result {
        Ok(value) => Event::Ok(value),
        Err(error) => Event::Err(format!("{error:#}")),
    });
    std::process::exit(0);
}

/// Stops every process this one started, and theirs: what a call runs (`cargo test`, and
/// the test binary it starts) is often in process groups of its own, so the whole tree is
/// followed, from `ps`. Asked to end first, then killed.
fn stop_descendants() {
    #[cfg(unix)]
    {
        let pids = descendants(std::process::id());
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            for pid in &pids {
                unsafe {
                    libc::kill(*pid as libc::pid_t, signal);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
}

/// The processes under `root`, from `ps -eo pid,ppid` (Linux and macOS).
#[cfg(unix)]
fn descendants(root: u32) -> Vec<u32> {
    let Ok(output) = std::process::Command::new("ps").args(["-eo", "pid=,ppid="]).output() else { return Vec::new() };
    let pairs: Vec<(u32, u32)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
        })
        .collect();
    let mut found = Vec::new();
    let mut pending = vec![root];
    while let Some(parent) = pending.pop() {
        for (pid, _) in pairs.iter().filter(|(_, ppid)| *ppid == parent) {
            found.push(*pid);
            pending.push(*pid);
        }
    }
    found
}

/// Reads a handler's parameters.
pub fn params<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).context("reading the call's parameters")
}

/// A handler's result.
pub fn result<T: Serialize>(value: &T) -> Result<Value> {
    serde_json::to_value(value).context("writing the call's result")
}

/// Where a remote project's own work runs: its host, through the window's connection.
#[derive(Clone)]
pub struct Host {
    client: Entity<RemoteClient>,
    program: String,
}

impl Host {
    /// The host of `project`, when its folders are on another machine.
    pub fn of(project: &Entity<Project>, cx: &App) -> Option<Self> {
        let client = project.read(cx).remote_client()?;
        Some(Self { client, program: server_binary(cx) })
    }

    /// The command that starts an agent on the host ([`agent::start`]).
    pub fn agent_command(&self, cx: &App) -> Result<agent::AgentCommand> {
        let command = RemoteCommand { program: self.program.clone(), args: vec![SUBCOMMAND.to_string(), agent::AGENT.to_string()], env: Default::default(), working_dir: None };
        let (template, preamble) = self.client.read(cx).build_stdio_command(command).context("starting the agent on the host")?;
        Ok(agent::AgentCommand { program: template.program, args: template.args, env: template.env.into_iter().collect(), preamble })
    }

    /// Calls `method` on the host with `params` and returns its result. `log` gets the
    /// output the call sends as it works.
    pub fn call<R: DeserializeOwned + Send + 'static>(
        &self,
        method: &str,
        params: &impl Serialize,
        log: Option<Arc<dyn Fn(&str) + Send + Sync>>,
        cx: &App,
    ) -> Task<Result<R>> {
        let command = RemoteCommand { program: self.program.clone(), args: vec![SUBCOMMAND.to_string(), method.to_string()], env: Default::default(), working_dir: None };
        let built = self.client.read(cx).build_stdio_command(command);
        let params = serde_json::to_string(params);
        let method = method.to_string();
        cx.background_spawn(async move {
            let (template, preamble) = built.context("running Forge's work on the host")?;
            let mut line = params.context("writing the call's parameters")?;
            line.push('\n');
            let started = std::time::Instant::now();
            let mut child = smol::process::Command::new(&template.program)
                .args(&template.args)
                .envs(&template.env)
                .stdin(smol::process::Stdio::piped())
                .stdout(smol::process::Stdio::piped())
                .stderr(smol::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .context("starting ssh")?;
            // Kept open while the call runs: closing it tells the host to stop.
            let mut stdin = child.stdin.take().context("no stdin")?;
            stdin.write_all(&preamble).await?;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
            let stdout = child.stdout.take().context("no stdout")?;
            let mut lines = smol::io::BufReader::new(stdout).lines();
            let mut outcome = None;
            while let Some(line) = lines.next().await {
                let line = line?;
                match serde_json::from_str::<Event>(&line) {
                    Ok(Event::Log(text)) => {
                        if let Some(log) = &log {
                            log(&text);
                        }
                    }
                    Ok(Event::Ok(value)) => {
                        outcome = Some(Ok(value));
                        break;
                    }
                    Ok(Event::Err(error)) => {
                        outcome = Some(Err(anyhow!(error)));
                        break;
                    }
                    // Something else wrote to stdout (a shell's greeting): not ours.
                    Err(_) => log::debug!("forge-host {method}: {line}"),
                }
            }
            drop(stdin);
            let value = match outcome {
                Some(outcome) => outcome?,
                None => {
                    let mut stderr = String::new();
                    if let Some(mut err) = child.stderr.take() {
                        smol::io::AsyncReadExt::read_to_string(&mut err, &mut stderr).await.ok();
                    }
                    let status = child.status().await.ok();
                    bail!("Forge's server on the host ended without answering `{method}` ({status:?}): {}", stderr.trim());
                }
            };
            log::info!("forge-host {method}: {:?}, {} bytes", started.elapsed(), value.to_string().len());
            serde_json::from_value(value).with_context(|| format!("reading the answer to `{method}`"))
        })
    }
}

/// A call to the host made from a thread that doesn't wait on the UI's executor (a
/// background job running git commands one after another): the `ssh` command is built once,
/// on the UI thread, and run for each call.
#[derive(Clone)]
pub struct Blocking {
    method: String,
    program: String,
    args: Vec<String>,
    env: std::collections::HashMap<String, String>,
    preamble: Vec<u8>,
}

impl Host {
    /// Calls of `method` that [`Blocking::call`] makes from any thread.
    pub fn blocking(&self, method: &str, cx: &App) -> Result<Blocking> {
        let command = RemoteCommand { program: self.program.clone(), args: vec![SUBCOMMAND.to_string(), method.to_string()], env: Default::default(), working_dir: None };
        let (template, preamble) = self.client.read(cx).build_stdio_command(command).context("running Forge's work on the host")?;
        Ok(Blocking { method: method.to_string(), program: template.program, args: template.args, env: template.env.into_iter().collect(), preamble })
    }
}

impl Blocking {
    /// Calls the method with `params` and waits for its result.
    pub fn call<R: DeserializeOwned>(&self, params: &impl Serialize) -> Result<R> {
        let mut line = serde_json::to_string(params).context("writing the call's parameters")?;
        line.push('\n');
        let mut child = std::process::Command::new(&self.program)
            .args(&self.args)
            .envs(&self.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("starting ssh")?;
        let mut stdin = child.stdin.take().context("no stdin")?;
        stdin.write_all(&self.preamble)?;
        stdin.write_all(line.as_bytes())?;
        stdin.flush()?;
        let stdout = child.stdout.take().context("no stdout")?;
        let mut outcome = None;
        for line in std::io::BufReader::new(stdout).lines() {
            match serde_json::from_str::<Event>(&line?) {
                Ok(Event::Ok(value)) => {
                    outcome = Some(Ok(value));
                    break;
                }
                Ok(Event::Err(error)) => {
                    outcome = Some(Err(anyhow!(error)));
                    break;
                }
                _ => {}
            }
        }
        drop(stdin);
        let value = match outcome {
            Some(outcome) => outcome?,
            None => {
                let mut stderr = String::new();
                if let Some(mut err) = child.stderr.take() {
                    std::io::Read::read_to_string(&mut err, &mut stderr).ok();
                }
                child.kill().ok();
                bail!("Forge's server on the host ended without answering `{}`: {}", self.method, stderr.trim());
            }
        };
        child.wait().ok();
        serde_json::from_value(value).with_context(|| format!("reading the answer to `{}`", self.method))
    }
}

/// The server binary on the host, relative to the home folder (where the server's `exec`
/// starts): named as `remote::transport` installed it.
fn server_binary(cx: &App) -> String {
    let channel = ReleaseChannel::global(cx);
    let version = match channel {
        ReleaseChannel::Dev => "build".to_string(),
        _ => AppVersion::global(cx).to_string(),
    };
    let names = paths::remote_server_names();
    format!("./{}/{}-{}-{version}", names.dir, names.binary_prefix, channel.dev_name())
}

/// Runs `call` on the project's host when it is remote, else `local` here, in the
/// background: one place for features to choose.
pub fn on_project_machine<R: DeserializeOwned + Send + 'static>(
    project: &Entity<Project>,
    method: &str,
    params: &impl Serialize,
    log: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    local: impl FnOnce() -> Result<R> + Send + 'static,
    cx: &App,
) -> Task<Result<R>> {
    match Host::of(project, cx) {
        Some(host) => host.call(method, params, log, cx),
        None => cx.background_spawn(async move { local() }),
    }
}
