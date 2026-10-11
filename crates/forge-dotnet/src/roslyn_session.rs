//! What Forge tells the Roslyn C# language server, and what it answers it.
//!
//! Roslyn loads nothing on its own: once it starts, the client says which solution
//! (`solution/open`) or projects (`project/open`) to load. Forge opens the one the Solution
//! Explorer shows, and opens it again in the server when another solution is chosen. When
//! a project needs a restore before it can load (new or changed packages), Roslyn asks
//! the client to run it (`workspace/_roslyn_projectNeedsRestore`) and reloads when the
//! answer comes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use dotnet_model::solution::{Solution, SolutionFormat};
use gpui::{App, Entity};
use lsp::{LanguageServer, LanguageServerId};
use project::Project;
use serde::{Deserialize, Serialize};

pub(crate) const ROSLYN: &str = "roslyn";

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

/// The Roslyn servers running for one workspace, and what each was told to load.
#[derive(Default)]
pub(crate) struct RoslynSession {
    servers: HashMap<LanguageServerId, Option<Target>>,
}

impl RoslynSession {
    /// Tells every running Roslyn server to load `solution`, unless it already did. Servers
    /// seen for the first time get their requests answered from then on.
    pub(crate) fn open(&mut self, project: &Entity<Project>, solution: Option<&Solution>, cx: &mut App) {
        let running: Vec<LanguageServerId> = project.read(cx).language_server_statuses(cx).filter(|(_, status)| status.name.0.as_ref() == ROSLYN).map(|(id, _)| id).collect();
        self.servers.retain(|id, _| running.contains(id));
        for id in running {
            if !self.servers.contains_key(&id)
                && let Some(server) = server(project, id, cx)
            {
                handle_requests(&server);
                self.servers.insert(id, None);
            }
        }
        let Some(target) = solution.and_then(Target::of) else { return };
        for (id, opened) in &mut self.servers {
            if opened.as_ref() == Some(&target) {
                continue;
            }
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
                Some(Ok(())) => *opened = Some(target.clone()),
                Some(Err(error)) => log::warn!("Roslyn: {error:#}"),
                None => log::warn!("Roslyn: {target:?} is not a file path"),
            }
        }
    }
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
}
