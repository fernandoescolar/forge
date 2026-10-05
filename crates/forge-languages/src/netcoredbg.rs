//! Debugging C# with Samsung's netcoredbg, the open-source .NET debug adapter.
//!
//! - [`NetcoredbgAdapter`] finds `netcoredbg` on the PATH or downloads its latest release.
//! - [`DotnetTestLocator`] turns the gutter's `dotnet test` task into a debug session: it
//!   builds the project, starts `dotnet test` with `VSTEST_HOST_DEBUG=1` so the test host
//!   prints its PID and waits, and attaches netcoredbg to that process.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use collections::HashMap;
use dap::adapters::{
    AdapterVersion, DapDelegate, DebugAdapter, DebugAdapterBinary, DebugAdapterName,
    DebugTaskDefinition, DownloadedFileType, download_adapter_from_github,
};
use dap::{DapLocator, DebugRequest, StartDebuggingRequestArguments};
use futures::{AsyncBufReadExt as _, StreamExt as _, io::BufReader};
use gpui::{AsyncApp, BackgroundExecutor, SharedString};
use http_client::github::latest_github_release;
use language::LanguageName;
use serde_json::{Value, json};
use task::{AttachRequest, BuildTaskDefinition, DebugScenario, SpawnInTerminal, TaskTemplate, ZedDebugConfig};
use util::command::{Stdio, new_command};

pub const ADAPTER_NAME: &str = "netcoredbg";
const REPO: &str = "Samsung/netcoredbg";
const LOCATOR_NAME: &str = "dotnet-test-locator";
/// Carries the `--filter` of the original `dotnet test` task through the build step.
const FILTER_ENV: &str = "FORGE_DOTNET_TEST_FILTER";
const PID_TIMEOUT: Duration = Duration::from_secs(120);

#[cfg(windows)]
const BINARY: &str = "netcoredbg.exe";
#[cfg(not(windows))]
const BINARY: &str = "netcoredbg";

pub struct NetcoredbgAdapter;

impl NetcoredbgAdapter {
    fn asset() -> Option<(&'static str, DownloadedFileType)> {
        Some(match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => ("netcoredbg-osx-arm64.zip", DownloadedFileType::Zip),
            ("linux", "x86_64") => ("netcoredbg-linux-amd64.tar.gz", DownloadedFileType::GzipTar),
            ("linux", "aarch64") => ("netcoredbg-linux-arm64.tar.gz", DownloadedFileType::GzipTar),
            ("windows", "x86_64") => ("netcoredbg-win64.zip", DownloadedFileType::Zip),
            _ => return None,
        })
    }

    async fn find_or_install(
        delegate: &Arc<dyn DapDelegate>,
        user_installed_path: Option<PathBuf>,
    ) -> Result<PathBuf> {
        if let Some(path) = user_installed_path.filter(|path| path.exists()) {
            return Ok(path);
        }
        if let Some(path) = delegate.which(BINARY.as_ref()).await {
            return Ok(path);
        }

        let (asset_name, file_type) =
            Self::asset().context("netcoredbg has no release for this platform; install it and put it on the PATH")?;
        let release = latest_github_release(REPO, true, false, delegate.http_client()).await?;
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .with_context(|| format!("netcoredbg release {} has no {asset_name}", release.tag_name))?;
        let version = AdapterVersion {
            tag_name: release.tag_name.clone(),
            url: asset.browser_download_url.clone(),
        };
        let dir = download_adapter_from_github(
            DebugAdapterName(ADAPTER_NAME.into()),
            version,
            file_type,
            delegate.as_ref(),
        )
        .await?;
        let binary = dir.join("netcoredbg").join(BINARY);
        util::fs::make_file_executable(&binary).await?;
        Ok(binary)
    }
}

#[async_trait(?Send)]
impl DebugAdapter for NetcoredbgAdapter {
    fn name(&self) -> DebugAdapterName {
        DebugAdapterName(ADAPTER_NAME.into())
    }

    fn adapter_language_name(&self) -> Option<LanguageName> {
        Some(LanguageName::new_static("C#"))
    }

