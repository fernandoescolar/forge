//! Project tools in the Solution Explorer: user secrets (`dotnet user-secrets`) and Entity
//! Framework Core migrations (`dotnet ef`), for the selected project.

use std::path::{Path, PathBuf};

use dotnet_model::msbuild::Project as MsBuildProject;
use gpui::{AppContext as _, Context, TaskExt as _, Window};
use task::TaskTemplate;

use super::*;

/// The project's `UserSecretsId`, as MSBuild evaluated it.
pub(crate) fn user_secrets_id(project: &MsBuildProject) -> Option<String> {
    project.properties.get("UserSecretsId").map(str::trim).filter(|id| !id.is_empty()).map(String::from)
}

/// The `UserSecretsId` written in a project file (right after `dotnet user-secrets init`,
/// before the project is evaluated again).
fn user_secrets_id_in(project_file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(project_file).ok()?;
    let start = text.find("<UserSecretsId>")? + "<UserSecretsId>".len();
    let end = text[start..].find("</UserSecretsId>")? + start;
    Some(text[start..end].trim().to_string()).filter(|id| !id.is_empty())
}

/// Where `dotnet user-secrets` keeps projects' secrets: `%APPDATA%\Microsoft\UserSecrets` on Windows,
/// `~/.microsoft/usersecrets` elsewhere.
pub(crate) fn user_secrets_root() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|appdata| PathBuf::from(appdata).join("Microsoft").join("UserSecrets"));
    }
    std::env::home_dir().map(|home| home.join(".microsoft/usersecrets"))
}

/// A project's secrets file, in that folder.
pub(crate) fn secrets_file(root: &Path, id: &str) -> PathBuf {
    root.join(id).join("secrets.json")
}

/// Whether the project uses Entity Framework Core.
pub(crate) fn uses_ef(project: &MsBuildProject) -> bool {
    project.package_references.iter().any(|p| p.name.starts_with("Microsoft.EntityFrameworkCore"))
}

