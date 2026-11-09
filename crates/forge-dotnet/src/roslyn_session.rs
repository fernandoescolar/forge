//! What Forge tells the Roslyn C# language server, and what it answers it.
//!
//! Roslyn loads nothing on its own: once it starts, the client says which solution
//! (`solution/open`) or projects (`project/open`) to load. Forge opens the one the Solution
//! Explorer shows. `solution/open` only adds to what is loaded, so another solution starts
//! a new server, which then opens that one alone.
//!
//! An exception while Roslyn handles a notification ends its process, and Zed doesn't notice:
//! requests just fail from then on. So Forge watches the process and starts it again.
//!
//! In a remote project the server runs in Forge's server on the host, and all of this goes
//! through the `LspStore`, which reaches it there. What the server asks of the client runs
//! next to it (`RoslynLspAdapter::register_server_handlers`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dotnet_model::solution::{Solution, SolutionFormat};
use gpui::{App, AppContext as _, Entity, Task};
use language::{Buffer, LanguageServerName};
use lsp::notification::Notification as _;
use lsp::{LanguageServerId, LanguageServerSelector};
use project::Project;
use serde::{Deserialize, Serialize};

pub(crate) const ROSLYN: &str = "roslyn";
/// How often to check that the servers are still running.
pub(crate) const WATCH_INTERVAL: Duration = Duration::from_secs(3);
/// A server that keeps dying is left alone for a while: restarts within this window count
/// together, and it gets another chance when one of them is older than that.
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

    /// The notification that loads it: its method and parameters.
    fn notification(&self) -> Option<(&'static str, serde_json::Value)> {
        match self {
            Target::Solution(path) => {
                let solution = file_uri(path)?;
                Some((SolutionOpen::METHOD, serde_json::to_value(SolutionOpenParams { solution }).ok()?))
            }
            Target::Projects(paths) => {
                let projects = paths.iter().filter_map(|path| file_uri(path)).collect();
                Some((ProjectOpen::METHOD, serde_json::to_value(ProjectOpenParams { projects }).ok()?))
            }
        }
    }
}

/// The `file:` URI of a path of the machine the server runs on, which may have other paths
/// than this one: `from_file_path` refuses a Linux host's on Windows.
fn file_uri(path: &Path) -> Option<lsp::Uri> {
    lsp::Uri::from_file_path(path).ok().or_else(|| {
        let path = path.to_str().filter(|path| path.starts_with('/'))?;
        let mut url = url::Url::parse("file:///").ok()?;
        url.set_path(path);
        url.as_str().parse().ok()
    })
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
    /// The servers died too often and were left dead: [`Self::retry`] starts them again.
    gave_up: bool,
}

