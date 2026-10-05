//! Runs tests with each kind's tool (`dotnet test`, `go test`, `cargo test`, Jest,
//! Vitest or pytest) and collects per-test results.

use std::{path::Path, pin::pin, sync::atomic::{AtomicBool, Ordering}, time::Duration};

use anyhow::{Context as _, Result};
use smol::io::{AsyncRead, AsyncReadExt as _};
use util::command::Stdio;

use crate::discovery::{Kind, TestProject};
use crate::trx::{self, CaseResult};

/// What to run within one test project.
#[derive(Clone, Debug, PartialEq)]
pub enum Scope {
    Project,
    /// `Namespace.Class`
    Class(String),
    /// `Namespace.Class.Method` each
    Methods(Vec<String>),
}

impl Scope {
    /// The `dotnet test --filter` expression. No backslashes: MSBuild turns them into `/`.
    pub fn filter(&self) -> Option<String> {
        match self {
            Scope::Project => None,
            // The trailing dot keeps `FooTests` from also running `FooTestsExtra`.
            Scope::Class(class) => Some(format!("FullyQualifiedName~{class}.")),
            Scope::Methods(methods) => Some(
                methods
                    .iter()
                    .map(|method| format!("FullyQualifiedName={method}"))
                    .collect::<Vec<_>>()
                    .join("|"),
            ),
        }
    }
}

pub struct RunOutput {
    pub results: Vec<CaseResult>,
    /// Everything `dotnet test` printed, for build errors and test output.
    pub log: String,
    pub success: bool,
}

/// What the test command has printed so far, shared with the panel while it runs.
pub type LiveLog = std::sync::Arc<std::sync::Mutex<String>>;

/// Runs the tests in `scope` of `project`, with the tool for its kind. Dropping the
/// future kills the process.
/// `env` is the project's shell environment (so tools on the user's `PATH` are found when
/// Forge was started from the Dock).
pub async fn run(project: &TestProject, scope: &Scope, live: LiveLog, env: &Env) -> Result<RunOutput> {
    match &project.kind {
        Kind::Dotnet => run_dotnet(&project.path, scope, live, env).await,
        Kind::Go => crate::go::run(project, scope, live, env).await,
        Kind::Rust { .. } => crate::rust::run(project, scope, live, env).await,
        Kind::Node(_) => crate::node::run(project, scope, live, env).await,
        Kind::Python => crate::python::run(project, scope, live, env).await,
    }
}

/// How to debug some tests: a task a debug locator turns into a session, or a session
/// ready to start.
pub enum DebugPlan {
    Build(task::TaskTemplate, &'static str),
    Scenario(task::DebugScenario),
}

/// How to debug `scope` of `project`, if Forge can.
pub fn debug_plan(project: &TestProject, scope: &Scope) -> Option<DebugPlan> {
    match project.kind {
        Kind::Node(_) => crate::node::debug_scenario(project, scope).map(DebugPlan::Scenario),
        _ => debug_task(project, scope).map(|(task, adapter)| DebugPlan::Build(task, adapter)),
    }
}

/// The task the debugger builds on to debug `scope` of `project`, and the debug adapter for
/// it; `None` for Jest and Vitest, which get a ready session instead (`debug_plan`). Debug
/// locators turn these into sessions: Forge's for `dotnet test`, Zed's for `go test`,
/// `cargo test` and pytest.
pub fn debug_task(project: &TestProject, scope: &Scope) -> Option<(task::TaskTemplate, &'static str)> {
    let quoted = |text: &str| format!("\"{text}\"");
    let (command, args, adapter) = match &project.kind {
        Kind::Dotnet => {
            let mut args = vec!["test".to_string(), quoted(&project.path.to_string_lossy())];
            if let Some(filter) = scope.filter() {
                args.extend(["--filter".to_string(), quoted(&filter)]);
            }
            ("dotnet".to_string(), args, "netcoredbg")
        }
        Kind::Go => {
            let module = crate::go::module_path(&project.path)?;
            let args = std::iter::once("test".to_string()).chain(crate::go::args(scope, &module)).collect();
            ("go".to_string(), args, "Delve")
        }
        Kind::Rust { package } => ("cargo".to_string(), crate::rust::args(package, scope), "CodeLLDB"),
        Kind::Python => {
            let dir = project.dir().to_string_lossy().into_owned();
            let ids: Vec<String> = match scope {
                Scope::Project => vec![],
                Scope::Class(id) => vec![id.clone()],
                Scope::Methods(ids) => ids.clone(),
            };
            let relative = ids.iter().map(|id| id.strip_prefix(&dir).map(|r| r.trim_start_matches('/').to_string()).unwrap_or_else(|| id.clone()));
            let args = ["-m".to_string(), "pytest".to_string()].into_iter().chain(relative).collect();
            (crate::python::interpreter(project.dir(), None), args, "Debugpy")
        }
        Kind::Node(_) => return None,
    };
    let template = task::TaskTemplate {
        label: format!("Debug tests in {}", project.name),
        command,
        args,
        cwd: Some(project.dir().to_string_lossy().into_owned()),
        ..task::TaskTemplate::default()
    };
    Some((template, adapter))
}

/// Environment variables for the test command (empty: Forge's own).
pub type Env = std::collections::HashMap<String, String>;

/// A command for `program`, with `env` applied (its `PATH` finds `program`).
pub fn command(program: &str, env: &Env) -> util::command::Command {
    let program = env
        .get("PATH")
        .and_then(|path| std::env::split_paths(path).map(|dir| dir.join(program)).find(|candidate| candidate.is_file()))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string());
    let mut command = util::command::new_command(program);
    command.envs(env.iter());
    command
}

