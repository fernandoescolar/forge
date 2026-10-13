//! Runs tests with each kind's tool (`dotnet test`, `go test`, `cargo test`, Jest,
//! Vitest or pytest) and collects per-test results.

use std::{path::Path, pin::pin, sync::atomic::{AtomicBool, Ordering}, time::Duration};

use anyhow::{Context as _, Result};
use forge_languages::active_build::ActiveBuild;
use smol::io::{AsyncRead, AsyncReadExt as _};
use util::command::Stdio;

use crate::cobertura::Coverage;
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
    /// One data row of a .NET test: its method, and the `--filter` that selects the row
    /// (`cases::CaseRow::filter`).
    Case { method: String, filter: String },
}

impl Scope {
    /// The `dotnet test --filter` expression. No backslashes: MSBuild turns them into `/`.
    pub fn filter(&self) -> Option<String> {
        match self {
            Scope::Case { filter, .. } => Some(filter.clone()),
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
    /// What the run covered, when coverage was asked for (.NET).
    pub coverage: Option<Coverage>,
}

/// How to run .NET tests.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunOptions {
    /// The configuration and framework (the title bar's).
    pub build: ActiveBuild,
    /// Collect code coverage.
    pub coverage: bool,
    /// A `.runsettings` file (`dotnet test --settings`).
    pub settings: Option<std::path::PathBuf>,
}

/// What the test command has printed so far, shared with the panel while it runs.
pub type LiveLog = std::sync::Arc<std::sync::Mutex<String>>;

/// Runs the tests in `scope` of `project`, with the tool for its kind. Dropping the
/// future kills the process.
/// `env` is the project's shell environment (so tools on the user's `PATH` are found when
/// Forge was started from the Dock).
/// `options` apply to .NET tests.
pub async fn run(project: &TestProject, scope: &Scope, live: LiveLog, env: &Env, options: &RunOptions) -> Result<RunOutput> {
    match &project.kind {
        Kind::Dotnet => run_dotnet(&project.path, scope, live, env, options).await,
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
pub fn debug_plan(project: &TestProject, scope: &Scope, build: &ActiveBuild) -> Option<DebugPlan> {
    match project.kind {
        Kind::Node(_) => crate::node::debug_scenario(project, scope).map(DebugPlan::Scenario),
        _ => debug_task(project, scope, build).map(|(task, adapter)| DebugPlan::Build(task, adapter)),
    }
}

/// The task the debugger builds on to debug `scope` of `project`, and the debug adapter for
/// it; `None` for Jest and Vitest, which get a ready session instead (`debug_plan`). Debug
/// locators turn these into sessions: Forge's for `dotnet test`, Zed's for `go test`,
/// `cargo test` and pytest.
pub fn debug_task(project: &TestProject, scope: &Scope, build: &ActiveBuild) -> Option<(task::TaskTemplate, &'static str)> {
    let quoted = |text: &str| format!("\"{text}\"");
    let (command, args, adapter) = match &project.kind {
        Kind::Dotnet => {
            let mut args = vec!["test".to_string(), quoted(&project.path.to_string_lossy())];
            args.extend(dotnet_build_args(&project.path, build));
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
                Scope::Case { method, .. } => vec![method.clone()],
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

/// `-c`, the platform and `--framework` for `dotnet test` of `project`: the framework only
/// when the project targets the chosen one (otherwise every framework runs, as usual).
fn dotnet_build_args(project: &Path, build: &ActiveBuild) -> Vec<String> {
    let mut args = build.configuration_args();
    if let Some(framework) = build.chosen_framework_in(&forge_languages::target_frameworks(project)) {
        args.extend(["--framework".to_string(), framework]);
    }
    args
}

/// The coverage collector for `dotnet test --collect`: coverlet's when the project
/// references `coverlet.collector` (the test templates do), else Microsoft's, which comes
/// with `Microsoft.NET.Test.Sdk`. Both write Cobertura.
fn coverage_collector(project: &Path) -> &'static str {
    let text = std::fs::read_to_string(project).unwrap_or_default().to_ascii_lowercase();
    if text.contains("\"coverlet.collector\"") {
        "XPlat Code Coverage;Format=cobertura"
    } else {
        "Code Coverage;Format=cobertura"
    }
}

/// Whether `dotnet test` runs `project` on Microsoft.Testing.Platform rather than VSTest:
/// the nearest `global.json` above it says `"test": { "runner": "Microsoft.Testing.Platform" }`
/// (.NET 10). It takes other arguments then, and exits with 5 on VSTest's.
fn uses_testing_platform(project: &Path) -> bool {
    let Some(global) = project.ancestors().skip(1).map(|dir| dir.join("global.json")).find(|file| file.is_file()) else {
        return false;
    };
    let json: serde_json::Value = match std::fs::read_to_string(&global).map_err(anyhow::Error::from).and_then(|text| Ok(serde_json::from_str(&text)?)) {
        Ok(json) => json,
        Err(error) => {
            log::warn!("{}: {error:#}", global.display());
            return false;
        }
    };
    json.pointer("/test/runner").and_then(|runner| runner.as_str()).is_some_and(|runner| runner.eq_ignore_ascii_case("Microsoft.Testing.Platform"))
}

/// The Microsoft.Testing.Platform option that writes a TRX file: xUnit v3 has its own, the
/// others take `Microsoft.Testing.Extensions.TrxReport`'s.
fn testing_platform_trx_option(project: &Path) -> &'static str {
    let text = std::fs::read_to_string(project).unwrap_or_default().to_ascii_lowercase();
    if text.contains("\"xunit.v3") { "--report-xunit-trx" } else { "--report-trx" }
}

/// The `dotnet test` arguments for `options`, after the project.
fn dotnet_test_args(project: &Path, options: &RunOptions, testing_platform: bool) -> Vec<String> {
    let mut args = dotnet_build_args(project, &options.build);
    if let Some(settings) = &options.settings {
        args.extend(["--settings".to_string(), settings.to_string_lossy().into_owned()]);
    }
    if options.coverage {
        if testing_platform {
            // `Microsoft.Testing.Extensions.CodeCoverage`'s.
            args.extend(["--coverage", "--coverage-output-format", "cobertura"].map(String::from));
        } else {
            args.extend(["--collect".to_string(), coverage_collector(project).to_string()]);
        }
    }
    args
}

/// Builds and runs `dotnet test` for `scope`, reading the TRX files and coverage reports it
/// writes.
async fn run_dotnet(project: &Path, scope: &Scope, live: LiveLog, env: &Env, options: &RunOptions) -> Result<RunOutput> {
    let results_dir = tempfile::Builder::new().prefix("forge-tests-").tempdir()?;
    let testing_platform = uses_testing_platform(project);
    let mut command = command("dotnet", env);
    command.arg("test");
    if testing_platform {
        command
            .arg("--project")
            .arg(project)
            .args(dotnet_test_args(project, options, true))
            .args([testing_platform_trx_option(project), "--results-directory"])
            .arg(results_dir.path())
            // Plain output: the live progress is noise in a captured log.
            .args(["--no-ansi", "--no-progress"]);
    } else {
        command
            .arg(project)
            .args(dotnet_test_args(project, options, false))
            .args(["--logger", "trx", "--results-directory"])
            .arg(results_dir.path())
            // Plain output: the terminal logger's live view is noise in a captured log.
            .args(["--tl:off", "--nologo"]);
    }
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
    let coverage = options.coverage.then(|| read_coverage(results_dir.path(), project));
    Ok(RunOutput { results, log: output.log, success: output.success, coverage })
}

/// The coverage reports under `dir` (each collector writes into a folder of its own),
/// merged, without the test project's own files.
fn read_coverage(dir: &Path, project: &Path) -> Coverage {
    let mut coverage = Coverage::default();
    let base = project.parent().unwrap_or(dir);
    let mut pending = vec![dir.to_path_buf()];
    while let Some(folder) = pending.pop() {
        for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.to_string_lossy().ends_with(".cobertura.xml") {
                match std::fs::read_to_string(&path).map_err(anyhow::Error::from).and_then(|xml| crate::cobertura::parse(&xml, base)) {
                    Ok(report) => coverage.merge(report),
                    Err(error) => log::warn!("{}: {error:#}", path.display()),
                }
            }
        }
    }
    // Editors know files by their real path (`/private/tmp`, not `/tmp`, on macOS).
    let canonical_base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let mut canonical = Coverage::default();
    for (file, lines) in std::mem::take(&mut coverage.files) {
        let file = file.canonicalize().unwrap_or(file);
        if !file.starts_with(&canonical_base) && !file.starts_with(base) {
            canonical.merge(Coverage { files: [(file, lines)].into_iter().collect() });
        }
    }
    canonical
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
    // Declared after `child`, so dropped first: its children go while it still has them.
    let mut cancel = KillDescendantsOnDrop { pid: child.id(), armed: true };
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
    // What it leaves running on purpose (build servers) stays.
    cancel.armed = false;
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

/// Stops what a test command started when its run is dropped before it ends (the user
/// cancelled): `kill_on_drop` stops only the command, and `cargo test`'s test binary would
/// run on, adopted by init.
struct KillDescendantsOnDrop {
    pid: u32,
    armed: bool,
}

impl Drop for KillDescendantsOnDrop {
    fn drop(&mut self) {
        if self.armed {
            forge_ui::process::kill_descendants(self.pid);
        }
    }
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
        let default = ActiveBuild::default();
        let (dotnet, adapter) = debug_task(&project(Kind::Dotnet, "/s/T.csproj"), &Scope::Methods(vec!["Ns.A.B".into()]), &default).unwrap();
        assert_eq!((dotnet.command.as_str(), adapter), ("dotnet", "netcoredbg"));
        assert_eq!(dotnet.args, ["test", "\"/s/T.csproj\"", "--filter", "\"FullyQualifiedName=Ns.A.B\""]);
        let release = ActiveBuild { configuration: Some("Release".into()), ..Default::default() };
        let (dotnet, _) = debug_task(&project(Kind::Dotnet, "/s/T.csproj"), &Scope::Project, &release).unwrap();
        assert_eq!(dotnet.args, ["test", "\"/s/T.csproj\"", "-c", "Release"]);
        let (rust, adapter) = debug_task(&project(Kind::Rust { package: "core".into() }, "/r/Cargo.toml"), &Scope::Project, &default).unwrap();
        assert_eq!((rust.command.as_str(), adapter, rust.args[0].as_str()), ("cargo", "CodeLLDB", "test"));
        let (python, adapter) = debug_task(&project(Kind::Python, "/nonexistent/py/pyproject.toml"), &Scope::Methods(vec!["/nonexistent/py/tests/test_a.py::test_b".into()]), &default).unwrap();
        assert_eq!((python.command.as_str(), adapter), ("python3", "Debugpy"));
        assert_eq!(python.args, ["-m", "pytest", "tests/test_a.py::test_b"]);
        assert_eq!(python.cwd.as_deref(), Some("/nonexistent/py"));
        assert!(debug_task(&project(Kind::Node(crate::discovery::NodeRunner::Jest), "/n/package.json"), &Scope::Project, &default).is_none(), "Node tests get a ready session instead");
    }

    #[test]
    fn coverage_and_runsettings_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let with_coverlet = dir.path().join("A.Tests.csproj");
        std::fs::write(&with_coverlet, r#"<Project><ItemGroup><PackageReference Include="coverlet.collector" Version="6.0.4" /></ItemGroup></Project>"#).unwrap();
        let without = dir.path().join("B.Tests.csproj");
        std::fs::write(&without, r#"<Project><ItemGroup><PackageReference Include="Microsoft.NET.Test.Sdk" Version="17.14.1" /></ItemGroup></Project>"#).unwrap();
        let options = RunOptions { coverage: true, settings: Some("/s/ci.runsettings".into()), ..Default::default() };
        assert_eq!(dotnet_test_args(&with_coverlet, &options, false), ["--settings", "/s/ci.runsettings", "--collect", "XPlat Code Coverage;Format=cobertura"]);
        assert_eq!(dotnet_test_args(&without, &options, false)[3], "Code Coverage;Format=cobertura");
        assert!(dotnet_test_args(&without, &RunOptions::default(), false).is_empty());
        assert_eq!(dotnet_test_args(&without, &options, true)[2..], ["--coverage", "--coverage-output-format", "cobertura"]);
    }

    #[test]
    fn detects_microsoft_testing_platform() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("tests/A.Tests/A.Tests.csproj");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(&project, r#"<Project><ItemGroup><PackageReference Include="xunit.v3" /></ItemGroup></Project>"#).unwrap();
        assert!(!uses_testing_platform(&project), "no global.json: VSTest");
        std::fs::write(dir.path().join("global.json"), r#"{ "sdk": { "version": "10.0.100" }, "test": { "runner": "Microsoft.Testing.Platform" } }"#).unwrap();
        assert!(uses_testing_platform(&project));
        assert_eq!(testing_platform_trx_option(&project), "--report-xunit-trx");
        // The nearest global.json decides.
        std::fs::write(dir.path().join("tests/global.json"), r#"{ "sdk": { "version": "10.0.100" } }"#).unwrap();
        assert!(!uses_testing_platform(&project));
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
        let all = futures::executor::block_on(run_dotnet(&project, &Scope::Project, Default::default(), &Env::default(), &RunOptions::default())).unwrap();
        let outcome = |name: &str| all.results.iter().find(|r| r.display_name.ends_with(name)).map(|r| r.outcome);
        assert_eq!(outcome(".Adds"), Some(crate::trx::Outcome::Passed));
        assert_eq!(outcome(".Fails"), Some(crate::trx::Outcome::Failed));
        assert_eq!(all.results.len(), 5, "{}", all.log);

        let one = futures::executor::block_on(run_dotnet(&project, &Scope::Methods(vec!["Demo.Tests.MathTests.Doubles".into()]), Default::default(), &Env::default(), &RunOptions::default())).unwrap();
        assert_eq!(one.results.len(), 2, "both theory rows, nothing else: {}", one.log);
    }
}
