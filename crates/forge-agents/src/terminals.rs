//! ACP `terminal/*` backed by real Zed terminals (PTY, colours, the project's shell
//! environment). The agent panel embeds the same `terminal::Terminal` entities, so the
//! user watches the agent's commands run live.
//!
//! Like `ProjectFs`, the tokio-side [`TerminalHost`] forwards to a task on the GPUI thread.

use anyhow::Context as _;
use async_trait::async_trait;
use futures::{
    FutureExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
    future::Shared,
};
use gpui::{App, AppContext as _, AsyncApp, Entity, Task, WeakEntity};
use ide_api::{IdeError, IdeResult, TerminalExit, TerminalHost, TerminalOutput, TerminalRequest};
use project::Project;
use std::{cell::RefCell, collections::HashMap, process::ExitStatus, rc::Rc};
use terminal::Terminal;

type ExitTask = Shared<Task<Option<ExitStatus>>>;

struct Running {
    terminal: Entity<Terminal>,
    exit: ExitTask,
    output_byte_limit: Option<usize>,
}

enum Request {
    Create(TerminalRequest, oneshot::Sender<IdeResult<String>>),
    Output(String, oneshot::Sender<IdeResult<TerminalOutput>>),
    Wait(String, oneshot::Sender<IdeResult<TerminalExit>>),
    Kill(String, oneshot::Sender<IdeResult<()>>),
    Release(String, oneshot::Sender<IdeResult<()>>),
}

/// GPUI-side registry of agent terminals; the panel looks terminals up here to embed them.
#[derive(Clone, Default)]
pub struct TerminalRegistry(Rc<RefCell<HashMap<String, Entity<Terminal>>>>);

impl TerminalRegistry {
    pub fn get(&self, id: &str) -> Option<Entity<Terminal>> {
        self.0.borrow().get(id).cloned()
    }
}

pub struct ZedTerminals {
    tx: mpsc::UnboundedSender<Request>,
}

impl ZedTerminals {
    pub fn new(project: &Entity<Project>, registry: TerminalRegistry, cx: &mut App) -> Self {
        let (tx, mut rx) = mpsc::unbounded::<Request>();
        let project = project.downgrade();
        // The thread owns the registry. This loop lives as long as the agent does, so it must
        // not keep the registry (and every terminal in it) alive after the thread is gone.
        let registry = Rc::downgrade(&registry.0);
        cx.spawn(async move |cx: &mut AsyncApp| {
            let mut running: HashMap<String, Running> = HashMap::new();
            let mut next_id = 1u64;
            while let Some(req) = rx.next().await {
                match req {
                    Request::Create(request, reply) => {
                        let id = format!("forge-term-{next_id}");
                        next_id += 1;
                        let limit = request.output_byte_limit;
                        let result = spawn(&project, request, false, cx).await.map(|terminal| {
                            let exit = cx.update(|cx| terminal.read(cx).wait_for_completed_task(cx)).shared();
                            if let Some(registry) = registry.upgrade() {
                                registry.borrow_mut().insert(id.clone(), terminal.clone());
                            }
                            running.insert(id.clone(), Running { terminal, exit, output_byte_limit: limit });
                            id
                        });
                        let _ = reply.send(result.map_err(|e| IdeError::Io(format!("{e:#}"))));
                    }
                    Request::Output(id, reply) => {
                        let result = match running.get(&id) {
                            Some(t) => {
                                let (output, truncated) = cx.update(|cx| tail(t.terminal.read(cx).get_content(), t.output_byte_limit));
                                let exit = t.exit.peek().map(|status| to_exit(*status));
                                Ok(TerminalOutput { output, truncated, exit })
                            }
                            None => Err(unknown(&id)),
                        };
                        let _ = reply.send(result);
                    }
                    Request::Wait(id, reply) => match running.get(&id) {
                        // Don't block the request loop while the command runs.
                        Some(t) => {
                            let exit = t.exit.clone();
                            cx.background_spawn(async move {
                                let _ = reply.send(Ok(to_exit(exit.await)));
                            })
                            .detach();
                        }
                        None => {
                            let _ = reply.send(Err(unknown(&id)));
                        }
                    },
                    Request::Kill(id, reply) => {
                        let result = match running.get(&id) {
                            Some(t) => {
                                cx.update(|cx| t.terminal.update(cx, |t, _| t.kill_active_task()));
                                Ok(())
                            }
                            None => Err(unknown(&id)),
                        };
                        let _ = reply.send(result);
                    }
                    Request::Release(id, reply) => {
                        if let Some(t) = running.remove(&id) {
                            if t.exit.peek().is_none() {
                                cx.update(|cx| t.terminal.update(cx, |t, _| t.kill_active_task()));
                            }
                        }
                        // The registry keeps the entity so the transcript can still show it.
                        let _ = reply.send(Ok(()));
                    }
                }
            }
        })
        .detach();
        Self { tx }
    }

    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<IdeResult<T>>) -> Request) -> IdeResult<T> {
        let (tx, rx) = oneshot::channel();
        self.tx.unbounded_send(make(tx)).map_err(|_| closed())?;
        rx.await.map_err(|_| closed())?
    }
}

fn closed() -> IdeError {
    IdeError::Unavailable("project closed".into())
}

fn unknown(id: &str) -> IdeError {
    IdeError::NotFound(format!("terminal {id}"))
}

/// An interactive terminal (stdin attached) running `request`, e.g. an agent's login.
pub async fn spawn_interactive(project: &WeakEntity<Project>, request: TerminalRequest, cx: &mut AsyncApp) -> anyhow::Result<Entity<Terminal>> {
    spawn(project, request, true, cx).await
}