/// Builds and runs `dotnet test` for `scope`, reading the TRX files it writes.
async fn run_dotnet(project: &Path, scope: &Scope, live: LiveLog, env: &Env) -> Result<RunOutput> {
    let results_dir = tempfile::Builder::new().prefix("forge-tests-").tempdir()?;
    let mut command = command("dotnet", env);
    command
        .arg("test")
        .arg(project)
        .args(["--logger", "trx", "--results-directory"])
        .arg(results_dir.path())
        // Plain output: the terminal logger's live view is noise in a captured log.
        .args(["--tl:off", "--nologo"]);
    if let Some(filter) = scope.filter() {
        command.args(["--filter", &filter]);
    }
    if let Some(dir) = project.parent() {
        command.current_dir(dir);
    }
    let output = execute(command, live, None).await.context("failed to start `dotnet test`; is the .NET SDK installed?")?;
    let mut results = Vec::new();
    for entry in std::fs::read_dir(results_dir.path())?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".trx") {
            continue;
        }
        let xml = std::fs::read_to_string(entry.path())?;
        results.extend(trx::parse(&xml, trx::framework_from_file_name(&name).as_deref())?);
    }
    Ok(RunOutput { results, log: output.log, success: output.success })
}

/// What a test command printed and how it ended.
pub struct Executed {
    pub stdout: String,
    pub stderr: String,
    /// Both streams as shown while it ran.
    pub log: String,
    pub success: bool,
}

/// Runs `command`, copying its output into `live` as it comes (stdout lines through
/// `show`, when given: tools whose stdout is JSON show its readable part). Dropping the
/// future kills the process.
///
/// Processes start with no blocked signals (`forge_ui::process`): `dotnet` hangs after its
/// last test otherwise. The run ends when the process exits, not when its output closes:
/// build servers that outlive it can keep the pipes open for minutes.
pub async fn execute(mut command: util::command::Command, live: LiveLog, show: Option<fn(&str) -> Option<String>>) -> Result<Executed> {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    live.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let mut child = forge_ui::process::spawn_unblocked(command)?;
    let stdout_text = LiveLog::default();
    let stderr_text = LiveLog::default();
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let drained = AtomicBool::new(false);
    let drain = async {
        smol::future::zip(append(stdout, vec![stdout_text.clone()], live.clone(), show), append(stderr, vec![stderr_text.clone()], live.clone(), None)).await;
        drained.store(true, Ordering::Relaxed);
        smol::future::pending::<()>().await
    };
    let mut drain = pin!(drain);
    let exited = async {
        loop {
            if let Some(status) = child.try_status()? {
                return anyhow::Ok(status);
            }
            smol::Timer::after(Duration::from_millis(250)).await;
        }
    };
    let status = smol::future::or(exited, async {
        drain.as_mut().await;
        unreachable!()
    })
    .await?;
    // Give the pipes a moment to deliver what the process printed last.
    smol::future::or(drain.as_mut(), async {
        for _ in 0..20 {
            if drained.load(Ordering::Relaxed) {
                break;
            }
            smol::Timer::after(Duration::from_millis(25)).await;
        }
    })
    .await;
    let take = |log: &LiveLog| log.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(Executed { stdout: take(&stdout_text), stderr: take(&stderr_text), log: take(&live), success: status.success() })
}

