//! Loading a workspace's .NET solution for the Solution Explorer: finding solutions,
//! evaluating their projects, listing their files and building the tree. It reads the disk,
//! so it runs where the files are: in Forge for a local project, on the host for a remote
//! one (`forge-dotnet`'s `host.rs`), which is why what it makes is serializable.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::explorer::{self, LoadedProjects, Node, ProjectChildren, TreeOptions};
use crate::msbuild::EvalOptions;
use crate::solution::{self, Solution};

/// How to load: the evaluation's configuration and properties, the tree's options and how
/// deep to look for solutions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoadOptions {
    pub eval: EvalOptions,
    pub tree: TreeOptions,
    pub solution_search_depth: usize,
}

/// What a load found.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Loaded {
    /// The solutions there are to choose from; `None` when only some projects reloaded.
    pub solutions: Option<Vec<PathBuf>>,
    pub solution: Option<Solution>,
    pub projects: LoadedProjects,
    pub children: ProjectChildren,
    pub error: Option<String>,
    /// The explorer's tree, made by [`Loaded::finish`].
    pub tree: Option<Node>,
    /// The projects that read each file, also made by [`Loaded::finish`].
    pub watched: HashMap<PathBuf, Vec<PathBuf>>,
}

impl Loaded {
    fn new(solutions: Option<Vec<PathBuf>>, solution: Option<Solution>, projects: LoadedProjects, children: ProjectChildren, error: Option<String>) -> Self {
        Self { solutions, solution, projects, children, error, tree: None, watched: HashMap::new() }
    }

    /// A load that failed: shown as the explorer's error.
    pub fn failed(error: String) -> Self {
        Self { error: Some(error), ..Self::default() }
    }

    /// Builds the explorer's tree and the index of watched files.
    pub fn finish(mut self) -> Self {
        self.tree = self.solution.as_ref().map(|s| explorer::build_with(s, &self.projects, &self.children));
        for project in self.projects.values().filter_map(|p| p.as_ref().ok()) {
            for file in project.watched_files().into_iter().chain([project.assets_file()]) {
                self.watched.entry(file).or_default().push(project.path.clone());
            }
        }
        self
    }
}

/// Evaluates projects and lists their files on every core.
fn evaluate_all(paths: Vec<PathBuf>, options: &LoadOptions, evaluate: bool, previous: &LoadedProjects) -> Vec<(PathBuf, Result<crate::Project, String>, Option<Arc<Vec<Node>>>)> {
    let (eval_options, tree_options) = (&options.eval, &options.tree);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(paths.len().max(1));
    let chunks: Vec<Vec<PathBuf>> = (0..threads).map(|t| paths.iter().skip(t).step_by(threads).cloned().collect()).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .into_iter()
                        .map(|path| {
                            let project = match previous.get(&path) {
                                Some(existing) if !evaluate => existing.clone(),
                                _ => crate::evaluate(&path, eval_options).map_err(|e| format!("{e:#}")),
                            };
                            let children = project.as_ref().ok().map(|p| Arc::new(explorer::project_children(p, tree_options)));
                            (path, project, children)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

/// Finds the solutions under `roots`, loads the chosen one (else the one saved for the
/// workspace, else the first; with none, the projects found) and evaluates its projects.
pub fn load_everything(roots: Vec<PathBuf>, chosen: Option<PathBuf>, saved: Option<PathBuf>, options: &LoadOptions) -> Loaded {
    let solutions: Vec<PathBuf> = roots.iter().flat_map(|root| solution::find_solutions(root, options.solution_search_depth)).collect();
    let chosen = chosen
        .filter(|p| p.is_file() && solutions.contains(p))
        .or_else(|| saved.filter(|p| solutions.contains(p)))
        .or_else(|| solutions.first().cloned());
    let loaded = match &chosen {
        Some(path) => Solution::load(path).map_err(|e| format!("{e:#}")),
        None => {
            // No solution: show the projects in the workspace.
            let root = roots.first().cloned().unwrap_or_default();
            let projects: Vec<PathBuf> = roots.iter().flat_map(|r| solution::find_projects(r, options.solution_search_depth + 2)).collect();
            if projects.is_empty() { Err(String::new()) } else { Ok(Solution::from_folder(&root, projects)) }
        }
    };
    let (solution, error) = match loaded {
        Ok(solution) => (Some(solution), None),
        Err(e) if e.is_empty() => (None, None),
        Err(e) => (None, Some(e)),
    };
    let mut projects = LoadedProjects::new();
    let mut children = ProjectChildren::new();
    if let Some(solution) = &solution {
        let paths = solution.all_projects().into_iter().map(|p| p.path.clone()).collect();
        for (path, project, nodes) in evaluate_all(paths, options, true, &LoadedProjects::new()) {
            if let Some(nodes) = nodes {
                children.insert(path.clone(), nodes);
            }
            projects.insert(path, project);
        }
    }
    Loaded::new(Some(solutions), solution, projects, children, error)
}

/// Evaluates again the projects in `evaluate` and lists again the files of those in
/// `relist`, keeping the rest of what was loaded.
pub fn load_some(
    solution: Option<Arc<Solution>>,
    previous: Arc<LoadedProjects>,
    mut children: ProjectChildren,
    evaluate: &HashSet<PathBuf>,
    relist: &HashSet<PathBuf>,
    options: &LoadOptions,
) -> Loaded {
    let mut projects: LoadedProjects = (*previous).clone();
    let to_evaluate: Vec<PathBuf> = evaluate.iter().filter(|p| projects.contains_key(*p)).cloned().collect();
    let to_relist: Vec<PathBuf> = relist.iter().filter(|p| projects.contains_key(*p) && !evaluate.contains(*p)).cloned().collect();
    for (path, project, nodes) in evaluate_all(to_evaluate, options, true, &previous).into_iter().chain(evaluate_all(to_relist, options, false, &previous)) {
        match nodes {
            Some(nodes) => children.insert(path.clone(), nodes),
            None => children.remove(&path),
        };
        projects.insert(path, project);
    }
    Loaded::new(None, solution.map(|s| (*s).clone()), projects, children, None)
}

/// Loading on a remote project's host (`forge_host`): [`load_everything`], then
/// [`Loaded::finish`], there.
pub const LOAD: &str = "dotnet.load";

#[derive(Serialize, Deserialize)]
pub struct LoadParams {
    pub roots: Vec<PathBuf>,
    pub chosen: Option<PathBuf>,
    pub saved: Option<PathBuf>,
    pub options: LoadOptions,
}

impl LoadParams {
    /// What the host answers.
    pub fn load(self) -> Loaded {
        load_everything(self.roots, self.chosen, self.saved, &self.options).finish()
    }
}