    async fn config_from_zed_format(&self, zed_scenario: ZedDebugConfig) -> Result<DebugScenario> {
        let config = match &zed_scenario.request {
            DebugRequest::Attach(attach) => json!({
                "request": "attach",
                "processId": attach.process_id,
            }),
            DebugRequest::Launch(launch) => {
                let mut config = json!({
                    "request": "launch",
                    "program": launch.program,
                    "args": launch.args,
                    "env": launch.env_json(),
                    "stopAtEntry": zed_scenario.stop_on_entry.unwrap_or(false),
                });
                if let Some(cwd) = &launch.cwd {
                    config["cwd"] = cwd.to_string_lossy().into_owned().into();
                }
                config
            }
        };
        Ok(DebugScenario {
            adapter: zed_scenario.adapter,
            label: zed_scenario.label,
            build: None,
            config,
            tcp_connection: None,
        })
    }

    fn dap_schema(&self) -> Value {
        json!({
            "oneOf": [
                {
                    "type": "object",
                    "required": ["request", "program"],
                    "properties": {
                        "request": { "enum": ["launch"] },
                        "program": { "type": "string", "description": "Program to start, e.g. `dotnet` or an app host." },
                        "args": { "type": "array", "items": { "type": "string" } },
                        "cwd": { "type": "string" },
                        "env": { "type": "object" },
                        "stopAtEntry": { "type": "boolean", "default": false },
                        "justMyCode": { "type": "boolean", "default": true }
                    }
                },
                {
                    "type": "object",
                    "required": ["request", "processId"],
                    "properties": {
                        "request": { "enum": ["attach"] },
                        "processId": { "type": "number" },
                        "justMyCode": { "type": "boolean", "default": true }
                    }
                }
            ]
        })
    }

    async fn get_binary(
        &self,
        delegate: &Arc<dyn DapDelegate>,
        config: &DebugTaskDefinition,
        user_installed_path: Option<PathBuf>,
        user_args: Option<Vec<String>>,
        user_env: Option<HashMap<String, String>>,
        _: &mut AsyncApp,
    ) -> Result<DebugAdapterBinary> {
        let binary = Self::find_or_install(delegate, user_installed_path).await?;
        let mut envs = delegate.shell_env().await;
        envs.extend(user_env.unwrap_or_default());
        Ok(DebugAdapterBinary {
            command: Some(binary.to_string_lossy().into_owned()),
            arguments: user_args.unwrap_or_else(|| vec!["--interpreter=vscode".into()]),
            envs,
            cwd: Some(delegate.worktree_root_path().to_path_buf()),
            connection: None,
            request_args: StartDebuggingRequestArguments {
                request: self.request_kind(&config.config).await?,
                configuration: config.config.clone(),
            },
        })
    }
}

pub struct DotnetTestLocator;

#[async_trait]
impl DapLocator for DotnetTestLocator {
    fn name(&self) -> SharedString {
        SharedString::new_static(LOCATOR_NAME)
    }

    async fn create_scenario(
        &self,
        build_config: &TaskTemplate,
        resolved_label: &str,
        adapter: &DebugAdapterName,
    ) -> Option<DebugScenario> {
        if build_config.command != "dotnet" || build_config.args.first()? != "test" {
            return None;
        }
        // The gutter task quotes its args for the shell; the build step quotes them itself.
        let project = build_config
            .args
            .get(1)
            .filter(|arg| !arg.starts_with('-'))
            .map(|arg| unquote(arg).to_string());
        let filter = build_config
            .args
            .iter()
            .skip_while(|arg| *arg != "--filter")
            .nth(1)
            .map(|arg| unquote(arg).to_string());

        let mut build = build_config.clone();
        build.label = format!("dotnet build {}", project.as_deref().unwrap_or_default());
        build.args = ["build".to_string()].into_iter().chain(project).collect();
        if let Some(filter) = filter {
            build.env.insert(FILTER_ENV.into(), filter);
        }
        Some(DebugScenario {
            adapter: adapter.0.clone(),
            label: resolved_label.to_string().into(),
            build: Some(BuildTaskDefinition::Template {
                task_template: build,
                locator_name: Some(self.name()),
            }),
            config: Value::Null,
            tcp_connection: None,
        })
    }

