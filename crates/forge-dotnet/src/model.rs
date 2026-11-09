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
use dotnet_model::explorer::{LoadedProjects, Node, ProjectChildren};
use dotnet_model::load::{self, LoadOptions, Loaded};
use dotnet_model::solution::{self, Solution};
use dotnet_model::{Project as MsBuildProject, paths};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Subscription, Task};
use project::{PathChange, Project};

use crate::config;
use crate::language_server_sync::AssetsWatchers;
use crate::roslyn_session::{ROSLYN, RoslynSession, WATCH_INTERVAL};
use forge_languages::active_build::ActiveBuild;

pub enum ModelEvent {
    /// The solution, its projects or the tree changed.
    Changed,
}

/// What has to be loaded again.
/// A load to run: here, or on a remote project's host.
enum Job {
    Here(Box<dyn FnOnce() -> Loaded + Send>),
    OnHost(Task<anyhow::Result<Loaded>>),
}

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
    /// The projects each file makes evaluate again when it changes (project files,
    /// imports, restore output): looked up for every changed file.
    watched: HashMap<PathBuf, Vec<PathBuf>>,
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
    /// Tells Roslyn which solution to load.
    roslyn: RoslynSession,
    _roslyn_watch: Task<()>,
    /// The solution whose saved configuration and framework were last restored.
    build_for: Option<PathBuf>,
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
            project::Event::LanguageServerAdded(_, name, _) if name.0.as_ref() == ROSLYN => {
                let solution = this.solution.clone();
                this.roslyn.open(&project, solution.as_deref(), cx);
            }
            _ => {}
        });
        // Projects evaluate in the chosen configuration: another one evaluates them again.
        let mut evaluated_build = ActiveBuild::get(cx);
        let build_subscription = cx.observe_global::<ActiveBuild>(move |this, cx| {
            let build = ActiveBuild::get(cx);
            if (&build.configuration, &build.platform) != (&evaluated_build.configuration, &evaluated_build.platform) {
                evaluated_build = build;
                this.pending.evaluate.extend(this.projects.keys().cloned());
                this.schedule(cx);
            }
        });
        let roslyn_watch = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(WATCH_INTERVAL).await;
                let Ok(exited) = this.update(cx, |this, cx| {
                    let project = this.project.clone();
                    this.roslyn.retry(&project, std::time::Instant::now(), cx);
                    this.roslyn.exited_servers(&project, cx)
                }) else {
                    break;
                };
                let exited = exited.await;
                if !exited.is_empty()
                    && this
                        .update(cx, |this, cx| {
                            let project = this.project.clone();
                            this.roslyn.servers_exited(&project, exited, cx);
                        })
                        .is_err()
                {
                    break;
                }
            }
        });
        let mut model = Self {
            project,
            solutions: Vec::new(),
            solution: None,
            projects: Arc::new(HashMap::new()),
            children: HashMap::new(),
            tree: None,
            watched: HashMap::new(),
            loading: false,
            error: None,
            generation: 0,
            chosen: None,
            pending: Pending { full: true, ..Default::default() },
            load_task: None,
            assets_watchers: AssetsWatchers::default(),
            roslyn: RoslynSession::default(),
            _roslyn_watch: roslyn_watch,
            build_for: None,
            _subscriptions: vec![subscription, build_subscription],
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
            for project in self.watched.get(path).into_iter().flatten() {
                self.pending.evaluate.insert(project.clone());
                evaluated = true;
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
        // It watches this machine's disk; a remote project's restores show up as updated
        // entries of its worktrees instead.
        if forge_ui::is_remote(self.project.read(cx)) {
            return;
        }
        let roots = self.roots(cx);
        let project = self.project.clone();
        self.assets_watchers.sync(&project, roots, cx);
    }

    /// The project's folders, where solutions are looked for (on its host, for a remote one).
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

    /// Builds, runs and tests in `build` from now on, and remembers it for this solution.
    pub fn select_build(&mut self, build: ActiveBuild, cx: &mut Context<Self>) {
        if let Some(solution) = &self.solution {
            let key = format!("forge-dotnet-build:{}", solution.path.display());
            let value = serde_json::json!({ "configuration": build.configuration, "platform": build.platform, "framework": build.framework }).to_string();
            let kvp = KeyValueStore::global(cx);
            cx.background_spawn(async move { kvp.write_kvp(key, value).await.ok() }).detach();
        }
        build.set(cx);
    }

    /// The configurations the solution defines (`Debug|Any CPU`, `Release|x64`…).
    pub fn configurations(&self) -> Vec<String> {
        self.solution.as_ref().map(|s| s.configurations.clone()).filter(|c| !c.is_empty()).unwrap_or_else(|| vec!["Debug|Any CPU".into(), "Release|Any CPU".into()])
    }

    /// Every target framework of the solution's projects, in the order first seen.
    pub fn frameworks(&self) -> Vec<String> {
        let mut frameworks: Vec<String> = Vec::new();
        for project in self.loaded_projects() {
            for framework in project.target_frameworks() {
                if !frameworks.contains(&framework) {
                    frameworks.push(framework);
                }
            }
        }
        frameworks
    }

    /// Whether some project builds for more than one framework.
    pub fn is_multi_targeted(&self) -> bool {
        self.loaded_projects().iter().any(|p| p.target_frameworks().len() > 1)
    }

    /// When another solution is shown, goes back to the configuration and framework last
    /// chosen for it (the defaults when none was).
    fn restore_build(&mut self, cx: &mut Context<Self>) {
        let path = self.solution.as_ref().map(|s| s.path.clone());
        if path == self.build_for {
            return;
        }
        self.build_for = path.clone();
        let Some(path) = path else { return };
        let saved = KeyValueStore::global(cx).read_kvp(&format!("forge-dotnet-build:{}", path.display())).ok().flatten();
        let saved: Option<serde_json::Value> = saved.and_then(|text| serde_json::from_str(&text).ok());
        let field = |name: &str| saved.as_ref().and_then(|s| s.get(name)?.as_str().map(String::from));
        let build = ActiveBuild { configuration: field("configuration"), platform: field("platform"), framework: field("framework") };
        if build != ActiveBuild::get(cx) {
            build.set(cx);
        }
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
            let result = match job {
                Job::Here(job) => cx.background_spawn(async move { job().finish() }).await,
                Job::OnHost(call) => call.await.unwrap_or_else(|error| Loaded::failed(format!("{error:#}"))),
            };
            this.update(cx, |this, cx| {
                this.apply(result, cx);
                this.load_task = None;
                // What changed while loading is loaded now.
                this.schedule(cx);
            })
            .ok();
        }));
    }

    fn take_job(&mut self, cx: &mut Context<Self>) -> Job {
        let pending = std::mem::take(&mut self.pending);
        let mut config = config::get(cx);
        let build = ActiveBuild::get(cx);
        if let Some(configuration) = &build.configuration {
            config.configuration = configuration.clone();
        }
        if let Some(platform) = build.msbuild_platform() {
            config.platform = platform;
        }
        let roots = self.roots(cx);
        let chosen = self.chosen.take().or_else(|| self.solution.as_ref().map(|s| s.path.clone()));
        let kvp = KeyValueStore::global(cx);
        let options = LoadOptions { eval: config.eval_options(), tree: config.tree_options(), solution_search_depth: config.solution_search_depth };
        // A remote project's files are on its host: the load runs there, and always in full
        // (what was loaded is here, not there).
        if let Some(host) = forge_host::Host::of(&self.project, cx) {
            let saved = kvp.read_kvp(&Self::selection_key(&roots)).ok().flatten().map(PathBuf::from);
            let params = load::LoadParams { roots, chosen, saved, options };
            return Job::OnHost(host.call(load::LOAD, &params, None, cx));
        }
        let current = (self.solution.clone(), self.projects.clone(), self.children.clone());
        Job::Here(Box::new(move || {
            let saved = kvp.read_kvp(&Self::selection_key(&roots)).ok().flatten().map(PathBuf::from);
            if pending.full {
                load::load_everything(roots, chosen, saved, &options)
            } else {
                let (solution, previous, children) = current;
                load::load_some(solution, previous, children, &pending.evaluate, &pending.relist, &options)
            }
        }))
    }

    fn apply(&mut self, result: Loaded, cx: &mut Context<Self>) {
        if let Some(solutions) = result.solutions {
            self.solutions = solutions;
        }
        self.solution = result.solution.map(Arc::new);
        self.projects = Arc::new(result.projects);
        self.children = result.children;
        self.tree = result.tree.map(Arc::new);
        self.watched = result.watched;
        self.error = result.error;
        self.loading = !self.pending.is_empty();
        self.generation += 1;
        self.restore_build(cx);
        let (project, solution) = (self.project.clone(), self.solution.clone());
        self.roslyn.open(&project, solution.as_deref(), cx);
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