/// The project `dotnet ef` starts to find the app's configuration: the project itself when
/// it runs, else the first runnable project that references it.
pub(crate) fn startup_project(project: &MsBuildProject, all: &[MsBuildProject]) -> PathBuf {
    if project.is_runnable() {
        return project.path.clone();
    }
    all.iter().find(|p| p.is_runnable() && p.project_references.iter().any(|r| r == &project.path)).map(|p| p.path.clone()).unwrap_or_else(|| project.path.clone())
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum EfCommand {
    AddMigration,
    RemoveMigration,
    ListMigrations,
    UpdateDatabase,
}

/// `dotnet ef …` for `command` (`name` for a new migration).
pub(crate) fn ef_args(command: EfCommand, project: &Path, startup: &Path, name: Option<&str>) -> Vec<String> {
    let quoted = |p: &Path| format!("\"{}\"", p.display());
    let mut args: Vec<String> = vec!["ef".into()];
    match command {
        EfCommand::AddMigration => args.extend(["migrations".into(), "add".into(), format!("\"{}\"", name.unwrap_or("Migration"))]),
        EfCommand::RemoveMigration => args.extend(["migrations".into(), "remove".into()]),
        EfCommand::ListMigrations => args.extend(["migrations".into(), "list".into()]),
        EfCommand::UpdateDatabase => args.extend(["database".into(), "update".into()]),
    }
    args.extend(["--project".into(), quoted(project)]);
    if startup != project {
        args.extend(["--startup-project".into(), quoted(startup)]);
    }
    args
}

impl SolutionExplorer {
    /// The selected project's evaluation.
    fn selected_project(&self, cx: &App) -> Option<MsBuildProject> {
        let path = self.selected_kind()?.project()?.to_path_buf();
        self.model.read(cx).project(&path).cloned()
    }

    fn run_task(&self, label: String, command: &str, args: Vec<String>, cwd: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let template = TaskTemplate { label, command: command.into(), args, cwd: Some(cwd.to_string_lossy().into_owned()), save: task::SaveStrategy::All, ..TaskTemplate::default() };
        let context = task::TaskContext { cwd: Some(cwd.to_path_buf()), ..Default::default() };
        let Some(resolved) = template.resolve_task("forge-dotnet", &context) else { return };
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| workspace.schedule_resolved_task(project::TaskSourceKind::UserInput, resolved, false, window, cx));
        }
    }

    /// Opens the project's `secrets.json`, setting up user secrets first if needed.
    pub(super) fn manage_user_secrets(&mut self, _: &ManageUserSecrets, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.selected_project(cx) else { return };
        let Some(root) = user_secrets_root() else { return };
        let existing = user_secrets_id(&project);
        let workspace = self.workspace.clone();
        let env = self.shell_env(project.dir(), cx);
        cx.spawn_in(window, async move |_, cx| {
            let id = match existing {
                Some(id) => id,
                None => {
                    let env = env.await;
                    let path = project.path.clone();
                    cx.background_spawn(async move {
                        let output = util::command::new_command("dotnet").args(["user-secrets", "init", "--project"]).arg(&path).envs(env).output().await?;
                        anyhow::ensure!(output.status.success(), "dotnet user-secrets init failed: {}", String::from_utf8_lossy(&output.stdout).trim());
                        user_secrets_id_in(&path).ok_or_else(|| anyhow::anyhow!("no UserSecretsId in {}", path.display()))
                    })
                    .await?
                }
            };
            let file = secrets_file(&root, &id);
            if !file.exists() {
                std::fs::create_dir_all(file.parent().unwrap_or(&root))?;
                std::fs::write(&file, "{\n}\n")?;
            }
            workspace.update_in(cx, |ws, window, cx| ws.open_abs_path(file, workspace::OpenOptions::default(), window, cx).detach_and_log_err(cx))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// The selected C# project's usings, every file's, into its global usings file; the
    /// changes open in a tab to review and save.
    pub(super) fn move_usings_to_global_usings(&mut self, _: &MoveUsingsToGlobalUsings, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.selected_project(cx).filter(|p| !p.is_fsharp()) else { return };
        let Some(workspace) = self.workspace.upgrade() else { return };
        let dir = project.dir().to_path_buf();
        let target = dir.join(crate::config::get(cx).global_usings_file);
        let name = project.path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let task = crate::global_usings::move_usings(workspace.read(cx).project().clone(), None, Some(dir), target.clone(), true, cx);
        cx.spawn_in(window, async move |_, cx| {
            let transaction = task.await?;
            workspace.update_in(cx, |ws, window, cx| {
                if transaction.0.is_empty() {
                    struct NothingToMove;
                    let message = format!("No file of {name} has usings to move.");
                    ws.show_toast(workspace::Toast::new(workspace::notifications::NotificationId::unique::<NothingToMove>(), message).autohide(), cx);
                } else {
                    let file = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    crate::global_usings::open_review(ws, transaction, format!("Usings of {name} → {file}"), window, cx);
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// The project folder's shell environment (`dotnet` and its tools on `PATH`).
    fn shell_env(&self, dir: &Path, cx: &mut Context<Self>) -> gpui::Task<Vec<(String, String)>> {
        let Some(project) = self.workspace.upgrade().map(|ws| ws.read(cx).project().clone()) else { return gpui::Task::ready(vec![]) };
        let dir: Arc<Path> = dir.into();
        let task = project.update(cx, |p, cx| p.environment().update(cx, |env, cx| env.directory_environment(dir, cx)));
        cx.background_spawn(async move { task.await.map(|env| env.into_iter().collect()).unwrap_or_default() })
    }

    /// Runs `dotnet ef` for the selected project, offering to install the tool first when it
    /// is missing.
    fn run_ef(&mut self, command: EfCommand, name: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.selected_project(cx) else { return };
        let all = self.model.read(cx).loaded_projects();
        let startup = startup_project(&project, &all);
        let args = ef_args(command, &project.path, &startup, name.as_deref());
        let dir = project.dir().to_path_buf();
        let env = self.shell_env(&dir, cx);
        cx.spawn_in(window, async move |this, cx| {
            let env = env.await;
            let check_dir = dir.clone();
            let installed = cx
                .background_spawn(async move {
                    util::command::new_command("dotnet").args(["ef", "--version"]).current_dir(&check_dir).envs(env).output().await.is_ok_and(|o| o.status.success())
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                if installed {
                    let label = format!("dotnet {}", args.iter().take(3).map(|a| a.trim_matches('"')).collect::<Vec<_>>().join(" "));
                    this.run_task(label, "dotnet", args, &dir, window, cx);
                    return;
                }
                let answer = window.prompt(
                    gpui::PromptLevel::Info,
                    "The Entity Framework Core tools aren't installed",
                    Some("Install dotnet-ef for your user (dotnet tool install --global dotnet-ef)? Run the migration command again once it is done."),
                    &["Install", "Cancel"],
                    cx,
                );
                cx.spawn_in(window, async move |this, cx| {
                    if answer.await.ok() == Some(0) {
                        this.update_in(cx, |this, window, cx| {
                            let args = ["tool", "install", "--global", "dotnet-ef"].map(String::from).to_vec();
                            this.run_task("Install dotnet-ef".into(), "dotnet", args, &dir, window, cx);
                        })
                        .ok();
                    }
                })
                .detach();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn add_migration(&mut self, _: &AddMigration, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        let explorer = cx.entity().downgrade();
        workspace.update(cx, |ws, cx| {
            forge_ui::pick::ask(ws, "Name of the new migration", "", window, cx, move |name, window, cx| {
                let name: String = name.chars().filter(|c| c.is_alphanumeric() || *c == '_').collect();
                if !name.is_empty() {
                    explorer.update(cx, |this, cx| this.run_ef(EfCommand::AddMigration, Some(name), window, cx)).ok();
                }
            });
        });
    }

    pub(super) fn remove_migration(&mut self, _: &RemoveMigration, window: &mut Window, cx: &mut Context<Self>) {
        self.run_ef(EfCommand::RemoveMigration, None, window, cx);
    }

    pub(super) fn list_migrations(&mut self, _: &ListMigrations, window: &mut Window, cx: &mut Context<Self>) {
        self.run_ef(EfCommand::ListMigrations, None, window, cx);
    }

    pub(super) fn update_database(&mut self, _: &UpdateDatabase, window: &mut Window, cx: &mut Context<Self>) {
        self.run_ef(EfCommand::UpdateDatabase, None, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(path: &str, output_type: Option<&str>, references: &[&str], packages: &[&str]) -> MsBuildProject {
        let mut properties = dotnet_model::msbuild::Properties::default();
        if let Some(output_type) = output_type {
            properties.set("OutputType", output_type);
        }
        MsBuildProject {
            path: path.into(),
            sdk: Some("Microsoft.NET.Sdk".into()),
            tools_version: None,
            properties,
            items: vec![],
            package_references: packages
                .iter()
                .map(|name| dotnet_model::msbuild::PackageReference {
                    name: name.to_string(),
                    version: None,
                    version_source: dotnet_model::msbuild::VersionSource::Inline,
                    defined_in: path.into(),
                    version_defined_in: None,
                })
                .collect(),
            package_versions: vec![],
            project_references: references.iter().map(PathBuf::from).collect(),
            references: vec![],
            imports: vec![],
            central_packages_file: None,
            items_file: None,
        }
    }

    #[test]
    fn ef_commands_find_the_startup_project() {
        let data = project("/s/Data/Data.csproj", None, &[], &["Microsoft.EntityFrameworkCore.SqlServer"]);
        let api = project("/s/Api/Api.csproj", Some("Exe"), &["/s/Data/Data.csproj"], &[]);
        assert!(uses_ef(&data) && !uses_ef(&api));
        let startup = startup_project(&data, &[data.clone(), api.clone()]);
        assert_eq!(startup, PathBuf::from("/s/Api/Api.csproj"), "the app that references the data project");
        assert_eq!(
            ef_args(EfCommand::AddMigration, &data.path, &startup, Some("AddUsers")),
            ["ef", "migrations", "add", "\"AddUsers\"", "--project", "\"/s/Data/Data.csproj\"", "--startup-project", "\"/s/Api/Api.csproj\""]
        );
        assert_eq!(ef_args(EfCommand::UpdateDatabase, &api.path, &api.path, None), ["ef", "database", "update", "--project", "\"/s/Api/Api.csproj\""]);
        assert_eq!(startup_project(&api, &[]), api.path, "an app is its own startup project");
    }

    #[test]
    fn finds_user_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Api.csproj");
        std::fs::write(&file, "<Project><PropertyGroup><UserSecretsId>abc-123</UserSecretsId></PropertyGroup></Project>").unwrap();
        assert_eq!(user_secrets_id_in(&file).as_deref(), Some("abc-123"));
        assert_eq!(secrets_file(Path::new("/Users/me/.microsoft/usersecrets"), "abc-123"), PathBuf::from("/Users/me/.microsoft/usersecrets/abc-123/secrets.json"));
    }
}
