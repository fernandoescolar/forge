//! The .NET view of a workspace: the solutions in it, the one being shown, its projects
//! evaluated in the background, and the explorer tree. The solution explorer and the
//! NuGet manager both read it.
//!
//! Large solutions stay responsive because reloads are incremental: a changed project
//! file re-evaluates that project, a file added or removed re-lists that project's
//! files, and only solution changes reload everything. Changes that arrive while a load
//! is running wait for it instead of restarting it, so a busy file system cannot keep
//! the explorer loading forever.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use db::kvp::KeyValueStore;
use dotnet_model::explorer::{self, LoadedProjects, Node, ProjectChildren};
use dotnet_model::solution::{self, Solution};
use dotnet_model::{Project as MsBuildProject, paths};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Subscription, Task};
use project::{PathChange, Project};

use crate::config::{self, DotnetConfig};
use crate::language_server_sync::AssetsWatchers;

pub enum ModelEvent {
    /// The solution, its projects or the tree changed.
    Changed,
}

/// What has to be loaded again.
#[derive(Default, Debug)]
struct Pending {
    /// Look for solutions and load everything.
    full: bool,
    /// Projects to evaluate again (their file or an import changed).
    evaluate: HashSet<PathBuf>,
    /// Projects whose files to list again (files added or removed).
    relist: HashSet<PathBuf>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        !self.full && self.evaluate.is_empty() && self.relist.is_empty()
    }
}

pub struct DotnetModel {
    project: Entity<Project>,
    /// Solutions found in the workspace, top-level ones first.
    pub solutions: Vec<PathBuf>,
    pub solution: Option<Arc<Solution>>,
    pub projects: Arc<LoadedProjects>,
    children: ProjectChildren,
    pub tree: Option<Arc<Node>>,
    pub loading: bool,
    pub error: Option<String>,
    /// Bumped on every reload, so views can cache derived data.
    pub generation: usize,
    /// A solution chosen by the user, to load next.
    chosen: Option<PathBuf>,
    pending: Pending,
    load_task: Option<Task<()>>,
    /// Tells OmniSharp about restores (see `language_server_sync`).
    assets_watchers: AssetsWatchers,
    _subscriptions: Vec<Subscription>,
}

/// Folders whose changes never matter to the explorer (restore output aside).
const NOISE: &[&str] = &["bin", "obj", ".git", "node_modules", ".vs", ".idea", "TestResults"];

