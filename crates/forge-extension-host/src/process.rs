//! `forge.process.spawn`: long-running programs (an extension's sidecars, language tools,
//! watchers) that the extension talks to through their standard input and output.
//!
//! Output arrives as `process.output` events (`{ id, stream, data }`, UTF-8 text in chunks
//! that never split a character) and the end as `process.exit` (`{ id, code }`). A process
//! belongs to the extension that started it and is killed when it unloads.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use futures::{AsyncReadExt as _, AsyncWriteExt as _, StreamExt as _, channel::{mpsc, oneshot}};
use gpui::{AppContext as _, Context};
use serde_json::{Value, json};

use crate::{host::ExtensionHost, js::ToJs};

pub(crate) struct Process {
    pub(crate) owner: String,
    /// Text to write to its standard input; `None` closes it.
    stdin: mpsc::UnboundedSender<Option<String>>,
    kill: Option<oneshot::Sender<()>>,
}

/// The folder of an extension's sidecars for this machine, as `forge-ext pack` names it
/// (`bin/<platform>/`).
pub fn platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    format!("{os}-{arch}")
}

/// Where sidecar `name` of the extension in `extension_dir` is for this machine.
pub fn sidecar_path(extension_dir: &std::path::Path, name: &str) -> PathBuf {
    let file = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    extension_dir.join("bin").join(platform()).join(file)
}