impl RoslynSession {
    /// Tells every running Roslyn server to load `solution`, unless it already did.
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
                    let Some((method, params)) = target.notification() else {
                        log::warn!("Roslyn: {target:?} is not a file path");
                        continue;
                    };
                    log::info!("Roslyn: opening {target:?}");
                    running.opened = Some(target.clone());
                    let sent = project.read(cx).lsp_store().read(cx).notify_language_server(*id, method.to_string(), params, cx);
                    cx.background_spawn(async move {
                        if let Err(error) = sent.await {
                            log::warn!("Roslyn: {error:#}");
                        }
                    })
                    .detach();
                }
            }
        }
        if restart {
            log::info!("Roslyn: restarting to load {target:?} alone");
            self.restart(project, cx);
        }
    }

    /// The servers that have died, with how they ended, for [`Self::servers_exited`].
    pub(crate) fn exited_servers(&mut self, project: &Entity<Project>, cx: &App) -> Task<Vec<(LanguageServerId, String)>> {
        self.sync_servers(project, cx);
        let lsp_store = project.read(cx).lsp_store().read(cx);
        let checks: Vec<_> = self.servers.iter().filter(|(_, running)| !running.finished).map(|(id, _)| (*id, lsp_store.language_server_exit_status(*id, cx))).collect();
        cx.background_spawn(async move {
            let mut exited = Vec::new();
            for (id, check) in checks {
                match check.await {
                    Ok(Some(status)) => exited.push((id, status)),
                    Ok(None) => {}
                    // The connection is down, and it says so itself: look again next time.
                    Err(error) => log::debug!("Roslyn: {error:#}"),
                }
            }
            exited
        })
    }

    /// Starts the servers again after some of them have died.
    pub(crate) fn servers_exited(&mut self, project: &Entity<Project>, exited: Vec<(LanguageServerId, String)>, cx: &mut App) {
        let mut died = false;
        for (id, status) in exited {
            // It may have been replaced while the answer came.
            if let Some(running) = self.servers.get_mut(&id).filter(|running| !running.finished) {
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
            log::error!("Roslyn: not restarting for now, it exited {MAX_RESTARTS} times in {} minutes", RESTART_WINDOW.as_secs() / 60);
            self.gave_up = true;
            project.update(cx, |_, cx| {
                cx.emit(project::Event::Toast {
                    notification_id: "roslyn-keeps-exiting".into(),
                    message: "The C# language server keeps exiting. Forge will start it again in a few minutes; its errors are in the Forge log.".into(),
                    link: None,
                });
            });
        }
    }

    /// Starts the servers that were left dead, once enough time has passed since they kept
    /// dying. Zed still counts a dead server as running, so until then C# has no server.
    pub(crate) fn retry(&mut self, project: &Entity<Project>, now: Instant, cx: &mut App) {
        if self.gave_up && allow_restart(&mut self.restarts, now) {
            log::info!("Roslyn: starting the language server again");
            self.gave_up = false;
            self.restart(project, cx);
        }
    }

    /// Follows the servers that came and went.
    fn sync_servers(&mut self, project: &Entity<Project>, cx: &App) {
        // A guest's servers are its host's to look after, and collab wouldn't forward this.
        if project.read(cx).is_via_collab() {
            return;
        }
        let running: Vec<LanguageServerId> = project.read(cx).language_server_statuses(cx).filter(|(_, status)| status.name.0.as_ref() == ROSLYN).map(|(id, _)| id).collect();
        self.servers.retain(|id, _| running.contains(id));
        for id in running {
            // A server that just started, by `retry` or by the user: nothing to start again.
            if !self.servers.contains_key(&id) {
                self.gave_up = false;
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
            project.restart_language_servers_for_buffers(buffers, HashSet::from_iter([LanguageServerSelector::Name(name)]), cx);
        });
    }
}

