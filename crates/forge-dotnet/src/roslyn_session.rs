//! What Forge tells the Roslyn C# language server, and what it answers it.
//!
//! Roslyn loads nothing on its own: once it starts, the client says which solution
//! (`solution/open`) or projects (`project/open`) to load. Forge opens the one the Solution
//! Explorer shows. `solution/open` only adds to what is loaded, so another solution starts
//! a new server, which then opens that one alone. When a project needs a restore before it
//! can load (new or changed packages), Roslyn asks the client to run it
//! (`workspace/_roslyn_projectNeedsRestore`) and reloads when the answer comes.
//!
//! An exception while Roslyn handles a notification ends its process, and Zed doesn't notice:
//! requests just fail from then on. So Forge watches the process and starts it again.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dotnet_model::solution::{Solution, SolutionFormat};
use gpui::{App, Entity};
use language::{Buffer, LanguageServerName};
use lsp::{IoKind, LanguageServer, LanguageServerId, LanguageServerSelector};
use project::Project;
use serde::{Deserialize, Serialize};

pub(crate) const ROSLYN: &str = "roslyn";
/// How often to check that the servers are still running.
pub(crate) const WATCH_INTERVAL: Duration = Duration::from_secs(3);
/// A server that keeps dying is left alone: restarts within this window count together.
const RESTART_WINDOW: Duration = Duration::from_secs(10 * 60);
const MAX_RESTARTS: usize = 3;

enum SolutionOpen {}

impl lsp::notification::Notification for SolutionOpen {
    type Params = SolutionOpenParams;
    const METHOD: &'static str = "solution/open";
}

#[derive(Debug, Serialize, Deserialize)]
struct SolutionOpenParams {
    solution: lsp::Uri,
}

enum ProjectOpen {}

impl lsp::notification::Notification for ProjectOpen {
    type Params = ProjectOpenParams;
    const METHOD: &'static str = "project/open";
}

#[derive(Debug, Serialize, Deserialize)]
struct ProjectOpenParams {
    projects: Vec<lsp::Uri>,
}

enum ProjectInitializationComplete {}

impl lsp::notification::Notification for ProjectInitializationComplete {
    type Params = Option<serde_json::Value>;
    const METHOD: &'static str = "workspace/projectInitializationComplete";
}

enum ProjectNeedsRestore {}

impl lsp::request::Request for ProjectNeedsRestore {
    type Params = RestoreParams;
    type Result = ();
    const METHOD: &'static str = "workspace/_roslyn_projectNeedsRestore";
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RestoreParams {
    #[serde(default)]
    project_file_paths: Vec<PathBuf>,
}

/// What the server should load.
#[derive(Clone, Debug, PartialEq)]
enum Target {
    Solution(PathBuf),
    Projects(Vec<PathBuf>),
}

impl Target {
    fn of(solution: &Solution) -> Option<Self> {
        match solution.format {
            SolutionFormat::Sln | SolutionFormat::Slnx => Some(Target::Solution(solution.path.clone())),
            SolutionFormat::Folder => {
                let projects: Vec<PathBuf> = solution.all_projects().into_iter().map(|p| p.path.clone()).collect();
                (!projects.is_empty()).then_some(Target::Projects(projects))
            }
        }
    }
}

/// How a server that has loaded `opened` gets to `target`.
#[derive(Debug, PartialEq)]
enum Step {
    Nothing,
    Send,
    Restart,
}

fn step(opened: Option<&Target>, target: &Target) -> Step {
    match (opened, target) {
        (Some(opened), target) if opened == target => Step::Nothing,
        (None, _) => Step::Send,
        // The projects of a folder come and go, and `project/open` takes the new ones.
        (Some(Target::Projects(_)), Target::Projects(_)) => Step::Send,
        (Some(_), _) => Step::Restart,
    }
}

/// Whether another restart fits in the window, recording it when it does.
fn allow_restart(restarts: &mut Vec<Instant>, now: Instant) -> bool {
    restarts.retain(|restart| now.duration_since(*restart) < RESTART_WINDOW);
    let allowed = restarts.len() < MAX_RESTARTS;
    if allowed {
        restarts.push(now);
    }
    allowed
}

#[derive(Default)]
struct RunningServer {
    /// What it was told to load.
    opened: Option<Target>,
    /// Being replaced by a new server, or dead and not restarted: nothing more to do with it.
    finished: bool,
}

/// The Roslyn servers running for one workspace.
#[derive(Default)]
pub(crate) struct RoslynSession {
    servers: HashMap<LanguageServerId, RunningServer>,
    restarts: Vec<Instant>,
}

impl RoslynSession {
    /// Tells every running Roslyn server to load `solution`, unless it already did. Servers
    /// seen for the first time get their requests answered from then on.
    pub(crate) fn open(&mut self, project: &Entity<Project>, solution: Option<&Solution>, cx: &mut App) {
        self.sync_servers(project, cx);
        let Some(target) = solution.and_then(Target::of) else { return };
        let mut restart = false;
        for (id, running) in &mut self.servers {
            if running.finished {
                continue;
            }
            match step(running.opened.as_ref(), &target) {
                Step::Nothing => {}
                Step::Restart => restart = true,
                Step::Send => {
                    let Some(server) = server(project, *id, cx) else { continue };
                    let sent = match &target {
                        Target::Solution(path) => {
                            log::info!("Roslyn: opening {}", path.display());
                            lsp::Uri::from_file_path(path).ok().map(|solution| server.notify::<SolutionOpen>(SolutionOpenParams { solution }))
                        }
                        Target::Projects(paths) => {
                            log::info!("Roslyn: opening {} project(s)", paths.len());
                            let projects = paths.iter().filter_map(|p| lsp::Uri::from_file_path(p).ok()).collect();
                            Some(server.notify::<ProjectOpen>(ProjectOpenParams { projects }))
                        }
                    };
                    match sent {
                        Some(Ok(())) => running.opened = Some(target.clone()),
                        Some(Err(error)) => log::warn!("Roslyn: {error:#}"),
                        None => log::warn!("Roslyn: {target:?} is not a file path"),
                    }
                }
            }
        }
        if restart {
            log::info!("Roslyn: restarting to load {target:?} alone");
            self.restart(project, cx);
        }
    }