impl ExtensionHost {
    /// `process.spawn`: starts a program (or the extension's sidecar) and replies with its id.
    pub(crate) fn spawn_process(&mut self, args: &Value, id: u64, cx: &mut Context<Self>) {
        let result = (|| -> Result<(String, Vec<String>, PathBuf, Vec<(String, String)>, String)> {
            let owner = args.get("extension").and_then(Value::as_str).context("spawn needs the extension that owns the process")?.to_string();
            let arguments: Vec<String> = args.get("args").and_then(Value::as_array).into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect();
            let env: Vec<(String, String)> = args.get("env").and_then(Value::as_object).into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect();
            let command = if let Some(name) = args.get("sidecar").and_then(Value::as_str) {
                let extension = self.extensions.iter().find(|e| e.id == owner).with_context(|| format!("{owner} is not loaded"))?;
                let path = sidecar_path(&extension.path, name);
                anyhow::ensure!(path.is_file(), "{owner} has no sidecar `{name}` for {} (expected {})", platform(), path.display());
                make_executable(&path)?;
                path.to_string_lossy().into_owned()
            } else {
                args.get("command").and_then(Value::as_str).context("missing command")?.to_string()
            };
            let cwd = self.resolve_path(args.get("cwd").and_then(Value::as_str).unwrap_or("."), cx);
            Ok((command, arguments, cwd, env, owner))
        })();
        let (command, arguments, cwd, extra_env, owner) = match result {
            Ok(v) => v,
            Err(e) => return self.reply(id, Err(e)),
        };
        let cwd = if cwd.is_dir() { cwd } else { std::env::temp_dir() };

        let pid = self.next_process;
        self.next_process += 1;
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded::<Option<String>>();
        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        self.processes.insert(pid, Process { owner, stdin: stdin_tx, kill: Some(kill_tx) });

        let env_task = self.workspace().map(|ws| {
            let dir: Arc<std::path::Path> = cwd.clone().into();
            let project = ws.read(cx).project().clone();
            project.update(cx, |p, cx| p.environment().update(cx, |env, cx| env.directory_environment(dir, cx)))
        });
        let js = self.js.clone();
        cx.spawn(async move |this, cx| {
            let env: Vec<(String, String)> = match env_task {
                Some(task) => task.await.map(|env| env.into_iter().collect()).unwrap_or_default(),
                None => std::env::vars().collect(),
            };
            let path = env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.clone());
            let program = ide_api::program_path(&command, path.as_deref().map(std::ffi::OsStr::new));
            let mut process = util::command::new_command(&program);
            process
                .args(&arguments)
                .envs(env)
                .envs(extra_env)
                .current_dir(&cwd)
                .stdin(util::command::Stdio::piped())
                .stdout(util::command::Stdio::piped())
                .stderr(util::command::Stdio::piped())
                .kill_on_drop(true);
            let mut child = match process.spawn() {
                Ok(child) => child,
                Err(e) => {
                    this.update(cx, |this, _| this.processes.remove(&pid)).ok();
                    js.send(ToJs::Resolve { call: id, ok: false, json: json!(format!("cannot start {command}: {e}")).to_string() });
                    return;
                }
            };
            js.send(ToJs::Resolve { call: id, ok: true, json: json!(pid).to_string() });

            let mut stdin = child.stdin.take();
            cx.background_spawn(async move {
                while let Some(message) = stdin_rx.next().await {
                    let (Some(text), Some(input)) = (message, stdin.as_mut()) else { break };
                    if input.write_all(text.as_bytes()).await.is_err() || input.flush().await.is_err() {
                        break;
                    }
                }
                drop(stdin);
            })
            .detach();
            // The pipes' types differ by platform (`Async<File>` on macOS, `ChildStdout` on Linux).
            type Pipe = Box<dyn futures::AsyncRead + Unpin + Send>;
            let pump = |reader: Option<Pipe>, stream: &'static str| {
                let js = js.clone();
                cx.background_spawn(async move {
                    let Some(mut reader) = reader else { return };
                    let mut buffer = vec![0u8; 64 * 1024];
                    let mut pending: Vec<u8> = Vec::new();
                    loop {
                        let n = match reader.read(&mut buffer).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        pending.extend_from_slice(&buffer[..n]);
                        // Send what is valid UTF-8; keep a character cut in half for the next read.
                        let valid = match std::str::from_utf8(&pending) {
                            Ok(_) => pending.len(),
                            Err(e) if e.error_len().is_none() => e.valid_up_to(),
                            Err(_) => pending.len(),
                        };
                        let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
                        pending.drain(..valid);
                        js.send(ToJs::Event { name: "process.output".into(), json: json!({ "id": pid, "stream": stream, "data": text }).to_string() });
                    }
                    if !pending.is_empty() {
                        let text = String::from_utf8_lossy(&pending).into_owned();
                        js.send(ToJs::Event { name: "process.output".into(), json: json!({ "id": pid, "stream": stream, "data": text }).to_string() });
                    }
                })
            };
            let stdout = pump(child.stdout.take().map(|r| Box::new(r) as Pipe), "stdout");
            let stderr = pump(child.stderr.take().map(|r| Box::new(r) as Pipe), "stderr");
            let status = child.status();
            let status = match futures::future::select(Box::pin(status), kill_rx).await {
                futures::future::Either::Left((status, _)) => status.ok(),
                futures::future::Either::Right((_, status)) => {
                    child.kill().ok();
                    status.await.ok()
                }
            };
            stdout.await;
            stderr.await;
            this.update(cx, |this, _| this.processes.remove(&pid)).ok();
            let code = status.and_then(|s| s.code());
            js.send(ToJs::Event { name: "process.exit".into(), json: json!({ "id": pid, "code": code }).to_string() });
        })
        .detach();
    }

    /// `process.write` / `process.closeStdin` / `process.kill`.
    pub(crate) fn process_call(&mut self, method: &str, args: &Value) -> Result<Value> {
        let pid = args.get("id").and_then(Value::as_u64).context("missing process id")?;
        let process = self.processes.get_mut(&pid).ok_or_else(|| anyhow!("process {pid} is not running"))?;
        match method {
            "process.write" => {
                let text = args.get("data").and_then(Value::as_str).unwrap_or_default().to_string();
                process.stdin.unbounded_send(Some(text)).map_err(|_| anyhow!("process {pid} closed its input"))?;
            }
            "process.closeStdin" => {
                process.stdin.unbounded_send(None).ok();
            }
            _ => {
                if let Some(kill) = process.kill.take() {
                    kill.send(()).ok();
                }
            }
        }
        Ok(Value::Null)
    }

    /// Kills every process `extension` started.
    pub(crate) fn kill_processes_of(&mut self, extension: &str) {
        for process in self.processes.values_mut().filter(|p| p.owner == extension) {
            if let Some(kill) = process.kill.take() {
                kill.send(()).ok();
            }
        }
    }
}

#[cfg(unix)]
fn make_executable(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o111 != 0o111 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o755)).with_context(|| format!("cannot make {} executable", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_: &std::path::Path) -> Result<()> {
    Ok(())
}