impl DotnetModel {
    pub fn new(project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&project, |this, project, event, cx| match event {
            project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_) => {
                this.pending.full = true;
                this.schedule(cx);
                this.watch_restores(cx);
            }
            project::Event::WorktreeUpdatedEntries(worktree_id, entries) => {
                let Some(root) = project.read(cx).worktree_for_id(*worktree_id, cx).map(|w| w.read(cx).abs_path().to_path_buf()) else { return };
                let changes: Vec<(PathBuf, PathChange)> = entries
                    .iter()
                    // The initial scan and lazily loaded folders report what already existed.
                    .filter(|(_, _, change)| *change != PathChange::Loaded)
                    .map(|(path, _, change)| (root.join(path.as_std_path()), *change))
                    .collect();
                if this.note_changes(&changes) {
                    this.schedule(cx);
                }
            }
            _ => {}
        });
        let mut model = Self {
            project,
            solutions: Vec::new(),
            solution: None,
            projects: Arc::new(HashMap::new()),
            children: HashMap::new(),
            tree: None,
            loading: false,
            error: None,
            generation: 0,
            chosen: None,
            pending: Pending { full: true, ..Default::default() },
            load_task: None,
            assets_watchers: AssetsWatchers::default(),
            _subscriptions: vec![subscription],
        };
        model.schedule(cx);
        model.watch_restores(cx);
        model
    }

    /// Records what changed on disk; returns whether anything needs loading.
    pub(crate) fn note_changes(&mut self, changes: &[(PathBuf, PathChange)]) -> bool {
        let before = (self.pending.full, self.pending.evaluate.len(), self.pending.relist.len());
        let solution_path = self.solution.as_ref().map(|s| s.path.clone());
        let folder_mode = self.solution.as_ref().is_some_and(|s| s.format == solution::SolutionFormat::Folder);
        for (path, change) in changes {
            let name = paths::file_name(path);
            let noisy = path.components().any(|c| NOISE.contains(&c.as_os_str().to_string_lossy().as_ref()));
            if noisy && name != "project.assets.json" {
                continue;
            }
            let ext = paths::extension(path);
            let added_or_removed = matches!(change, PathChange::Added | PathChange::Removed | PathChange::AddedOrUpdated);
            if matches!(ext.as_str(), "sln" | "slnx") {
                if Some(path) == solution_path.as_ref() || added_or_removed {
                    self.pending.full = true;
                }
                continue;
            }
            if solution::PROJECT_EXTENSIONS.contains(&ext.as_str()) && added_or_removed && folder_mode {
                self.pending.full = true;
                continue;
            }
            // Project files, imports and restore output re-evaluate the projects that read them.
            let mut evaluated = false;
            for project in self.projects.values().filter_map(|p| p.as_ref().ok()) {
                if project.watched_files().iter().any(|f| f == path) || project.assets_file() == *path {
                    self.pending.evaluate.insert(project.path.clone());
                    evaluated = true;
                }
            }
            if evaluated || !added_or_removed {
                continue;
            }
            if let Some(project) = self.project_for_file(path) {
                self.pending.relist.insert(project.path.clone());
            }
        }
        (self.pending.full, self.pending.evaluate.len(), self.pending.relist.len()) != before
    }

    fn watch_restores(&mut self, cx: &mut Context<Self>) {
        let roots = self.roots(cx);
        let project = self.project.clone();
        self.assets_watchers.sync(&project, roots, cx);
    }

    pub fn roots(&self, cx: &App) -> Vec<PathBuf> {
        self.project.read(cx).visible_worktrees(cx).map(|tree| tree.read(cx).abs_path().to_path_buf()).collect()
    }

    fn selection_key(roots: &[PathBuf]) -> String {
        format!("forge-dotnet-solution:{}", roots.iter().map(|r| r.to_string_lossy()).collect::<Vec<_>>().join("|"))
    }

    /// Shows another solution and remembers the choice for this workspace.
    pub fn select_solution(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let key = Self::selection_key(&self.roots(cx));
        let kvp = KeyValueStore::global(cx);
        let value = path.to_string_lossy().into_owned();
        cx.background_spawn(async move { kvp.write_kvp(key, value).await.ok() }).detach();
        self.chosen = Some(path);
        self.pending.full = true;
        self.schedule(cx);
    }

    /// Finds and loads everything again.
    pub fn reload(&mut self, _find_solutions: bool, cx: &mut Context<Self>) {
        self.pending.full = true;
        self.schedule(cx);
    }

    /// Loads again what Forge itself just changed: solution files reload the solution,
    /// project files and files in a project reload that project. File events would get
    /// there too, a little later.
    pub fn reload_paths(&mut self, touched: &[PathBuf], cx: &mut Context<Self>) {
        if touched.is_empty() {
            self.pending.full = true;
        }
        for path in touched {
            if matches!(paths::extension(path).as_str(), "sln" | "slnx") || self.solution.as_ref().is_none_or(|s| s.format == solution::SolutionFormat::Folder && path.is_dir()) {
                self.pending.full = true;
            } else if self.projects.contains_key(path) {
                self.pending.evaluate.insert(path.clone());
            } else if let Some(project) = self.project_for_file(path) {
                self.pending.evaluate.insert(project.path.clone());
            } else {
                self.pending.full = true;
            }
        }
        self.schedule(cx);
    }

    /// Starts loading what is pending, unless a load is running: then it waits for it.
    fn schedule(&mut self, cx: &mut Context<Self>) {
        if self.load_task.is_some() || self.pending.is_empty() {
            return;
        }
        self.loading = true;
        cx.notify();
        self.load_task = Some(cx.spawn(async move |this, cx| {
            // Changes come in bursts; gather them before loading.
            cx.background_executor().timer(Duration::from_millis(250)).await;
            let Ok(job) = this.update(cx, |this, cx| this.take_job(cx)) else { return };
            let result = cx.background_spawn(async move { job() }).await;
            this.update(cx, |this, cx| {
                this.apply(result, cx);
                this.load_task = None;
                // What changed while loading is loaded now.
                this.schedule(cx);
            })
            .ok();
        }));
    }

    fn take_job(&mut self, cx: &mut Context<Self>) -> Box<dyn FnOnce() -> LoadResult + Send> {
        let pending = std::mem::take(&mut self.pending);
        let config = config::get(cx);
        let roots = self.roots(cx);
        let chosen = self.chosen.take().or_else(|| self.solution.as_ref().map(|s| s.path.clone()));
        let saved = KeyValueStore::global(cx).read_kvp(&Self::selection_key(&roots)).ok().flatten().map(PathBuf::from);
        let current = (self.solution.clone(), self.projects.clone(), self.children.clone());
        Box::new(move || {
            if pending.full {
                load_everything(roots, chosen, saved, &config)
            } else {
                load_some(current, pending, &config)
            }
        })
    }

    fn apply(&mut self, result: LoadResult, cx: &mut Context<Self>) {
        if let Some(solutions) = result.solutions {
            self.solutions = solutions;
        }
        self.solution = result.solution.map(Arc::new);
        self.projects = Arc::new(result.projects);
        self.children = result.children;
        self.tree = self.solution.as_ref().map(|s| Arc::new(explorer::build_with(s, &self.projects, &self.children)));
        self.error = result.error;
        self.loading = !self.pending.is_empty();
        self.generation += 1;
        cx.emit(ModelEvent::Changed);
        cx.notify();
    }

    /// An evaluated project by its file.
    pub fn project(&self, path: &Path) -> Option<&MsBuildProject> {
        self.projects.get(path).and_then(|p| p.as_ref().ok())
    }

    /// Every project that evaluated, in solution order.
    pub fn loaded_projects(&self) -> Vec<MsBuildProject> {
        let Some(solution) = &self.solution else { return Vec::new() };
        solution.all_projects().into_iter().filter_map(|p| self.project(&p.path).cloned()).collect()
    }

    /// The project that contains a file (the deepest project folder above it).
    pub fn project_for_file(&self, file: &Path) -> Option<&MsBuildProject> {
        self.projects
            .values()
            .filter_map(|p| p.as_ref().ok())
            .filter(|p| paths::is_within(file, p.dir()))
            .max_by_key(|p| p.dir().components().count())
    }

    /// What a reload would do now: (everything, projects to evaluate, projects to list).
    #[cfg(test)]
    pub(crate) fn pending(&self) -> (bool, Vec<PathBuf>, Vec<PathBuf>) {
        let mut evaluate: Vec<_> = self.pending.evaluate.iter().cloned().collect();
        let mut relist: Vec<_> = self.pending.relist.iter().cloned().collect();
        evaluate.sort();
        relist.sort();
        (self.pending.full, evaluate, relist)
    }

    pub fn workspace_project(&self) -> &Entity<Project> {
        &self.project
    }
}