/// Mirrors Zed's own ACP terminals: the project's shell environment, no pagers, and stdin
/// closed unless `interactive`.
async fn spawn(project: &WeakEntity<Project>, request: TerminalRequest, interactive: bool, cx: &mut AsyncApp) -> anyhow::Result<Entity<Terminal>> {
    let project = project.upgrade().context("project closed")?;
    let mut env = match &request.cwd {
        Some(dir) => {
            let dir: std::sync::Arc<std::path::Path> = dir.clone().into();
            project.update(cx, |p, cx| p.environment().update(cx, |env, cx| env.directory_environment(dir, cx))).await.unwrap_or_default()
        }
        None => Default::default(),
    };
    env.insert("PAGER".into(), String::new());
    env.insert("GIT_PAGER".into(), "cat".into());
    env.extend(request.env);

    let shell = task::Shell::Program(util::get_default_system_shell_preferring_bash());
    let builder = task::ShellBuilder::new(&shell, false);
    let builder = if interactive { builder } else { builder.redirect_stdin_to_dev_null() };
    let (command, args) = builder.build(Some(request.command), &request.args);
    project
        .update(cx, |p, cx| {
            p.create_terminal_task(task::SpawnInTerminal { command: Some(command), args, cwd: request.cwd, env, ..Default::default() }, cx)
        })
        .await
}

/// ACP truncates from the beginning: keep the newest output.
fn tail(content: String, limit: Option<usize>) -> (String, bool) {
    match limit {
        Some(limit) if content.len() > limit => {
            let mut start = content.len() - limit;
            while !content.is_char_boundary(start) {
                start += 1;
            }
            (content[start..].to_string(), true)
        }
        _ => (content, false),
    }
}

fn to_exit(status: Option<ExitStatus>) -> TerminalExit {
    match status {
        Some(s) => TerminalExit { exit_code: s.code().map(|c| c as u32), signal: signal(&s) },
        None => TerminalExit::default(),
    }
}

/// The signal that ended the command (Unix only: Windows has none).
#[cfg(unix)]
fn signal(status: &ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal().map(|n| n.to_string())
}

#[cfg(not(unix))]
fn signal(_: &ExitStatus) -> Option<String> {
    None
}

#[async_trait]
impl TerminalHost for ZedTerminals {
    async fn create(&self, request: TerminalRequest) -> IdeResult<String> {
        self.ask(|tx| Request::Create(request, tx)).await
    }
    async fn output(&self, id: &str) -> IdeResult<TerminalOutput> {
        self.ask(|tx| Request::Output(id.to_string(), tx)).await
    }
    async fn wait_for_exit(&self, id: &str) -> IdeResult<TerminalExit> {
        self.ask(|tx| Request::Wait(id.to_string(), tx)).await
    }
    async fn kill(&self, id: &str) -> IdeResult<()> {
        self.ask(|tx| Request::Kill(id.to_string(), tx)).await
    }
    async fn release(&self, id: &str) -> IdeResult<()> {
        self.ask(|tx| Request::Release(id.to_string(), tx)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use settings::SettingsStore;
    use std::time::Duration;

    fn request(script: &str) -> TerminalRequest {
        TerminalRequest { command: "sh".into(), args: vec!["-c".into(), script.into()], env: vec![("FORGE_TEST".into(), "yes".into())], cwd: None, output_byte_limit: None }
    }

    /// Returns the project too: the host only holds a weak handle (the workspace owns it).
    async fn setup(cx: &mut TestAppContext) -> (ZedTerminals, TerminalRegistry, Entity<Project>) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        cx.executor().allow_parking();
        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let registry = TerminalRegistry::default();
        let host = cx.update(|cx| ZedTerminals::new(&project, registry.clone(), cx));
        (host, registry, project)
    }

    #[gpui::test]
    async fn runs_commands_in_real_zed_terminals(cx: &mut TestAppContext) {
        let (host, registry, _project) = setup(cx).await;
        let id = host.create(request("echo forged-$FORGE_TEST; exit 3")).await.unwrap();
        assert!(registry.get(&id).is_some(), "panel can embed it");
        let exit = host.wait_for_exit(&id).await.unwrap();
        assert_eq!(exit.exit_code, Some(3));
        let out = host.output(&id).await.unwrap();
        assert!(out.output.contains("forged-yes"), "{:?}", out.output);
        assert_eq!(out.exit.unwrap().exit_code, Some(3));
        host.release(&id).await.unwrap();
        assert!(matches!(host.output(&id).await, Err(IdeError::NotFound(_))));
        assert!(registry.get(&id).is_some(), "transcript keeps showing released terminals");
    }

    #[gpui::test]
    async fn kill_ends_a_running_command(cx: &mut TestAppContext) {
        let (host, _, _project) = setup(cx).await;
        let id = host.create(request("echo started; sleep 60")).await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !host.output(&id).await.unwrap().output.contains("started") {
            assert!(std::time::Instant::now() < deadline, "no output");
            cx.background_executor.timer(Duration::from_millis(20)).await;
        }
        assert!(host.output(&id).await.unwrap().exit.is_none(), "still running");
        host.kill(&id).await.unwrap();
        host.wait_for_exit(&id).await.unwrap();
    }

    #[test]
    fn tail_keeps_newest_output_on_char_boundaries() {
        assert_eq!(tail("abcdef".into(), Some(3)), ("def".into(), true));
        assert_eq!(tail("abc".into(), Some(10)), ("abc".into(), false));
        assert_eq!(tail("ñandú".into(), Some(2)), ("ú".into(), true));
        assert_eq!(tail("x".into(), None), ("x".into(), false));
    }
}