    /// Starts the servers again if one of them has died.
    pub(crate) fn watch(&mut self, project: &Entity<Project>, cx: &mut App) {
        self.sync_servers(project, cx);
        let mut died = false;
        for (id, running) in &mut self.servers {
            if running.finished {
                continue;
            }
            if let Some(status) = server(project, *id, cx).and_then(|server| server.exit_status()) {
                log::error!("Roslyn: the language server exited ({status})");
                running.finished = true;
                died = true;
            }
        }
        if !died {
            return;
        }
        if allow_restart(&mut self.restarts, Instant::now()) {
            self.restart(project, cx);
        } else {
            log::error!("Roslyn: not restarting, it exited {MAX_RESTARTS} times in {} minutes", RESTART_WINDOW.as_secs() / 60);
            project.update(cx, |_, cx| {
                cx.emit(project::Event::Toast {
                    notification_id: "roslyn-keeps-exiting".into(),
                    message: "The C# language server keeps exiting, so Forge stopped restarting it. See the Forge log for its errors.".into(),
                    link: None,
                });
            });
        }
    }

    /// Follows the servers that came and went.
    fn sync_servers(&mut self, project: &Entity<Project>, cx: &App) {
        let running: Vec<LanguageServerId> = project.read(cx).language_server_statuses(cx).filter(|(_, status)| status.name.0.as_ref() == ROSLYN).map(|(id, _)| id).collect();
        self.servers.retain(|id, _| running.contains(id));
        for id in running {
            if !self.servers.contains_key(&id)
                && let Some(server) = server(project, id, cx)
            {
                handle_requests(&server);
                self.servers.insert(id, RunningServer::default());
            }
        }
    }