/// The C# files of the workspace's folders.
fn project_buffers(project: &Entity<Project>, cx: &App) -> Vec<Entity<Buffer>> {
    let mut buffers = forge_languages::language_servers::workspace_buffers(project, cx);
    buffers.retain(|buffer| buffer.read(cx).language().is_some_and(|language| language.name().as_ref() == "C#"));
    buffers
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn opens_solutions_or_the_projects_of_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("App.sln"), include_str!("../../dotnet-model/tests/fixtures/sample.sln")).unwrap();
        let solution = Solution::load(&root.join("App.sln")).unwrap();
        assert_eq!(Target::of(&solution), Some(Target::Solution(root.join("App.sln"))));
    }

    #[test]
    fn a_hosts_paths_become_uris() {
        assert_eq!(file_uri(Path::new("/home/me/My App/App.sln")).unwrap().as_str(), "file:///home/me/My%20App/App.sln");
        let (method, params) = Target::Solution(PathBuf::from("/src/App.sln")).notification().unwrap();
        assert_eq!(method, "solution/open");
        assert_eq!(params, serde_json::json!({ "solution": "file:///src/App.sln" }));
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

    /// A server that keeps dying is left dead, and started again when the restarts are old.
    #[gpui::test]
    async fn a_server_that_kept_dying_gets_another_chance(cx: &mut gpui::TestAppContext) {
        use futures::StreamExt as _;

        cx.update(workspace::AppState::test);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("App.sln");
        std::fs::write(&path, include_str!("../../dotnet-model/tests/fixtures/sample.sln")).unwrap();
        let solution = Solution::load(&path).unwrap();

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
        let mut servers = vec![fake_servers.next().await.unwrap()];
        cx.run_until_parked();

        let mut session = RoslynSession::default();
        let die = |session: &mut RoslynSession, cx: &mut gpui::TestAppContext| {
            cx.update(|cx| session.open(&project, Some(&solution), cx));
            let id = *session.servers.iter().find(|(_, running)| !running.finished).expect("a running server").0;
            cx.update(|cx| session.servers_exited(&project, vec![(id, "exit status: 134".to_string())], cx));
            cx.run_until_parked();
        };
        for _ in 0..MAX_RESTARTS {
            die(&mut session, cx);
            servers.push(fake_servers.next().await.unwrap());
            cx.run_until_parked();
        }
        die(&mut session, cx);
        assert!(session.gave_up);
        assert!(fake_servers.try_recv().is_err(), "left dead");

        let now = Instant::now();
        cx.update(|cx| session.retry(&project, now, cx));
        cx.run_until_parked();
        assert!(session.gave_up && fake_servers.try_recv().is_err(), "the restarts are too recent");

        cx.update(|cx| session.retry(&project, now + RESTART_WINDOW, cx));
        let mut last = fake_servers.next().await.unwrap();
        cx.run_until_parked();
        cx.update(|cx| session.open(&project, Some(&solution), cx));
        last.receive_notification::<SolutionOpen>().await;
        assert!(!session.gave_up);
        cx.update(|cx| session.retry(&project, now + RESTART_WINDOW * 2, cx));
        cx.run_until_parked();
        assert!(fake_servers.try_recv().is_err(), "a running server is left alone");
    }

    /// The command replaces a running server, starts one the user stopped, and in both cases
    /// the session tells the new server what to load. A file outside the workspace's folders
    /// doesn't get a server of its own out of it.
    #[gpui::test]
    async fn the_restart_command_replaces_running_and_stopped_servers(cx: &mut gpui::TestAppContext) {
        use futures::StreamExt as _;

        cx.update(workspace::AppState::test);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("App.sln");
        std::fs::write(&path, include_str!("../../dotnet-model/tests/fixtures/sample.sln")).unwrap();
        let solution = Solution::load(&path).unwrap();

        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/dir", serde_json::json!({ "App.cs": "class App { }" })).await;
        fs.insert_tree("/decompiled", serde_json::json!({ "Lib.cs": "class Lib { }" })).await;
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
        let (buffer, _registration) = project.update(cx, |project, cx| project.open_local_buffer_with_lsp("/dir/App.cs", cx)).await.unwrap();
        let mut first = fake_servers.next().await.unwrap();
        cx.run_until_parked();

        let mut session = RoslynSession::default();
        cx.update(|cx| session.open(&project, Some(&solution), cx));
        first.receive_notification::<SolutionOpen>().await;

        let (worktree, _) = project.update(cx, |project, cx| project.find_or_create_worktree("/decompiled/Lib.cs", false, cx)).await.unwrap();
        let hidden = worktree.read_with(cx, |worktree, _| worktree.id());
        let decompiled = project.update(cx, |project, cx| project.open_buffer((hidden, util::rel_path::rel_path("Lib.cs")), cx)).await.unwrap();
        cx.run_until_parked();
        let visible = cx.update(|cx| forge_languages::language_servers::workspace_buffers(&project, cx));
        assert!(visible.contains(&buffer) && !visible.contains(&decompiled));

        cx.update(|cx| forge_languages::language_servers::restart(&project, cx));
        let mut second = fake_servers.next().await.unwrap();
        cx.run_until_parked();
        cx.update(|cx| session.open(&project, Some(&solution), cx));
        second.receive_notification::<SolutionOpen>().await;
        assert_eq!(session.servers.len(), 1, "the old server is gone");

        project.update(cx, |project, cx| project.stop_language_servers_for_buffers(vec![buffer.clone()], Default::default(), cx));
        cx.run_until_parked();
        assert_eq!(project.read_with(cx, |project, cx| project.language_server_statuses(cx).count()), 0, "stopped");

        cx.update(|cx| forge_languages::language_servers::restart(&project, cx));
        let mut third = fake_servers.next().await.unwrap();
        cx.run_until_parked();
        cx.update(|cx| session.open(&project, Some(&solution), cx));
        third.receive_notification::<SolutionOpen>().await;
    }

    /// The whole session against a server on the host, through a fake SSH connection: there
    /// `LspStore::language_server_for_id` is `None`, and the store has to reach it another way.
    #[gpui::test]
    async fn a_remote_server_opens_restarts_and_is_watched(cx: &mut gpui::TestAppContext, server_cx: &mut gpui::TestAppContext) {
        use futures::StreamExt as _;
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().unwrap();
        let load = |name: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, include_str!("../../dotnet-model/tests/fixtures/sample.sln")).unwrap();
            (Solution::load(&path).unwrap(), lsp::Uri::from_file_path(&path).unwrap())
        };
        let ((alpha, alpha_uri), (beta, beta_uri)) = (load("Alpha.sln"), load("Beta.sln"));

        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        let server_fs = fs::FakeFs::new(server_cx.executor());
        server_fs.insert_tree("/code/app", serde_json::json!({ "App.cs": "class App { }" })).await;
        let (options, session_client, _) = remote::RemoteClient::fake_server(cx, server_cx);
        server_cx.update(remote_server::HeadlessProject::init);
        let host_languages = Arc::new(language::LanguageRegistry::new(cx.executor()));
        let headless = server_cx.new(|cx| {
            remote_server::HeadlessProject::new(
                remote_server::HeadlessAppState {
                    session: session_client,
                    fs: server_fs.clone(),
                    http_client: Arc::new(http_client::BlockedHttpClient),
                    node_runtime: node_runtime::NodeRuntime::unavailable(),
                    languages: host_languages.clone(),
                    extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                    startup_time: Instant::now(),
                },
                false,
                cx,
            )
        });
        let connection = remote::RemoteClient::connect_mock(options, cx).await;
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
        });
        let client = cx.update(|cx| client::Client::new(Arc::new(clock::FakeSystemClock::new()), http_client::FakeHttpClient::with_404_response(), cx));
        cx.update(|cx| Project::init(&client, cx));
        let user_store = cx.new(|cx| client::UserStore::new(client.clone(), cx));
        let languages = Arc::new(language::LanguageRegistry::test(cx.executor()));
        let local_fs = fs::FakeFs::new(cx.executor());
        let project = cx.update(|cx| Project::remote(connection, client, node_runtime::NodeRuntime::unavailable(), user_store, languages.clone(), local_fs, false, cx));

        let csharp = || language::LanguageConfig {
            name: "C#".into(),
            matcher: language::LanguageMatcher { path_suffixes: vec!["cs".into()], ..Default::default() }.into(),
            ..Default::default()
        };
        languages.register_test_language(csharp());
        languages.register_fake_lsp_adapter("C#", language::FakeLspAdapter { name: ROSLYN, ..Default::default() });
        host_languages.register_test_language(csharp());
        host_languages.register_fake_lsp_adapter("C#", language::FakeLspAdapter { name: ROSLYN, ..Default::default() });
        let mut fake_servers = host_languages.register_fake_lsp_server(LanguageServerName::new_static(ROSLYN), lsp::ServerCapabilities::default(), None);
        cx.run_until_parked();

        let worktree_id = project.update(cx, |project, cx| project.find_or_create_worktree("/code/app", true, cx)).await.unwrap().0.read_with(cx, |worktree, _| worktree.id());
        cx.run_until_parked();
        let (_buffer, _registration) = project.update(cx, |project, cx| project.open_buffer_with_lsp((worktree_id, util::rel_path::rel_path("App.cs")), cx)).await.unwrap();
        cx.run_until_parked();
        let mut first = fake_servers.next().await.unwrap();
        server_cx.run_until_parked();
        cx.run_until_parked();
        let first_id = project.read_with(cx, |project, cx| {
            let lsp_store = project.lsp_store();
            let (id, _) = project.language_server_statuses(cx).next().expect("the client knows the host's server");
            assert!(lsp_store.read(cx).language_server_for_id(id).is_none(), "and has no handle on it");
            id
        });

        let mut session = RoslynSession::default();
        cx.update(|cx| session.open(&project, Some(&alpha), cx));
        cx.run_until_parked();
        server_cx.run_until_parked();
        assert_eq!(first.receive_notification::<SolutionOpen>().await.solution, alpha_uri, "`solution/open` reaches the host");

        // It's alive, and the answer comes from the host.
        let exited = cx.update(|cx| session.exited_servers(&project, cx));
        cx.run_until_parked();
        server_cx.run_until_parked();
        cx.run_until_parked();
        assert_eq!(exited.await, Vec::new());

        // Another solution replaces the server, which then loads that one alone.
        cx.update(|cx| session.open(&project, Some(&beta), cx));
        cx.run_until_parked();
        server_cx.run_until_parked();
        let mut second = fake_servers.next().await.unwrap();
        server_cx.run_until_parked();
        cx.run_until_parked();
        cx.update(|cx| session.open(&project, Some(&beta), cx));
        cx.run_until_parked();
        server_cx.run_until_parked();
        assert_eq!(second.receive_notification::<SolutionOpen>().await.solution, beta_uri);
        assert_eq!(session.servers.len(), 1, "the old server is gone");
        assert!(!session.servers.contains_key(&first_id));

        // A server that died is replaced, and its successor is told what to load.
        let second_id = *session.servers.keys().next().unwrap();
        cx.update(|cx| session.servers_exited(&project, vec![(second_id, "exit status: 134".to_string())], cx));
        cx.run_until_parked();
        server_cx.run_until_parked();
        let mut third = fake_servers.next().await.unwrap();
        server_cx.run_until_parked();
        cx.run_until_parked();
        cx.update(|cx| session.open(&project, Some(&beta), cx));
        cx.run_until_parked();
        server_cx.run_until_parked();
        assert_eq!(third.receive_notification::<SolutionOpen>().await.solution, beta_uri);
        drop(headless);
    }

    /// The name of the file a definition is in.
    #[cfg(unix)]
    fn definition_file(project: &Entity<Project>, buffer: &Entity<Buffer>, name: &str, cx: &mut gpui::TestAppContext) -> Option<String> {
        let offset = buffer.read_with(cx, |buffer, _| buffer.text().rfind(name).unwrap());
        let definitions = project.update(cx, |project, cx| project.definitions(buffer, offset, cx));
        let definitions = cx.foreground_executor().block_test(definitions).ok()??;
        cx.read(|cx| definitions.first().and_then(|link| Some(link.target.buffer.read(cx).file()?.path().file_name()?.to_string())))
    }

    #[cfg(unix)]
    fn wait_for_definition(project: &Entity<Project>, buffer: &Entity<Buffer>, name: &str, file: &str, cx: &mut gpui::TestAppContext) {
        let deadline = Instant::now() + Duration::from_secs(240);
        loop {
            let found = definition_file(project, buffer, name, cx);
            if found.as_deref() == Some(file) {
                return;
            }
            assert!(Instant::now() < deadline, "{name} never led to {file}: last {found:?}");
            std::thread::sleep(Duration::from_secs(1));
            cx.run_until_parked();
        }
    }

    /// The real Roslyn behind the fake connection: go to definition between the solution's
    /// projects, another solution loading alone, and a killed server coming back. Needs the
    /// .NET SDK and Roslyn: `FORGE_TEST_ROSLYN_DLL=<its dll> cargo test -p forge-dotnet real_roslyn -- --ignored`.
    #[cfg(unix)]
    #[gpui::test]
    #[ignore = "needs the .NET SDK and FORGE_TEST_ROSLYN_DLL"]
    async fn real_roslyn_on_the_host(cx: &mut gpui::TestAppContext, server_cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        cx.executor().allow_parking();
        server_cx.executor().allow_parking();
        let dll = std::env::var("FORGE_TEST_ROSLYN_DLL").expect("FORGE_TEST_ROSLYN_DLL");
        let dotnet = std::env::var("FORGE_TEST_DOTNET").unwrap_or_else(|_| "/usr/bin/dotnet".into());
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let write = |path: &str, text: String| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        let csproj = |reference: &str| format!(r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup><ItemGroup>{reference}</ItemGroup></Project>"#);
        write("Lib/Lib.csproj", csproj(""));
        write("Lib/Greeter.cs", "namespace Lib; public class Greeter { }\n".into());
        write("Lib/User.cs", "namespace Lib; public class User { Greeter greeter; }\n".into());
        write("App/App.csproj", csproj(r#"<ProjectReference Include="../Lib/Lib.csproj" />"#));
        write("App/Program.cs", "class Program { static void Main() { var greeter = new Lib.Greeter(); } }\n".into());
        write("All.slnx", r#"<Solution><Project Path="App/App.csproj" /><Project Path="Lib/Lib.csproj" /></Solution>"#.into());
        write("Only.slnx", r#"<Solution><Project Path="Lib/Lib.csproj" /></Solution>"#.into());
        write(
            ".forge/settings.json",
            serde_json::json!({ "lsp": { "roslyn": { "binary": { "path": dotnet, "arguments": [dll, "--stdio", "--logLevel", "Information"] } } } }).to_string(),
        );
        let all = Solution::load(&root.join("All.slnx")).unwrap();
        let only = Solution::load(&root.join("Only.slnx")).unwrap();

        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        let server_fs: Arc<dyn fs::Fs> = fs::RealFs::new(None, server_cx.executor());
        let (options, session_client, _) = remote::RemoteClient::fake_server(cx, server_cx);
        server_cx.update(remote_server::HeadlessProject::init);
        let host_languages = Arc::new(language::LanguageRegistry::new(server_cx.executor()));
        let headless = server_cx.new(|cx| {
            remote_server::HeadlessProject::new(
                remote_server::HeadlessAppState {
                    session: session_client,
                    fs: server_fs,
                    http_client: Arc::new(http_client::BlockedHttpClient),
                    node_runtime: node_runtime::NodeRuntime::unavailable(),
                    languages: host_languages.clone(),
                    extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                    startup_time: Instant::now(),
                },
                false,
                cx,
            )
        });
        server_cx.update(|cx| forge_languages::register_languages(host_languages.clone(), cx));
        let connection = remote::RemoteClient::connect_mock(options, cx).await;
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
        });
        let client = cx.update(|cx| client::Client::new(Arc::new(clock::FakeSystemClock::new()), http_client::FakeHttpClient::with_404_response(), cx));
        cx.update(|cx| Project::init(&client, cx));
        let user_store = cx.new(|cx| client::UserStore::new(client.clone(), cx));
        let languages = Arc::new(language::LanguageRegistry::test(cx.executor()));
        cx.update(|cx| forge_languages::register_languages(languages.clone(), cx));
        let project = cx.update(|cx| Project::remote(connection, client, node_runtime::NodeRuntime::unavailable(), user_store, languages, fs::FakeFs::new(cx.background_executor().clone()), false, cx));

        let worktree_id = project.update(cx, |project, cx| project.find_or_create_worktree(&root, true, cx)).await.unwrap().0.read_with(cx, |worktree, _| worktree.id());
        let settle = |cx: &mut gpui::TestAppContext| {
            for _ in 0..20 {
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(100));
            }
        };
        settle(cx);
        let (program, _program_lsp) = project.update(cx, |project, cx| project.open_buffer_with_lsp((worktree_id, util::rel_path::rel_path("App/Program.cs")), cx)).await.unwrap();
        let (user, _user_lsp) = project.update(cx, |project, cx| project.open_buffer_with_lsp((worktree_id, util::rel_path::rel_path("Lib/User.cs")), cx)).await.unwrap();

        let roslyn_ids = |project: &Entity<Project>, cx: &mut gpui::TestAppContext| -> Vec<LanguageServerId> {
            project.read_with(cx, |project, cx| project.language_server_statuses(cx).filter(|(_, status)| status.name.0.as_ref() == ROSLYN).map(|(id, _)| id).collect())
        };
        let wait_for_new_server = |seen: &[LanguageServerId], cx: &mut gpui::TestAppContext| -> LanguageServerId {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                cx.run_until_parked();
                if let Some(id) = roslyn_ids(&project, cx).into_iter().find(|id| !seen.contains(id)) {
                    return id;
                }
                assert!(Instant::now() < deadline, "no new Roslyn server");
                std::thread::sleep(Duration::from_millis(200));
            }
        };

        let mut session = RoslynSession::default();
        let first = wait_for_new_server(&[], cx);
        cx.update(|cx| session.open(&project, Some(&all), cx));
        wait_for_definition(&project, &program, "Greeter", "Greeter.cs", cx);
        let deadline = Instant::now() + Duration::from_secs(120);
        while !root.join("App/obj/project.assets.json").exists() {
            assert!(Instant::now() < deadline, "the restore Roslyn asked for ran on the host");
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(200));
        }

        // Another solution: a new server with that one alone, so App is no project of it.
        cx.update(|cx| session.open(&project, Some(&only), cx));
        let second = wait_for_new_server(&[first], cx);
        settle(cx);
        cx.update(|cx| session.open(&project, Some(&only), cx));
        wait_for_definition(&project, &user, "Greeter", "Greeter.cs", cx);
        assert_eq!(definition_file(&project, &program, "Greeter", cx), None, "App isn't loaded");

        // Killed: noticed through the host, started again, and told what to load.
        let pid = server_cx.read(|cx| headless.read(cx).lsp_store.read(cx).language_server_for_id(second).and_then(|server| server.process_id())).expect("its process");
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        let deadline = Instant::now() + Duration::from_secs(30);
        let exited = loop {
            cx.run_until_parked();
            let exited = cx.update(|cx| session.exited_servers(&project, cx));
            let exited = cx.foreground_executor().block_test(exited);
            if !exited.is_empty() {
                break exited;
            }
            assert!(Instant::now() < deadline, "the death went unnoticed");
            std::thread::sleep(Duration::from_millis(200));
        };
        assert_eq!(exited[0].0, second);
        cx.update(|cx| session.servers_exited(&project, exited, cx));
        wait_for_new_server(&[first, second], cx);
        settle(cx);
        cx.update(|cx| session.open(&project, Some(&only), cx));
        wait_for_definition(&project, &user, "Greeter", "Greeter.cs", cx);

        let last = roslyn_ids(&project, cx);
        for id in last {
            if let Some(pid) = server_cx.read(|cx| headless.read(cx).lsp_store.read(cx).language_server_for_id(id).and_then(|server| server.process_id())) {
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
        }
    }

    /// The real Roslyn through what happens to files in a working day: edits, saves, other
    /// line endings, changes on disk, renames, deletions, another language, closing and
    /// opening again. Roslyn ends its process when the documents it is told about don't add
    /// up, so it has to be the same process at the end.
    /// `FORGE_TEST_ROSLYN_DLL=<its dll> cargo test -p forge-dotnet real_roslyn_survives -- --ignored`.
    #[cfg(unix)]
    #[gpui::test]
    #[ignore = "needs the .NET SDK and FORGE_TEST_ROSLYN_DLL"]
    async fn real_roslyn_survives_file_churn(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let dll = std::env::var("FORGE_TEST_ROSLYN_DLL").expect("FORGE_TEST_ROSLYN_DLL");
        let dotnet = std::env::var("FORGE_TEST_DOTNET").unwrap_or_else(|_| "/usr/bin/dotnet".into());
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let write = |path: &str, text: String| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        const USER: &str = "namespace Lib; public class User { Greeter greeter; }\n";
        write("Lib/Lib.csproj", r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup></Project>"#.into());
        write("Lib/Greeter.cs", "namespace Lib; public class Greeter { }\n".into());
        write("Lib/User.cs", USER.into());
        write("Lib/Other.cs", "namespace Lib; public class Other { }\n".into());
        write("All.slnx", r#"<Solution><Project Path="Lib/Lib.csproj" /></Solution>"#.into());
        write(
            ".forge/settings.json",
            serde_json::json!({ "lsp": { "roslyn": { "binary": { "path": dotnet, "arguments": [dll, "--stdio", "--logLevel", "Information"] } } } }).to_string(),
        );
        let solution = Solution::load(&root.join("All.slnx")).unwrap();

        cx.update(workspace::AppState::test);
        let fs: Arc<dyn fs::Fs> = fs::RealFs::new(None, cx.executor());
        let project = Project::test(fs, [root.as_path()], cx).await;
        let languages = project.read_with(cx, |project, _| project.languages().clone());
        cx.update(|cx| forge_languages::register_languages(languages.clone(), cx));
        let settle = |cx: &mut gpui::TestAppContext| {
            for _ in 0..15 {
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(100));
            }
        };
        settle(cx);
        let open = |path: &str, cx: &mut gpui::TestAppContext| {
            let opened = project.update(cx, |project, cx| project.open_local_buffer_with_lsp(root.join(path), cx));
            cx.foreground_executor().block_test(opened).unwrap()
        };
        let (mut user, mut user_lsp) = open("Lib/User.cs", cx);

        // The server and how its process ended, if it has.
        let server = |cx: &mut gpui::TestAppContext| -> Option<(LanguageServerId, Option<String>)> {
            project.read_with(cx, |project, cx| {
                let lsp_store = project.lsp_store().read(cx);
                let (id, _) = project.language_server_statuses(cx).find(|(_, status)| status.name.0.as_ref() == ROSLYN)?;
                Some((id, lsp_store.language_server_for_id(id)?.exit_status().map(|status| status.to_string())))
            })
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        let first = loop {
            cx.run_until_parked();
            if let Some((id, _)) = server(cx) {
                break id;
            }
            assert!(Instant::now() < deadline, "no Roslyn server");
            std::thread::sleep(Duration::from_millis(200));
        };
        let mut session = RoslynSession::default();
        cx.update(|cx| session.open(&project, Some(&solution), cx));
        wait_for_definition(&project, &user, "Greeter", "Greeter.cs", cx);

        let mut died: Vec<&str> = Vec::new();
        let mut current = first;
        // Whether the server outlived `step`; a new one takes over when it didn't.
        let mut check = |step: &'static str, session: &mut RoslynSession, cx: &mut gpui::TestAppContext| {
            settle(cx);
            match server(cx) {
                Some((id, None)) if id == current => {}
                state => {
                    eprintln!("Roslyn died at {step:?}: {state:?}");
                    died.push(step);
                    cx.update(|cx| forge_languages::language_servers::restart(&project, cx));
                    let deadline = Instant::now() + Duration::from_secs(60);
                    loop {
                        settle(cx);
                        if let Some((id, None)) = server(cx)
                            && id != current
                        {
                            current = id;
                            break;
                        }
                        assert!(Instant::now() < deadline, "no new Roslyn server after {step:?}");
                    }
                    cx.update(|cx| session.open(&project, Some(&solution), cx));
                    settle(cx);
                }
            }
        };
        let edit = |buffer: &Entity<Buffer>, cx: &mut gpui::TestAppContext| buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "// edited\n")], None, cx));
        let save = |buffer: &Entity<Buffer>, cx: &mut gpui::TestAppContext| {
            let saved = project.update(cx, |project, cx| project.save_buffer(buffer.clone(), cx));
            cx.foreground_executor().block_test(saved).unwrap();
        };

        edit(&user, cx);
        save(&user, cx);
        check("edit and save", &mut session, cx);

        for _ in 0..200 {
            user.update(cx, |buffer, cx| {
                let end = buffer.len();
                buffer.edit([(end..end, "// more\n")], None, cx);
                buffer.edit([(0..3, "")], None, cx);
                buffer.edit([(0..0, "// ")], None, cx);
            });
            cx.run_until_parked();
        }
        check("many edits", &mut session, cx);

        user.update(cx, |buffer, cx| buffer.set_line_ending(language::LineEnding::Windows, cx));
        edit(&user, cx);
        save(&user, cx);
        user.update(cx, |buffer, cx| buffer.set_line_ending(language::LineEnding::Unix, cx));
        save(&user, cx);
        check("other line endings", &mut session, cx);

        std::fs::write(root.join("Lib/User.cs"), "// from outside\r\nnamespace Lib; public class User { Greeter greeter; }\r\n").unwrap();
        check("changed on disk", &mut session, cx);
        edit(&user, cx);
        check("edit after the change on disk", &mut session, cx);
        save(&user, cx);

        std::fs::rename(root.join("Lib/User.cs"), root.join("Lib/Member.cs")).unwrap();
        check("renamed on disk", &mut session, cx);
        edit(&user, cx);
        save(&user, cx);
        check("edit after the rename", &mut session, cx);
        std::fs::rename(root.join("Lib/Member.cs"), root.join("Lib/User.cs")).unwrap();
        check("renamed back", &mut session, cx);

        std::fs::rename(root.join("Lib/User.cs"), root.join("Lib/User.txt")).unwrap();
        check("renamed to another language", &mut session, cx);
        edit(&user, cx);
        save(&user, cx);
        std::fs::rename(root.join("Lib/User.txt"), root.join("Lib/User.cs")).unwrap();
        check("renamed back to C#", &mut session, cx);
        edit(&user, cx);
        check("edit after coming back to C#", &mut session, cx);
        save(&user, cx);

        std::fs::remove_file(root.join("Lib/User.cs")).unwrap();
        check("deleted on disk", &mut session, cx);
        edit(&user, cx);
        check("edit after the deletion", &mut session, cx);
        std::fs::write(root.join("Lib/User.cs"), USER).unwrap();
        check("created again", &mut session, cx);
        edit(&user, cx);
        check("edit after it was created again", &mut session, cx);
        save(&user, cx);

        project.update(cx, |project, cx| project.set_language_for_buffer(&user, language::PLAIN_TEXT.clone(), cx));
        check("set to plain text", &mut session, cx);
        edit(&user, cx);
        let csharp = cx.foreground_executor().block_test(languages.language_for_name("C#")).unwrap();
        project.update(cx, |project, cx| project.set_language_for_buffer(&user, csharp, cx));
        check("set to C# again", &mut session, cx);
        edit(&user, cx);
        save(&user, cx);
        check("edit after the languages", &mut session, cx);

        drop(user_lsp);
        drop(user);
        check("closed", &mut session, cx);
        (user, user_lsp) = open("Lib/User.cs", cx);
        check("opened again", &mut session, cx);
        // A second handle on the same buffer, and one going away.
        let (again, again_lsp) = open("Lib/User.cs", cx);
        drop(again_lsp);
        drop(again);
        edit(&user, cx);
        check("opened twice", &mut session, cx);

        let (other, _other_lsp) = open("Lib/Other.cs", cx);
        std::fs::remove_file(root.join("Lib/User.cs")).unwrap();
        std::fs::rename(root.join("Lib/Other.cs"), root.join("Lib/User.cs")).unwrap();
        check("renamed over an open file", &mut session, cx);
        edit(&other, cx);
        edit(&user, cx);
        check("edits after the rename over an open file", &mut session, cx);

        std::fs::write(root.join("Lib/User.cs"), USER).unwrap();
        settle(cx);
        let reloaded = project.update(cx, |project, cx| project.reload_buffers([user.clone(), other.clone()].into_iter().collect(), false, cx));
        cx.foreground_executor().block_test(reloaded).ok();
        check("reloaded", &mut session, cx);

        cx.update(|cx| session.open(&project, Some(&solution), cx));
        check("the same solution again", &mut session, cx);

        drop(user_lsp);
        if let Some(pid) = project.read_with(cx, |project, cx| project.lsp_store().read(cx).language_server_for_id(current).and_then(|server| server.process_id())) {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
        assert!(died.is_empty(), "Roslyn died at: {died:?}");
    }
}