impl EventEmitter<ModelEvent> for DotnetModel {}

struct LoadResult {
    solutions: Option<Vec<PathBuf>>,
    solution: Option<Solution>,
    projects: LoadedProjects,
    children: ProjectChildren,
    error: Option<String>,
}

/// Evaluates projects and lists their files on every core.
fn evaluate_all(paths: Vec<PathBuf>, config: &DotnetConfig, evaluate: bool, previous: &LoadedProjects) -> Vec<(PathBuf, Result<MsBuildProject, String>, Option<Arc<Vec<Node>>>)> {
    let options = config.eval_options();
    let tree_options = config.tree_options();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(paths.len().max(1));
    let chunks: Vec<Vec<PathBuf>> = (0..threads).map(|t| paths.iter().skip(t).step_by(threads).cloned().collect()).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                let (options, tree_options) = (&options, &tree_options);
                scope.spawn(move || {
                    chunk
                        .into_iter()
                        .map(|path| {
                            let project = match previous.get(&path) {
                                Some(existing) if !evaluate => existing.clone(),
                                _ => dotnet_model::evaluate(&path, options).map_err(|e| format!("{e:#}")),
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

fn load_everything(roots: Vec<PathBuf>, chosen: Option<PathBuf>, saved: Option<PathBuf>, config: &DotnetConfig) -> LoadResult {
    let solutions: Vec<PathBuf> = roots.iter().flat_map(|root| solution::find_solutions(root, config.solution_search_depth)).collect();
    let chosen = chosen
        .filter(|p| p.is_file() && solutions.contains(p))
        .or_else(|| saved.filter(|p| solutions.contains(p)))
        .or_else(|| solutions.first().cloned());
    let loaded = match &chosen {
        Some(path) => Solution::load(path).map_err(|e| format!("{e:#}")),
        None => {
            // No solution: show the projects in the workspace.
            let root = roots.first().cloned().unwrap_or_default();
            let projects: Vec<PathBuf> = roots.iter().flat_map(|r| solution::find_projects(r, config.solution_search_depth + 2)).collect();
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
        for (path, project, nodes) in evaluate_all(paths, config, true, &LoadedProjects::new()) {
            if let Some(nodes) = nodes {
                children.insert(path.clone(), nodes);
            }
            projects.insert(path, project);
        }
    }
    LoadResult { solutions: Some(solutions), solution, projects, children, error }
}

fn load_some(current: (Option<Arc<Solution>>, Arc<LoadedProjects>, ProjectChildren), pending: Pending, config: &DotnetConfig) -> LoadResult {
    let (solution, previous, mut children) = current;
    let mut projects: LoadedProjects = (*previous).clone();
    let evaluate: Vec<PathBuf> = pending.evaluate.iter().filter(|p| projects.contains_key(*p)).cloned().collect();
    let relist: Vec<PathBuf> = pending.relist.iter().filter(|p| projects.contains_key(*p) && !pending.evaluate.contains(*p)).cloned().collect();
    for (path, project, nodes) in evaluate_all(evaluate, config, true, &previous).into_iter().chain(evaluate_all(relist, config, false, &previous)) {
        match nodes {
            Some(nodes) => children.insert(path.clone(), nodes),
            None => children.remove(&path),
        };
        projects.insert(path, project);
    }
    LoadResult { solutions: None, solution: solution.map(|s| (*s).clone()), projects, children, error: None }
}