    /// Replaces the servers with new ones for the open C# files. The new ones report as
    /// `LanguageServerAdded`, and `open` tells them what to load.
    fn restart(&mut self, project: &Entity<Project>, cx: &mut App) {
        for running in self.servers.values_mut() {
            running.finished = true;
        }
        let buffers = project_buffers(project, cx);
        let name = LanguageServerName::new_static(ROSLYN);
        project.update(cx, |project, cx| {
            project.restart_language_servers_for_buffers(buffers, HashSet::from_iter([LanguageServerSelector::Name(name)]), true, cx);
        });
    }
}

/// The C# files of the workspace's folders. Decompiled sources sit in hidden worktrees of
/// their own, and each of those would start another server loading the whole solution.
fn project_buffers(project: &Entity<Project>, cx: &App) -> Vec<Entity<Buffer>> {
    let project = project.read(cx);
    project
        .buffer_store()
        .read(cx)
        .buffers()
        .filter(|buffer| {
            let buffer = buffer.read(cx);
            let is_csharp = buffer.language().is_some_and(|language| language.name().as_ref() == "C#");
            let in_workspace = buffer.file().and_then(|file| project.worktree_for_id(file.worktree_id(cx), cx)).is_some_and(|worktree| worktree.read(cx).is_visible());
            is_csharp && in_workspace
        })
        .collect()
}

fn server(project: &Entity<Project>, id: LanguageServerId, cx: &App) -> Option<Arc<LanguageServer>> {
    project.read(cx).lsp_store().read(cx).language_server_for_id(id)
}

fn handle_requests(server: &LanguageServer) {
    server
        .on_notification::<ProjectInitializationComplete, _>(|_, _| log::info!("Roslyn: projects loaded"))
        .detach();
    server
        .on_request::<ProjectNeedsRestore, _, _>(|params, _| async move {
            for project in &params.project_file_paths {
                log::info!("Roslyn: restoring {}", project.display());
                crate::restore::restore(project).await;
            }
            Ok(())
        })
        .detach();
    // Roslyn only writes to stderr when it fails: this is the stack trace of what ended it.
    server
        .on_io(|kind, message| {
            if matches!(kind, IoKind::StdErr) {
                log::error!("Roslyn: {}", message.trim_end());
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_solutions_or_the_projects_of_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("App.sln"), include_str!("../../dotnet-model/tests/fixtures/sample.sln")).unwrap();
        let solution = Solution::load(&root.join("App.sln")).unwrap();
        assert_eq!(Target::of(&solution), Some(Target::Solution(root.join("App.sln"))));

        let params: RestoreParams = serde_json::from_str(r#"{ "projectFilePaths": ["/src/App/App.csproj"] }"#).unwrap();
        assert_eq!(params.project_file_paths, vec![PathBuf::from("/src/App/App.csproj")]);
    }

    #[test]
    fn another_solution_needs_a_new_server() {
        let solution = |name: &str| Target::Solution(PathBuf::from(name));
        let projects = |names: &[&str]| Target::Projects(names.iter().map(PathBuf::from).collect());

        assert_eq!(step(None, &solution("A.sln")), Step::Send);
        assert_eq!(step(Some(&solution("A.sln")), &solution("A.sln")), Step::Nothing);
        assert_eq!(step(Some(&solution("A.sln")), &solution("B.sln")), Step::Restart);
        assert_eq!(step(Some(&solution("A.sln")), &projects(&["A.csproj"])), Step::Restart);
        assert_eq!(step(Some(&projects(&["A.csproj"])), &solution("A.sln")), Step::Restart);
        assert_eq!(step(Some(&projects(&["A.csproj"])), &projects(&["A.csproj"])), Step::Nothing);
        assert_eq!(step(Some(&projects(&["A.csproj"])), &projects(&["A.csproj", "B.csproj"])), Step::Send);
    }

    #[test]
    fn restarts_stop_when_a_server_keeps_dying() {
        let start = Instant::now();
        let mut restarts = Vec::new();
        for _ in 0..MAX_RESTARTS {
            assert!(allow_restart(&mut restarts, start));
        }
        assert!(!allow_restart(&mut restarts, start + Duration::from_secs(1)));
        assert!(allow_restart(&mut restarts, start + RESTART_WINDOW), "old restarts stop counting");
    }

    #[gpui::test]
    async fn another_solution_loads_alone_in_a_new_server(cx: &mut gpui::TestAppContext) {
        use futures::StreamExt as _;

        cx.update(workspace::AppState::test);
        let dir = tempfile::tempdir().unwrap();
        let load = |name: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, include_str!("../../dotnet-model/tests/fixtures/sample.sln")).unwrap();
            (Solution::load(&path).unwrap(), lsp::Uri::from_file_path(&path).unwrap())
        };
        let ((alpha, alpha_uri), (beta, beta_uri)) = (load("Alpha.sln"), load("Beta.sln"));

        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/dir", serde_json::json!({ "App.cs": "class App { }" })).await;
        let project = Project::test(fs, ["/dir".as_ref()], cx).await;
        let languages = project.read_with(cx, |project, _| project.languages().clone());
        languages.add(Arc::new(language::Language::new(
            language::LanguageConfig {
                name: "C#".into(),
                matcher: language::LanguageMatcher { path_suffixes: vec!["cs".into()], ..Default::default() }.into(),
                ..Default::default()
            },
            None,
        )));
        let mut fake_servers = languages.register_fake_lsp("C#", language::FakeLspAdapter { name: ROSLYN, ..Default::default() });
        let (_buffer, _registration) = project.update(cx, |project, cx| project.open_local_buffer_with_lsp("/dir/App.cs", cx)).await.unwrap();
        let mut first = fake_servers.next().await.unwrap();
        cx.run_until_parked();

        let mut session = RoslynSession::default();
        cx.update(|cx| session.open(&project, Some(&alpha), cx));
        assert_eq!(first.receive_notification::<SolutionOpen>().await.solution, alpha_uri);

        // Choosing it again changes nothing; choosing another replaces the server.
        cx.update(|cx| session.open(&project, Some(&alpha), cx));
        cx.update(|cx| session.open(&project, Some(&beta), cx));
        let mut second = fake_servers.next().await.unwrap();
        cx.run_until_parked();

        cx.update(|cx| session.open(&project, Some(&beta), cx));
        assert_eq!(second.receive_notification::<SolutionOpen>().await.solution, beta_uri);
        assert_eq!(session.servers.len(), 1, "the old server is gone");
    }
}