    async fn run(
        &self,
        build_config: SpawnInTerminal,
        executor: BackgroundExecutor,
    ) -> Result<DebugRequest> {
        let project = build_config.args.get(1).map(|arg| unquote(arg).to_string());
        let filter = build_config.env.get(FILTER_ENV).map(|filter| unquote(filter).to_string());
        let framework = project
            .as_deref()
            .and_then(|project| target_frameworks(Path::new(project)).pop());

        let mut args = vec!["test".to_string()];
        args.extend(project);
        args.push("--no-build".into());
        if let Some(framework) = framework {
            // With several target frameworks every test host would wait for a debugger.
            args.extend(["--framework".into(), framework]);
        }
        if let Some(filter) = filter {
            args.extend(["--filter".into(), filter]);
        }
        log::info!("debugging tests: dotnet {}", args.join(" "));

        let mut command = new_command("dotnet");
        command
            .args(&args)
            .envs(build_config.env.iter().filter(|(key, _)| *key != FILTER_ENV))
            .env("VSTEST_HOST_DEBUG", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &build_config.cwd {
            command.current_dir(cwd);
        }
        // With SIGCHLD blocked, dotnet would never see its test host exit (see forge_ui::process).
        let mut child = forge_ui::process::spawn_unblocked(command).context("failed to start `dotnet test`")?;
        let stdout = child.stdout.take().context("no stdout from `dotnet test`")?;
        let mut lines = BufReader::new(stdout).lines();

        let pid = smol::future::or(
            async {
                while let Some(line) = lines.next().await {
                    let line = line?;
                    log::info!("dotnet test: {line}");
                    if let Some(pid) = parse_test_host_pid(&line) {
                        return Ok(pid);
                    }
                }
                anyhow::bail!("`dotnet test` exited before starting a test host; is the filter right?")
            },
            async {
                executor.timer(PID_TIMEOUT).await;
                anyhow::bail!("timed out waiting for the test host to start")
            },
        )
        .await?;

        // Keep draining the test run's output so it never blocks on a full pipe, and
        // reap the process once the tests finish.
        executor
            .spawn(async move {
                while let Some(Ok(line)) = lines.next().await {
                    log::info!("dotnet test: {line}");
                }
                child.status().await.ok();
            })
            .detach();

        Ok(DebugRequest::Attach(AttachRequest { process_id: Some(pid) }))
    }
}

const RUN_LOCATOR_NAME: &str = "dotnet-run-locator";
/// Carries the launch profile's arguments (JSON) from the scenario to the launch.
const LAUNCH_ARGS_ENV: &str = "FORGE_LAUNCH_ARGS";

/// Debugs `dotnet run --project X [--launch-profile P]`: builds the project, asks MSBuild
/// for the program it produced and launches that under netcoredbg, with the launch
/// profile's environment and arguments (which `dotnet run` would have applied).
pub struct DotnetRunLocator;

#[async_trait]
impl DapLocator for DotnetRunLocator {
    fn name(&self) -> SharedString {
        SharedString::new_static(RUN_LOCATOR_NAME)
    }

    async fn create_scenario(
        &self,
        build_config: &TaskTemplate,
        resolved_label: &str,
        adapter: &DebugAdapterName,
    ) -> Option<DebugScenario> {
        if build_config.command != "dotnet" || build_config.args.first()? != "run" {
            return None;
        }
        let project = build_config
            .args
            .iter()
            .skip_while(|arg| *arg != "--project")
            .nth(1)
            .map(|arg| unquote(arg).to_string());
        let profile_name = build_config.args.iter().skip_while(|arg| *arg != "--launch-profile").nth(1).map(|arg| unquote(arg).to_string());
        let profile = project
            .as_deref()
            .and_then(|p| Path::new(p).parent())
            .and_then(|dir| crate::launch_settings::profile(dir, profile_name.as_deref()));
        let mut build = build_config.clone();
        build.label = format!("dotnet build {}", project.as_deref().unwrap_or_default());
        build.args = ["build".to_string()].into_iter().chain(project).collect();
        if let Some(profile) = profile {
            for (key, value) in profile.env {
                build.env.entry(key).or_insert(value);
            }
            if !profile.args.is_empty() {
                build.env.insert(LAUNCH_ARGS_ENV.into(), serde_json::to_string(&profile.args).unwrap_or_default());
            }
        }
        Some(DebugScenario {
            adapter: adapter.0.clone(),
            label: resolved_label.to_string().into(),
            build: Some(BuildTaskDefinition::Template {
                task_template: build,
                locator_name: Some(self.name()),
            }),
            config: Value::Null,
            tcp_connection: None,
        })
    }