/// Appends everything `stream` yields to `raw` and, line by line through `show`, to
/// `live`; multi-byte characters split across reads stay intact.
async fn append(stream: Option<impl AsyncRead + Unpin>, raw: Vec<LiveLog>, live: LiveLog, show: Option<fn(&str) -> Option<String>>) {
    let Some(mut stream) = stream else { return };
    let mut buf = vec![0u8; 8192];
    let mut pending = Vec::new();
    let mut line = String::new();
    let push = |text: &str, line: &mut String| {
        for log in &raw {
            log.lock().unwrap_or_else(|e| e.into_inner()).push_str(text);
        }
        match show {
            None => live.lock().unwrap_or_else(|e| e.into_inner()).push_str(text),
            Some(show) => {
                line.push_str(text);
                while let Some(end) = line.find('\n') {
                    let full: String = line.drain(..=end).collect();
                    if let Some(shown) = show(full.trim_end_matches('\n')) {
                        live.lock().unwrap_or_else(|e| e.into_inner()).push_str(&shown);
                    }
                }
            }
        }
    };
    while let Ok(n) = stream.read(&mut buf).await {
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buf[..n]);
        let valid = match std::str::from_utf8(&pending) {
            Ok(_) => pending.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => pending.len(),
        };
        let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
        pending.drain(..valid);
        push(&text, &mut line);
    }
    if !pending.is_empty() {
        let text = String::from_utf8_lossy(&pending).into_owned();
        push(&text, &mut line);
    }
    if !line.is_empty() {
        push("\n", &mut line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_debug_tasks() {
        use crate::discovery::{Kind, TestProject};
        let project = |kind: Kind, path: &str| TestProject { name: "p".into(), path: path.into(), kind, classes: vec![] };
        let (dotnet, adapter) = debug_task(&project(Kind::Dotnet, "/s/T.csproj"), &Scope::Methods(vec!["Ns.A.B".into()])).unwrap();
        assert_eq!((dotnet.command.as_str(), adapter), ("dotnet", "netcoredbg"));
        assert_eq!(dotnet.args, ["test", "\"/s/T.csproj\"", "--filter", "\"FullyQualifiedName=Ns.A.B\""]);
        let (rust, adapter) = debug_task(&project(Kind::Rust { package: "core".into() }, "/r/Cargo.toml"), &Scope::Project).unwrap();
        assert_eq!((rust.command.as_str(), adapter, rust.args[0].as_str()), ("cargo", "CodeLLDB", "test"));
        let (python, adapter) = debug_task(&project(Kind::Python, "/nonexistent/py/pyproject.toml"), &Scope::Methods(vec!["/nonexistent/py/tests/test_a.py::test_b".into()])).unwrap();
        assert_eq!((python.command.as_str(), adapter), ("python3", "Debugpy"));
        assert_eq!(python.args, ["-m", "pytest", "tests/test_a.py::test_b"]);
        assert_eq!(python.cwd.as_deref(), Some("/nonexistent/py"));
        assert!(debug_task(&project(Kind::Node(crate::discovery::NodeRunner::Jest), "/n/package.json"), &Scope::Project).is_none(), "Node tests get a ready session instead");
    }

    #[test]
    fn builds_filters() {
        assert_eq!(Scope::Project.filter(), None);
        assert_eq!(Scope::Class("Ns.FooTests".into()).filter().as_deref(), Some("FullyQualifiedName~Ns.FooTests."));
        assert_eq!(
            Scope::Methods(vec!["Ns.A.B".into(), "Ns.A.C".into()]).filter().as_deref(),
            Some("FullyQualifiedName=Ns.A.B|FullyQualifiedName=Ns.A.C")
        );
    }

    /// Needs the .NET SDK and an xUnit project with the tests of `fixtures/demo_net10.0.trx`:
    /// `FORGE_TESTS_DOTNET_PROJECT=/path/Demo.Tests.csproj cargo test -p forge-tests -- --ignored`
    #[test]
    #[ignore]
    fn runs_a_real_project() {
        let Ok(project) = std::env::var("FORGE_TESTS_DOTNET_PROJECT") else {
            return;
        };
        let project = std::path::PathBuf::from(project);
        let all = futures::executor::block_on(run_dotnet(&project, &Scope::Project, Default::default(), &Env::default())).unwrap();
        let outcome = |name: &str| all.results.iter().find(|r| r.display_name.ends_with(name)).map(|r| r.outcome);
        assert_eq!(outcome(".Adds"), Some(crate::trx::Outcome::Passed));
        assert_eq!(outcome(".Fails"), Some(crate::trx::Outcome::Failed));
        assert_eq!(all.results.len(), 5, "{}", all.log);

        let one = futures::executor::block_on(run_dotnet(&project, &Scope::Methods(vec!["Demo.Tests.MathTests.Doubles".into()]), Default::default(), &Env::default())).unwrap();
        assert_eq!(one.results.len(), 2, "both theory rows, nothing else: {}", one.log);
    }
}