    async fn run(&self, build_config: SpawnInTerminal, _: BackgroundExecutor) -> Result<DebugRequest> {
        let project = build_config.args.get(1).map(|arg| PathBuf::from(unquote(arg)));
        let mut command = new_command("dotnet");
        command.arg("msbuild");
        if let Some(project) = &project {
            command.arg(project);
        }
        command.arg("-getProperty:TargetPath");
        // A multi-targeted project has no TargetPath of its own; pick one framework.
        if let Some(framework) = project.as_deref().and_then(|p| target_frameworks(p).pop()) {
            command.arg(format!("-p:TargetFramework={framework}"));
        }
        if let Some(cwd) = &build_config.cwd {
            command.current_dir(cwd);
        }
        let output = forge_ui::process::spawn_unblocked(command).context("failed to run `dotnet msbuild`")?.output().await.context("`dotnet msbuild` failed")?;
        anyhow::ensure!(output.status.success(), "`dotnet msbuild -getProperty:TargetPath` failed: {}", String::from_utf8_lossy(&output.stdout));
        let program = target_path(&String::from_utf8_lossy(&output.stdout)).context("MSBuild reported no TargetPath")?;
        let cwd = project.as_deref().and_then(Path::parent).map(Path::to_path_buf).or(build_config.cwd.clone());
        let mut env = build_config.env;
        let profile_args: Vec<String> = env.remove(LAUNCH_ARGS_ENV).and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default();
        Ok(DebugRequest::Launch(task::LaunchRequest {
            program: "dotnet".into(),
            args: std::iter::once(program).chain(profile_args).collect(),
            cwd,
            env: env.into_iter().collect(),
        }))
    }
}

/// `dotnet msbuild -getProperty:X` prints the bare value for one property.
fn target_path(stdout: &str) -> Option<String> {
    let path = stdout.trim();
    (!path.is_empty() && !path.starts_with('{')).then(|| path.to_string())
}

/// `Process Id: 25250, Name: dotnet`, printed by the test host under `VSTEST_HOST_DEBUG`.
fn parse_test_host_pid(line: &str) -> Option<u32> {
    let rest = line.trim().strip_prefix("Process Id:")?;
    rest.split(',').next()?.trim().parse().ok()
}

/// Gutter task args are quoted for the shell (see `csharp_tasks`).
fn unquote(arg: &str) -> &str {
    arg.strip_prefix('"')
        .and_then(|arg| arg.strip_suffix('"'))
        .unwrap_or(arg)
}

/// `<TargetFramework>` or `<TargetFrameworks>` of a project, in declaration order.
fn target_frameworks(project: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(project) else {
        return Vec::new();
    };
    ["TargetFrameworks", "TargetFramework"]
        .iter()
        .find_map(|tag| {
            let start = text.find(&format!("<{tag}>"))? + tag.len() + 2;
            let end = start + text[start..].find(&format!("</{tag}>"))?;
            Some(
                text[start..end]
                    .split(';')
                    .map(str::trim)
                    .filter(|framework| !framework.is_empty() && !framework.contains('$'))
                    .map(String::from)
                    .collect(),
            )
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_test_host_pid() {
        assert_eq!(parse_test_host_pid("Process Id: 25250, Name: dotnet"), Some(25250));
        assert_eq!(parse_test_host_pid("A total of 1 test files matched"), None);
    }

    #[test]
    fn reads_target_frameworks() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("Tests.csproj");
        std::fs::write(
            &project,
            "<Project><PropertyGroup><TargetFrameworks>net8.0;net9.0;net10.0</TargetFrameworks></PropertyGroup></Project>",
        )
        .unwrap();
        assert_eq!(target_frameworks(&project), ["net8.0", "net9.0", "net10.0"]);
        assert_eq!(unquote("\"/a b/Tests.csproj\""), "/a b/Tests.csproj");
    }

    #[test]
    fn turns_dotnet_test_into_a_build_and_attach() {
        let template = TaskTemplate {
            label: "dotnet test X".into(),
            command: "dotnet".into(),
            args: vec!["test".into(), "\"/p/Tests.csproj\"".into(), "--filter".into(), "\"FullyQualifiedName=A.B\"".into()],
            ..TaskTemplate::default()
        };
        let adapter = DebugAdapterName(ADAPTER_NAME.into());
        let scenario = futures::executor::block_on(DotnetTestLocator.create_scenario(&template, "dotnet test X", &adapter))
            .unwrap();
        let Some(BuildTaskDefinition::Template { task_template, locator_name }) = scenario.build else {
            panic!("expected a build template");
        };
        assert_eq!(locator_name.as_ref().map(|name| name.as_ref()), Some(LOCATOR_NAME));
        assert_eq!(task_template.args, ["build", "/p/Tests.csproj"]);
        assert_eq!(task_template.env.get(FILTER_ENV).map(String::as_str), Some("FullyQualifiedName=A.B"));

        let other = TaskTemplate { command: "cargo".into(), args: vec!["test".into()], ..TaskTemplate::default() };
        assert!(futures::executor::block_on(DotnetTestLocator.create_scenario(&other, "", &adapter)).is_none());

        let run = TaskTemplate {
            command: "dotnet".into(),
            args: vec!["run".into(), "--project".into(), "\"/p/App.csproj\"".into()],
            ..TaskTemplate::default()
        };
        let scenario = futures::executor::block_on(DotnetRunLocator.create_scenario(&run, "dotnet run App", &adapter)).unwrap();
        let Some(BuildTaskDefinition::Template { task_template, .. }) = scenario.build else {
            panic!("expected a build template");
        };
        assert_eq!(task_template.args, ["build", "/p/App.csproj"]);
        assert!(futures::executor::block_on(DotnetRunLocator.create_scenario(&template, "", &adapter)).is_none(), "tests are not runs");
        assert_eq!(target_path("/p/bin/App.dll\n").as_deref(), Some("/p/bin/App.dll"));
    }

    /// Debugging a launch profile carries its environment and arguments to the launch.
    #[test]
    fn debugs_with_the_launch_profile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Properties")).unwrap();
        std::fs::write(
            dir.path().join("Properties/launchSettings.json"),
            r#"{"profiles": {"http": {"commandName": "Project", "applicationUrl": "http://localhost:5000"},
                "seed": {"commandName": "Project", "commandLineArgs": "--seed 3", "environmentVariables": {"MODE": "seed"}}}}"#,
        )
        .unwrap();
        let project = dir.path().join("App.csproj");
        let adapter = DebugAdapterName("netcoredbg".into());
        let build_env = |args: Vec<String>| {
            let run = TaskTemplate { command: "dotnet".into(), args, ..TaskTemplate::default() };
            let scenario = futures::executor::block_on(DotnetRunLocator.create_scenario(&run, "", &adapter)).unwrap();
            let Some(BuildTaskDefinition::Template { task_template, .. }) = scenario.build else { panic!("expected a build template") };
            task_template.env
        };
        let quoted = format!("\"{}\"", project.display());
        let default = build_env(vec!["run".into(), "--project".into(), quoted.clone()]);
        assert_eq!(default.get("ASPNETCORE_URLS").map(String::as_str), Some("http://localhost:5000"), "the first profile, like dotnet run");
        let seed = build_env(vec!["run".into(), "--project".into(), quoted, "--launch-profile".into(), "\"seed\"".into()]);
        assert_eq!(seed.get("MODE").map(String::as_str), Some("seed"));
        assert_eq!(seed.get(LAUNCH_ARGS_ENV).map(String::as_str), Some(r#"["--seed","3"]"#));
    }
}
