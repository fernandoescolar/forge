//! What the explorer's commands do: file and folder operations that keep project files in
//! step, solution edits, `dotnet` commands, references and drag & drop.

use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use dotnet_model::explorer::NodeKind;
use dotnet_model::msbuild::edit::{self, Position};
use dotnet_model::solution::{self, SolutionEdit, SolutionFormat};
use dotnet_model::{cli, paths, templates};
use fs::{Fs, RemoveOptions};
use gpui::{ClipboardItem, PathPromptOptions, PromptLevel};
use task::TaskTemplate;

use super::*;
use forge_ui::pick::{self, Choice};

/// A piece of background work in a job.
type Step = futures::future::BoxFuture<'static, Result<()>>;

/// Copies a file or a folder tree.
fn copy_recursively(from: &Path, to: &Path) -> Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_recursively(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(from, to).with_context(|| format!("copying {}", from.display()))?;
    }
    Ok(())
}

fn evaluate(project: &Path, cx_config: &crate::config::DotnetConfig) -> Result<dotnet_model::Project> {
    dotnet_model::evaluate(project, &cx_config.eval_options())
}

/// Adds items for files that were put into a project (all of them, for a folder).
fn add_items(project: &Path, path: &Path, config: &crate::config::DotnetConfig) -> Result<()> {
    let item_types = config.item_types();
    if path.is_dir() {
        let files: Vec<PathBuf> = walkdir_files(path);
        if files.is_empty() {
            edit::add_folder(&evaluate(project, config)?, path)?;
        }
        for file in files {
            edit::add_file(&evaluate(project, config)?, &file, &edit::item_type_for(&file, &item_types), None)?;
        }
        Ok(())
    } else {
        edit::add_file(&evaluate(project, config)?, path, &edit::item_type_for(path, &item_types), None)
    }
}

fn walkdir_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walkdir_files(&path));
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

impl SolutionExplorer {
    /// Runs file work in the background, then reloads what it `touched` (solution or
    /// project files, or files in a project; nothing means everything) and opens the
    /// returned file.
    pub(crate) fn run_job(
        &mut self,
        label: &str,
        touched: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
        job: impl Future<Output = Result<Option<PathBuf>>> + Send + 'static,
    ) {
        self.busy = Some(label.to_string().into());
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(job).await;
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                this.model.update(cx, |model, cx| model.reload_paths(&touched, cx));
                match result {
                    Ok(Some(path)) if path.is_file() => this.open_file(path, true, window, cx),
                    Ok(_) => {}
                    Err(error) => this.show_error(format!("{error:#}"), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn config(&self, cx: &App) -> crate::config::DotnetConfig {
        config::get(cx)
    }

    /// The folder new files go in and its project, from the selection.
    fn target_dir(&self, cx: &App) -> Option<(PathBuf, PathBuf)> {
        match self.selected_kind()? {
            NodeKind::Project { path } => Some((path.parent()?.to_path_buf(), path)),
            NodeKind::Folder { project, path, is_link: false } => Some((path, project)),
            NodeKind::File { project, path, is_link: false, .. } => Some((path.parent()?.to_path_buf(), project)),
            kind => {
                let project = kind.project()?.to_path_buf();
                let _ = cx;
                Some((project.parent()?.to_path_buf(), project))
            }
        }
    }

    /// The row new entries show under: the selected folder or project, or a file's parent.
    fn target_row(&self) -> Option<String> {
        let id = self.selected.clone()?;
        match self.selected_kind()? {
            NodeKind::File { .. } => self.index.get(&id).and_then(|(_, parent)| parent.clone()),
            _ => Some(id),
        }
    }

    /// The solution folder (or the root, `Some(None)`) for new projects and folders.
    fn target_solution_folder(&self) -> Option<Option<String>> {
        match self.selected_kind() {
            Some(NodeKind::SolutionFolder { id }) => Some(Some(id)),
            Some(NodeKind::Solution) | None => Some(None),
            Some(_) => {
                let mut current = self.selected.clone();
                while let Some(id) = current {
                    let (kind, parent) = self.index.get(&id)?;
                    if let NodeKind::SolutionFolder { id } = kind {
                        return Some(Some(id.clone()));
                    }
                    current = parent.clone();
                }
                Some(None)
            }
        }
    }

    fn solution_path(&self, cx: &App) -> Option<(PathBuf, SolutionFormat)> {
        self.model.read(cx).solution.as_ref().map(|s| (s.path.clone(), s.format))
    }

    fn real_solution(&self, cx: &mut Context<Self>) -> Option<PathBuf> {
        match self.solution_path(cx) {
            Some((path, SolutionFormat::Sln | SolutionFormat::Slnx)) => Some(path),
            _ => {
                self.show_error("This workspace has no solution file. Create one with New Solution.".into(), cx);
                None
            }
        }
    }

    pub(super) fn new_file(&mut self, _: &NewFile, window: &mut Window, cx: &mut Context<Self>) {
        self.new_file_with(None, None, window, cx);
    }

    pub(super) fn new_file_with(&mut self, template: Option<templates::Template>, anchor: Option<Position>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((dir, project)) = self.target_dir(cx) else { return };
        let anchor = anchor.and_then(|position| match self.selected_kind() {
            Some(NodeKind::File { path, .. }) => Some((path, position)),
            _ => None,
        });
        let Some(row) = self.target_row() else { return };
        self.start_edit(EditKind::NewFile { dir, project, template, anchor }, row, "", true, window, cx);
    }

    pub(super) fn new_folder(&mut self, _: &NewFolder, window: &mut Window, cx: &mut Context<Self>) {
        let Some((dir, project)) = self.target_dir(cx) else { return };
        let Some(row) = self.target_row() else { return };
        self.start_edit(EditKind::NewFolder { dir, project }, row, "", true, window, cx);
    }

    pub(super) fn new_solution_folder(&mut self, _: &NewSolutionFolder, window: &mut Window, cx: &mut Context<Self>) {
        if self.real_solution(cx).is_none() {
            return;
        }
        let parent = self.target_solution_folder().flatten();
        let row = match &parent {
            Some(id) => format!("folder:{id}"),
            None => self.lines.iter().find_map(|l| if let Line::Row(r) = l { Some(r.id.clone()) } else { None }).unwrap_or_default(),
        };
        self.start_edit(EditKind::NewSolutionFolder { parent }, row, "", true, window, cx);
    }

    pub(super) fn rename(&mut self, _: &Rename, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else { return };
        let Some((kind, _)) = self.index.get(&id).cloned() else { return };
        let label = self.lines.iter().find_map(|l| match l {
            Line::Row(r) if r.id == id => Some(r.label.to_string()),
            _ => None,
        });
        let (edit, initial) = match kind {
            NodeKind::File { path, project, is_link: false, .. } | NodeKind::Folder { path, project, is_link: false } => {
                let name = paths::file_name(&path);
                (EditKind::Rename { path, project: Some(project) }, name)
            }
            NodeKind::SolutionItem { path, .. } => {
                let name = paths::file_name(&path);
                (EditKind::Rename { path, project: None }, name)
            }
            NodeKind::Project { path } => {
                let name = paths::file_name(&path);
                (EditKind::RenameProject { path }, name)
            }
            NodeKind::SolutionFolder { id } => (EditKind::RenameSolutionFolder { id }, label.unwrap_or_default()),
            NodeKind::Solution if self.real_solution(cx).is_some() => (EditKind::RenameSolution, label.unwrap_or_default()),
            _ => return,
        };
        self.start_edit(edit, id, &initial, false, window, cx);
    }

    /// Does what the inline name editor was opened for.
    pub(super) fn apply_edit(&mut self, kind: EditKind, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if name.contains(['/', '\\']) && !matches!(kind, EditKind::NewFile { .. } | EditKind::NewFolder { .. }) {
            self.show_error("Names cannot contain slashes.".into(), cx);
            return;
        }
        let config = self.config(cx);
        let solution = self.solution_path(cx);
        let model = self.model.read(cx);
        match kind {
            EditKind::NewFile { dir, project, template, anchor } => {
                let name = match &template {
                    Some(t) if !name.to_lowercase().ends_with(&format!(".{}", t.extension)) => format!("{name}.{}", t.extension),
                    _ => name,
                };
                let path = dir.join(&name);
                let content = match (&template, model.project(&project)) {
                    (Some(template), project) => templates::render(template, &path, project).unwrap_or_default(),
                    _ => String::new(),
                };
                self.run_job("Creating file…", vec![project.clone()], window, cx, async move {
                    if path.exists() {
                        bail!("{} already exists", paths::file_name(&path));
                    }
                    std::fs::create_dir_all(path.parent().unwrap())?;
                    std::fs::write(&path, content)?;
                    let project = evaluate(&project, &config)?;
                    let anchor = anchor.as_ref().map(|(p, pos)| (p.as_path(), *pos));
                    edit::add_file(&project, &path, &edit::item_type_for(&path, &config.item_types()), anchor)?;
                    Ok(Some(path))
                });
            }
            EditKind::NewFolder { dir, project } => {
                let path = dir.join(&name);
                self.run_job("Creating folder…", vec![project.clone()], window, cx, async move {
                    if path.exists() {
                        bail!("{} already exists", paths::file_name(&path));
                    }
                    std::fs::create_dir_all(&path)?;
                    edit::add_folder(&evaluate(&project, &config)?, &path)?;
                    Ok(None)
                });
            }
            EditKind::NewSolutionFolder { parent } => {
                let Some((solution, _)) = solution else { return };
                self.run_job("Adding solution folder…", vec![solution.clone()], window, cx, async move {
                    solution::edit_file(&solution, &SolutionEdit::CreateFolder { parent, name })?;
                    Ok(None)
                });
            }
            EditKind::Rename { path, project } => {
                let to = path.with_file_name(&name);
                if to == path {
                    return;
                }
                let solution_item = project.is_none().then(|| solution.clone()).flatten();
                let folder = self.selected_kind().and_then(|k| if let NodeKind::SolutionItem { folder, .. } = k { Some(folder) } else { None });
                self.run_job("Renaming…", project.iter().cloned().chain(solution_item.as_ref().map(|(s, _)| s.clone())).collect(), window, cx, async move {
                    let only_case = to.to_string_lossy().to_lowercase() == path.to_string_lossy().to_lowercase();
                    if to.exists() && !only_case {
                        bail!("{} already exists", name);
                    }
                    std::fs::rename(&path, &to)?;
                    if let Some(project) = project {
                        edit::rename_path(&evaluate(&project, &config)?, &path, &to)?;
                    }
                    if let (Some((solution, _)), Some(folder)) = (solution_item, folder) {
                        solution::edit_file(&solution, &SolutionEdit::RemoveFile { folder: folder.clone(), path: path.clone() })?;
                        solution::edit_file(&solution, &SolutionEdit::AddFile { folder, path: to.clone() })?;
                    }
                    Ok(None)
                });
            }
            EditKind::RenameProject { path } => {
                let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
                let name = if paths::extension(Path::new(&name)) == ext.to_lowercase() { name } else { format!("{name}.{ext}") };
                let to = path.with_file_name(&name);
                if to == path {
                    return;
                }
                let id = model.solution.as_ref().and_then(|s| s.project_by_path(&path)).map(|p| p.id.clone());
                let referencing: Vec<PathBuf> = model.loaded_projects().into_iter().filter(|p| p.project_references.contains(&path)).map(|p| p.path).collect();
                self.run_job("Renaming project…", Vec::new(), window, cx, async move {
                    if to.exists() {
                        bail!("{name} already exists");
                    }
                    std::fs::rename(&path, &to)?;
                    if let (Some((solution, SolutionFormat::Sln | SolutionFormat::Slnx)), Some(id)) = (solution, id) {
                        solution::edit_file(&solution, &SolutionEdit::RenameProject { id, path: to.clone() })?;
                    }
                    for project in referencing {
                        edit::rename_path(&evaluate(&project, &config)?, &path, &to)?;
                    }
                    Ok(None)
                });
            }
            EditKind::RenameSolutionFolder { id } => {
                let Some((solution, _)) = solution else { return };
                self.run_job("Renaming solution folder…", vec![solution.clone()], window, cx, async move {
                    solution::edit_file(&solution, &SolutionEdit::RenameFolder { id, name })?;
                    Ok(None)
                });
            }
            EditKind::RenameSolution => {
                let Some((solution, _)) = solution else { return };
                let ext = paths::extension(&solution);
                let name = if name.to_lowercase().ends_with(&format!(".{ext}")) { name } else { format!("{name}.{ext}") };
                let to = solution.with_file_name(name);
                if to.exists() {
                    self.show_error(format!("{} already exists", to.display()), cx);
                    return;
                }
                if let Err(error) = std::fs::rename(&solution, &to) {
                    self.show_error(format!("{error:#}"), cx);
                    return;
                }
                self.model.update(cx, |model, cx| model.select_solution(to, cx));
            }
        }
    }

    /// The work that removes one selected node, and what it touches. Files and folders
    /// go to the Trash (links only leave the project), projects and solution items leave
    /// the solution, packages and references leave their project.
    fn removal(&self, kind: &NodeKind, cx: &App) -> Option<(Vec<PathBuf>, Step)> {
        let config = self.config(cx);
        let solution = self.solution_path(cx).filter(|(_, f)| *f != SolutionFormat::Folder).map(|(p, _)| p);
        let model = self.model.read(cx);
        match kind.clone() {
            NodeKind::File { path, project, is_link, .. } | NodeKind::Folder { path, project, is_link } => {
                let fs = <dyn Fs>::global(cx);
                Some((
                    vec![project.clone()],
                    Box::pin(async move {
                        if !is_link {
                            fs.trash(&path, RemoveOptions { recursive: true, ignore_if_not_exists: true }).await?;
                        }
                        edit::remove_path(&evaluate(&project, &config)?, &path)
                    }),
                ))
            }
            NodeKind::SolutionFolder { id } => {
                let solution = solution?;
                Some((vec![solution.clone()], Box::pin(async move { solution::edit_file(&solution, &SolutionEdit::DeleteFolder { id }) })))
            }
            NodeKind::Project { path } | NodeKind::ProjectError { path, .. } => {
                let solution = solution?;
                let id = model.solution.as_ref()?.project_by_path(&path)?.id.clone();
                Some((vec![solution.clone()], Box::pin(async move { solution::edit_file(&solution, &SolutionEdit::RemoveProject { id }) })))
            }
            NodeKind::SolutionItem { folder, path } => {
                let solution = solution?;
                Some((vec![solution.clone()], Box::pin(async move { solution::edit_file(&solution, &SolutionEdit::RemoveFile { folder, path }) })))
            }
            NodeKind::Package { project, name, .. } => {
                Some((vec![project.clone()], Box::pin(async move { edit::remove_package(&evaluate(&project, &config)?, &name) })))
            }
            NodeKind::ProjectReference { project, target } => {
                Some((vec![project.clone()], Box::pin(async move { edit::remove_project_reference(&evaluate(&project, &config)?, &target) })))
            }
            _ => None,
        }
    }

    /// Runs several steps one after the other in one job, so edits to the same project
    /// file never race.
    fn run_steps(&mut self, label: &str, steps: Vec<(Vec<PathBuf>, Step)>, window: &mut Window, cx: &mut Context<Self>) {
        if steps.is_empty() {
            return;
        }
        let mut touched: Vec<PathBuf> = steps.iter().flat_map(|(t, _)| t.clone()).collect();
        touched.sort();
        touched.dedup();
        self.run_job(label, touched, window, cx, async move {
            let mut errors = Vec::new();
            for (_, step) in steps {
                if let Err(error) = step.await {
                    errors.push(format!("{error:#}"));
                }
            }
            if errors.is_empty() { Ok(None) } else { Err(anyhow!(errors.join("\n"))) }
        });
    }

    /// Removes what is selected; asks first when that deletes files or solution folders.
    pub(super) fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        let kinds = self.selected_kinds();
        let steps: Vec<(Vec<PathBuf>, Step)> = kinds.iter().filter_map(|k| self.removal(k, cx)).collect();
        if steps.is_empty() {
            return;
        }
        let deletes_files = kinds.iter().any(|k| matches!(k, NodeKind::File { is_link: false, .. } | NodeKind::Folder { is_link: false, .. }));
        let removes_folders = kinds.iter().any(|k| matches!(k, NodeKind::SolutionFolder { .. }));
        if !deletes_files && !removes_folders {
            self.run_steps("Removing…", steps, window, cx);
            return;
        }
        let what = if kinds.len() == 1 {
            self.selected.as_ref().and_then(|id| self.row_of.get(id)).and_then(|ix| self.lines.get(*ix)).map(|l| match l {
                Line::Row(r) => r.label.to_string(),
                _ => String::new(),
            }).unwrap_or_default()
        } else {
            format!("{} items", kinds.len())
        };
        let detail = match (deletes_files, removes_folders) {
            (true, false) => "Files are moved to the Trash.",
            (false, true) => "Projects in solution folders leave the solution too. No files are deleted.",
            _ => "Files are moved to the Trash; projects in solution folders leave the solution.",
        };
        let answer = window.prompt(PromptLevel::Warning, &format!("Delete {what}?"), Some(detail), &["Delete", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await.ok() != Some(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| this.run_steps("Deleting…", steps, window, cx)).ok();
        })
        .detach();
    }

    pub(super) fn remove_from_solution(&mut self, _: &RemoveFromSolution, window: &mut Window, cx: &mut Context<Self>) {
        let steps = self
            .selected_kinds()
            .iter()
            .filter(|k| matches!(k, NodeKind::Project { .. } | NodeKind::ProjectError { .. } | NodeKind::SolutionItem { .. }))
            .filter_map(|k| self.removal(k, cx))
            .collect();
        self.run_steps("Removing from the solution…", steps, window, cx);
    }

    pub(super) fn add_solution_item(&mut self, _: &AddSolutionItem, window: &mut Window, cx: &mut Context<Self>) {
        let Some(solution) = self.real_solution(cx) else { return };
        let folder = self.target_solution_folder().flatten();
        let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Add".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(files))) = paths.await else { return };
            this.update_in(cx, |this, window, cx| {
                this.run_job("Adding files…", vec![solution.clone()], window, cx, async move {
                    let folder = match folder {
                        Some(folder) => folder,
                        None => {
                            // Files go in a "Solution Items" folder, as Visual Studio does.
                            let existing = solution::Solution::load(&solution)?.folders.iter().find(|f| f.name.eq_ignore_ascii_case("Solution Items")).map(|f| f.id.clone());
                            match existing {
                                Some(id) => id,
                                None => {
                                    solution::edit_file(&solution, &SolutionEdit::CreateFolder { parent: None, name: "Solution Items".into() })?;
                                    solution::Solution::load(&solution)?.folders.iter().find(|f| f.name == "Solution Items").map(|f| f.id.clone()).context("folder not created")?
                                }
                            }
                        }
                    };
                    for file in files {
                        solution::edit_file(&solution, &SolutionEdit::AddFile { folder: folder.clone(), path: file })?;
                    }
                    Ok(None)
                });
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected_kinds()
            .into_iter()
            .filter_map(|k| match k {
                NodeKind::File { path, is_link: false, .. } | NodeKind::Folder { path, is_link: false, .. } => Some(path),
                _ => None,
            })
            .collect()
    }

    pub(super) fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let paths = self.selected_paths();
        if !paths.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join("\n")));
            self.clipboard = Some(Clipboard { paths, cut: false });
            cx.notify();
        }
    }

    pub(super) fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        let paths = self.selected_paths();
        if !paths.is_empty() {
            self.clipboard = Some(Clipboard { paths, cut: true });
            cx.notify();
        }
    }

    pub(super) fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clipboard) = self.clipboard.clone() else { return };
        let Some((dir, project)) = self.target_dir(cx) else { return };
        let config = self.config(cx);
        let model = self.model.read(cx);
        let sources: Vec<(PathBuf, Option<PathBuf>)> = clipboard.paths.iter().map(|p| (p.clone(), model.project_for_file(p).map(|p| p.path.clone()))).collect();
        if clipboard.cut {
            self.clipboard = None;
        }
        self.run_job(if clipboard.cut { "Moving…" } else { "Copying…" }, sources.iter().filter_map(|(_, p)| p.clone()).chain([project.clone()]).collect(), window, cx, async move {
            for (source, source_project) in sources {
                if dir.starts_with(&source) {
                    bail!("cannot paste a folder into itself");
                }
                let mut target = dir.join(source.file_name().context("no file name")?);
                if target.exists() {
                    if clipboard.cut && target == source {
                        continue;
                    }
                    target = paths::copy_name(&target);
                }
                if clipboard.cut {
                    std::fs::rename(&source, &target)?;
                    match source_project {
                        Some(sp) if sp == project => edit::rename_path(&evaluate(&project, &config)?, &source, &target)?,
                        other => {
                            if let Some(sp) = other {
                                edit::remove_path(&evaluate(&sp, &config)?, &source)?;
                            }
                            add_items(&project, &target, &config)?;
                        }
                    }
                } else {
                    copy_recursively(&source, &target)?;
                    add_items(&project, &target, &config)?;
                }
            }
            Ok(None)
        });
    }

    pub(super) fn duplicate(&mut self, _: &Duplicate, window: &mut Window, cx: &mut Context<Self>) {
        let Some(NodeKind::File { path, project, is_link: false, .. } | NodeKind::Folder { path, project, is_link: false }) = self.selected_kind() else { return };
        let config = self.config(cx);
        self.run_job("Duplicating…", vec![project.clone()], window, cx, async move {
            let target = paths::copy_name(&path);
            copy_recursively(&path, &target)?;
            add_items(&project, &target, &config)?;
            Ok(target.is_file().then_some(target))
        });
    }

    fn selected_disk_path(&self, cx: &App) -> Option<PathBuf> {
        match self.selected_kind()? {
            NodeKind::Solution => self.solution_path(cx).map(|(p, _)| p),
            NodeKind::SolutionFolder { .. } => self.solution_path(cx).map(|(p, _)| p.parent().unwrap_or(&p).to_path_buf()),
            kind => kind.path().or(kind.project()).map(Path::to_path_buf),
        }
    }

    pub(super) fn copy_path(&mut self, _: &CopyPath, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.selected_disk_path(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(path.to_string_lossy().into_owned()));
        }
    }

    pub(super) fn copy_relative_path(&mut self, _: &CopyRelativePath, _: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.selected_disk_path(cx) else { return };
        let base = self.model.read(cx).solution.as_ref().map(|s| s.dir().to_path_buf());
        let relative = base.and_then(|b| paths::relative(&b, &path)).unwrap_or(path);
        cx.write_to_clipboard(ClipboardItem::new_string(relative.to_string_lossy().into_owned()));
    }

    pub(super) fn reveal_in_finder(&mut self, _: &RevealInFinder, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.selected_disk_path(cx) {
            cx.reveal_path(&path);
        }
    }

    pub(super) fn open_in_terminal(&mut self, _: &OpenInTerminal, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.selected_disk_path(cx) else { return };
        let working_directory = if path.is_dir() { path } else { path.parent().map(Path::to_path_buf).unwrap_or(path) };
        window.dispatch_action(workspace::OpenTerminal { working_directory, local: false }.boxed_clone(), cx);
    }

    /// What `dotnet build` and friends act on: the selected project, else the solution.
    fn command_target(&self, cx: &App) -> Option<PathBuf> {
        match self.selected_kind() {
            Some(NodeKind::Solution | NodeKind::SolutionFolder { .. }) | None => match self.solution_path(cx)? {
                (path, SolutionFormat::Sln | SolutionFormat::Slnx) => Some(path),
                // Without a solution, `dotnet` needs a project.
                (_, SolutionFormat::Folder) => self.model.read(cx).solution.as_ref()?.all_projects().first().map(|p| p.path.clone()),
            },
            Some(kind) => kind.project().map(Path::to_path_buf),
        }
    }

    /// Runs a `dotnet` command in a terminal.
    pub(crate) fn run_dotnet(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.command_target(cx) else { return };
        let config = self.config(cx);
        let args = cli::command_line(command, &[("projectPath", &target.to_string_lossy())], &config.custom_commands);
        let Some((program, args)) = args.split_first() else { return };
        let cwd = target.parent().map(Path::to_path_buf).unwrap_or_default();
        let template = TaskTemplate {
            label: format!("dotnet {command} {}", paths::file_name(&target)),
            command: program.clone(),
            args: args.iter().map(|a| cli::quote(a)).collect(),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            save: task::SaveStrategy::All,
            ..TaskTemplate::default()
        };
        let context = task::TaskContext { cwd: Some(cwd), ..Default::default() };
        let Some(resolved) = template.resolve_task("forge-dotnet", &context) else { return };
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.schedule_resolved_task(project::TaskSourceKind::UserInput, resolved, false, window, cx);
            });
        }
    }

    pub(super) fn build(&mut self, _: &Build, window: &mut Window, cx: &mut Context<Self>) {
        self.run_dotnet("build", window, cx);
    }
    pub(super) fn rebuild_project(&mut self, _: &Rebuild, window: &mut Window, cx: &mut Context<Self>) {
        self.run_dotnet("rebuild", window, cx);
    }
    pub(super) fn clean(&mut self, _: &Clean, window: &mut Window, cx: &mut Context<Self>) {
        self.run_dotnet("clean", window, cx);
    }
    pub(super) fn restore(&mut self, _: &Restore, window: &mut Window, cx: &mut Context<Self>) {
        self.run_dotnet("restore", window, cx);
    }
    pub(super) fn run(&mut self, _: &Run, window: &mut Window, cx: &mut Context<Self>) {
        self.run_dotnet("run", window, cx);
    }
    pub(super) fn watch(&mut self, _: &Watch, window: &mut Window, cx: &mut Context<Self>) {
        self.run_dotnet("watch", window, cx);
    }
    pub(super) fn pack(&mut self, _: &Pack, window: &mut Window, cx: &mut Context<Self>) {
        self.run_dotnet("pack", window, cx);
    }
    pub(super) fn publish(&mut self, _: &Publish, window: &mut Window, cx: &mut Context<Self>) {
        self.run_dotnet("publish", window, cx);
    }

    /// Tests run in Forge's Tests panel, which shows results per test.
    pub(super) fn test(&mut self, _: &Test, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.selected_kind().and_then(|k| k.project().map(Path::to_path_buf));
        let workspace = self.workspace.clone();
        // The Tests panel reads the workspace when it runs, so it runs outside its update.
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else { return };
            let Some(panel) = workspace.update(cx, |workspace, cx| workspace.focus_panel::<forge_tests::TestPanel>(window, cx)) else { return };
            panel.update(cx, |panel, cx| match &project {
                Some(project) => panel.run_project(project, window, cx),
                None => panel.run_all(window, cx),
            });
        });
    }

    pub(super) fn manage_packages(&mut self, _: &ManagePackages, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.selected_kind().and_then(|k| k.project().map(Path::to_path_buf));
        let model = self.model.clone();
        pick::defer_workspace(self.workspace.clone(), window, cx, move |workspace, window, cx| crate::nuget_view::open(workspace, model, project, window, cx));
    }

    pub(super) fn add_project_reference(&mut self, _: &AddProjectReference, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_path) = self.selected_kind().and_then(|k| k.project().map(Path::to_path_buf)) else { return };
        let model = self.model.read(cx);
        let Some(project) = model.project(&project_path).cloned() else { return };
        let candidates: Vec<PathBuf> = model
            .solution
            .as_ref()
            .map(|s| s.all_projects().into_iter().map(|p| p.path.clone()).filter(|p| *p != project_path && !project.project_references.contains(p)).collect())
            .unwrap_or_default();
        if candidates.is_empty() {
            self.show_error("There are no other projects to reference.".into(), cx);
            return;
        }
        let dir = model.solution.as_ref().map(|s| s.dir().to_path_buf()).unwrap_or_default();
        let choices = candidates.iter().map(|p| Choice::new(paths::file_stem(p)).detail(paths::relative(&dir, p).unwrap_or(p.clone()).to_string_lossy().into_owned())).collect();
        let this = cx.entity().downgrade();
        pick::defer_workspace(self.workspace.clone(), window, cx, move |workspace, window, cx| {
            pick::pick(workspace, "Reference which project?", choices, window, cx, move |ix, window, cx| {
                let target = candidates[ix].clone();
                this.update(cx, |this, cx| {
                    this.run_job("Adding reference…", vec![project.path.clone()], window, cx, async move {
                        edit::add_project_reference(&project, &target)?;
                        Ok(None)
                    });
                })
                .ok();
            });
        });
    }

    pub(super) fn remove_reference(&mut self, _: &RemoveReference, window: &mut Window, cx: &mut Context<Self>) {
        let steps = self
            .selected_kinds()
            .iter()
            .filter(|k| matches!(k, NodeKind::Package { .. } | NodeKind::ProjectReference { .. }))
            .filter_map(|k| self.removal(k, cx))
            .collect();
        self.run_steps("Removing…", steps, window, cx);
    }

    pub(super) fn update_package(&mut self, _: &UpdatePackage, window: &mut Window, cx: &mut Context<Self>) {
        let Some(NodeKind::Package { project, name, version }) = self.selected_kind() else { return };
        let Some(client) = crate::fetch::client(cx) else { return };
        let config = self.config(cx);
        let sources = dotnet_model::nuget::sources_for(project.parent().unwrap_or(Path::new(".")));
        let workspace = self.workspace.clone();
        let this = cx.entity().downgrade();
        self.busy = Some(format!("Looking up versions of {name}…").into());
        cx.notify();
        cx.spawn_in(window, async move |panel, cx| {
            let versions = client.versions(&sources, &name, config.nuget.include_prerelease).await;
            panel
                .update_in(cx, |panel, window, cx| {
                    panel.busy = None;
                    cx.notify();
                    if versions.is_empty() {
                        panel.show_error(format!("No versions of {name} found in the package sources."), cx);
                        return;
                    }
                    let choices = versions
                        .iter()
                        .enumerate()
                        .map(|(i, v)| {
                            let mut choice = Choice::new(v.clone());
                            if Some(v) == version.as_ref() {
                                choice = choice.detail("current");
                            } else if i == 0 {
                                choice = choice.detail("latest");
                            }
                            choice
                        })
                        .collect();
                    pick::defer_workspace(workspace.clone(), window, cx, move |workspace, window, cx| {
                        pick::pick(workspace, &format!("Version of {name}"), choices, window, cx, move |ix, window, cx| {
                            let version = versions[ix].clone();
                            this.update(cx, |this, cx| {
                                this.run_job("Updating package…", Vec::new(), window, cx, async move {
                                    edit::set_package_version(&evaluate(&project, &config)?, &name, &version)?;
                                    crate::restore::restore(&project).await;
                                    Ok(None)
                                });
                            })
                            .ok();
                        });
                    });
                })
                .ok();
        })
        .detach();
    }

    pub(super) fn move_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        self.move_item(Position::Before, window, cx);
    }

    pub(super) fn move_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        self.move_item(Position::After, window, cx);
    }

    fn move_item(&mut self, position: Position, window: &mut Window, cx: &mut Context<Self>) {
        let Some(NodeKind::File { path, project, .. }) = self.selected_kind() else { return };
        if !paths::extension(&project).eq("fsproj") {
            return;
        }
        let config = self.config(cx);
        self.run_job("Moving…", vec![project.clone()], window, cx, async move {
            edit::move_item(&evaluate(&project, &config)?, &path, position)?;
            Ok(None)
        });
    }

    pub(super) fn add_existing_project(&mut self, _: &AddExistingProject, window: &mut Window, cx: &mut Context<Self>) {
        let Some(solution) = self.real_solution(cx) else { return };
        let parent = self.target_solution_folder().flatten();
        let model = self.model.read(cx);
        let in_solution: Vec<PathBuf> = model.solution.as_ref().map(|s| s.all_projects().into_iter().map(|p| p.path.clone()).collect()).unwrap_or_default();
        let roots = model.roots(cx);
        let mut candidates: Vec<PathBuf> = roots.iter().flat_map(|r| solution::find_projects(r, 8)).filter(|p| !in_solution.contains(p)).collect();
        candidates.sort();
        let dir = solution.parent().unwrap_or(Path::new(".")).to_path_buf();
        let mut choices: Vec<Choice> = candidates.iter().map(|p| Choice::new(paths::file_stem(p)).detail(paths::relative(&dir, p).unwrap_or(p.clone()).to_string_lossy().into_owned())).collect();
        choices.push(Choice::new("Browse…").detail("Choose a project file"));
        let this = cx.entity().downgrade();
        pick::defer_workspace(self.workspace.clone(), window, cx, move |workspace, window, cx| {
            pick::pick(workspace, "Add which project to the solution?", choices, window, cx, move |ix, window, cx| {
                let chosen = candidates.get(ix).cloned();
                this.update(cx, |this, cx| {
                    let add = move |this: &mut SolutionExplorer, path: PathBuf, window: &mut Window, cx: &mut Context<SolutionExplorer>| {
                        let (solution, parent) = (solution.clone(), parent.clone());
                        this.run_job("Adding project…", vec![solution.clone()], window, cx, async move {
                            solution::edit_file(&solution, &SolutionEdit::AddProject { path, parent })?;
                            Ok(None)
                        });
                    };
                    match chosen {
                        Some(path) => add(this, path, window, cx),
                        None => {
                            let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Add".into()) });
                            cx.spawn_in(window, async move |this, cx| {
                                let Ok(Ok(Some(mut files))) = paths.await else { return };
                                let Some(path) = files.pop() else { return };
                                this.update_in(cx, |this, window, cx| add(this, path, window, cx)).ok();
                            })
                            .detach();
                        }
                    }
                })
                .ok();
            });
        });
    }

    pub(super) fn select_solution(&mut self, _: &SelectSolution, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let solutions = model.solutions.clone();
        if solutions.is_empty() {
            return;
        }
        let roots = model.roots(cx);
        let choices = solutions
            .iter()
            .map(|p| {
                let relative = roots.iter().find_map(|r| p.strip_prefix(r).ok()).map(|r| r.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string_lossy().into_owned());
                Choice::new(paths::file_name(p)).detail(relative)
            })
            .collect();
        let model = self.model.clone();
        pick::defer_workspace(self.workspace.clone(), window, cx, move |workspace, window, cx| {
            pick::pick(workspace, "Show which solution?", choices, window, cx, move |ix, _, cx| {
                let path = solutions[ix].clone();
                model.update(cx, |model, cx| model.select_solution(path, cx));
            });
        });
    }

    pub(super) fn new_solution(&mut self, _: &NewSolution, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let Some(root) = model.roots(cx).into_iter().next() else {
            self.show_error("Open a folder first.".into(), cx);
            return;
        };
        // Projects already in the folder go into the new solution.
        let loose: Vec<PathBuf> = match &model.solution {
            Some(s) if s.format == SolutionFormat::Folder => s.all_projects().into_iter().map(|p| p.path.clone()).collect(),
            _ => Vec::new(),
        };
        let default_name = paths::file_name(&root);
        let model = self.model.clone();
        let this = cx.entity().downgrade();
        let choices = vec![Choice::new(".slnx").detail("XML solution (Visual Studio 17.13+, .NET 9+)"), Choice::new(".sln").detail("Classic solution file")];
        let workspace_weak = self.workspace.clone();
        pick::defer_workspace(self.workspace.clone(), window, cx, move |workspace, window, cx| {
            pick::pick(workspace, "Solution format", choices, window, cx, move |ix, window, cx| {
                let extension = if ix == 0 { "slnx" } else { "sln" };
                pick::defer_workspace(workspace_weak.clone(), window, cx, move |workspace, window, cx| {
                    pick::ask(workspace, "Solution name", &default_name, window, cx, move |name, window, cx| {
                        let path = root.join(format!("{name}.{extension}"));
                        this.update(cx, |this, cx| {
                            let model = model.clone();
                            let job_path = path.clone();
                            this.run_job("Creating solution…", Vec::new(), window, cx, async move {
                                if job_path.exists() {
                                    bail!("{} already exists", paths::file_name(&job_path));
                                }
                                let text = if extension == "slnx" { dotnet_model::slnx::empty_solution() } else { dotnet_model::sln::empty_solution() };
                                std::fs::write(&job_path, text)?;
                                for project in loose {
                                    solution::edit_file(&job_path, &SolutionEdit::AddProject { path: project, parent: None })?;
                                }
                                Ok(None)
                            });
                            model.update(cx, |model, cx| model.select_solution(path, cx));
                        })
                        .ok();
                    });
                });
            });
        });
    }

    pub(super) fn new_project(&mut self, _: &NewProject, window: &mut Window, cx: &mut Context<Self>) {
        let Some(templates) = self.project_templates.clone() else {
            self.busy = Some("Reading the project templates…".into());
            cx.notify();
            cx.spawn_in(window, async move |this, cx| {
                let output = cx.background_spawn(crate::restore::dotnet_output(vec!["new".into(), "list".into(), "--type".into(), "project".into()], None)).await;
                this.update_in(cx, |this, window, cx| {
                    this.busy = None;
                    match output {
                        Ok(text) => {
                            let templates = cli::parse_template_list(&text);
                            if templates.is_empty() {
                                this.show_error("`dotnet new list` returned no project templates.".into(), cx);
                                return;
                            }
                            this.project_templates = Some(templates);
                            this.new_project(&NewProject, window, cx);
                        }
                        Err(error) => this.show_error(format!("Could not run `dotnet new list`: {error:#}"), cx),
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        };
        let model = self.model.read(cx);
        let solution = model.solution.as_ref().filter(|s| s.format != SolutionFormat::Folder).map(|s| s.path.clone());
        let base = model.solution.as_ref().map(|s| s.dir().to_path_buf()).or_else(|| model.roots(cx).into_iter().next());
        let Some(base) = base else {
            self.show_error("Open a folder first.".into(), cx);
            return;
        };
        let parent = self.target_solution_folder().flatten();
        let choices = templates.iter().map(|t| Choice::new(t.name.clone()).detail(format!("{} · {}", t.short_names.join(", "), t.languages.join(", ")))).collect();
        let workspace_weak = self.workspace.clone();
        let this = cx.entity().downgrade();
        pick::defer_workspace(self.workspace.clone(), window, cx, move |workspace, window, cx| {
            pick::pick(workspace, "New project from which template?", choices, window, cx, move |ix, window, cx| {
                let template = templates[ix].clone();
                let languages = template.languages.clone();
                let workspace_for_language = workspace_weak.clone();
                let ask_name = move |language: Option<String>, window: &mut Window, cx: &mut App| {
                    let (this, base, parent, solution, template) = (this.clone(), base.clone(), parent.clone(), solution.clone(), template.clone());
                    pick::defer_workspace(workspace_weak.clone(), window, cx, move |workspace, window, cx| {
                        pick::ask(workspace, "Project name", "", window, cx, move |name, window, cx| {
                            let dir = base.join(&name);
                            let short = template.short_names.first().cloned().unwrap_or_default();
                            let args = cli::command_line(
                                "createProject",
                                &[("projectType", &short), ("language", language.as_deref().unwrap_or("")), ("projectName", &name), ("folderName", &dir.to_string_lossy()), ("framework", "")],
                                &config::get(cx).custom_commands,
                            );
                            this.update(cx, |this, cx| {
                                this.run_job(&format!("Creating {name}…"), Vec::new(), window, cx, async move {
                                    if dir.exists() && std::fs::read_dir(&dir).map(|mut d| d.next().is_some()).unwrap_or(false) {
                                        bail!("{} already exists and is not empty", dir.display());
                                    }
                                    crate::restore::dotnet_output(args.into_iter().skip(1).collect(), Some(base.clone())).await?;
                                    let project = solution::find_projects(&dir, 1).into_iter().next().ok_or_else(|| anyhow!("dotnet new made no project in {}", dir.display()))?;
                                    if let Some(solution) = solution {
                                        solution::edit_file(&solution, &SolutionEdit::AddProject { path: project.clone(), parent })?;
                                    }
                                    Ok(Some(project))
                                });
                            })
                            .ok();
                        });
                    });
                };
                if languages.len() > 1 {
                    pick::defer_workspace(workspace_for_language.clone(), window, cx, move |workspace, window, cx| {
                        let choices = languages.iter().map(|l| Choice::new(l.clone())).collect();
                        pick::pick(workspace, "Language", choices, window, cx, move |ix, window, cx| ask_name(Some(languages[ix].clone()), window, cx));
                    });
                } else {
                    ask_name(languages.first().cloned(), window, cx);
                }
            });
        });
    }

    pub(super) fn centralize_packages(&mut self, _: &CentralizePackageVersions, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let Some(solution) = model.solution.clone() else { return };
        let projects = model.loaded_projects();
        let answer = window.prompt(
            PromptLevel::Info,
            "Centralize package versions?",
            Some("Versions move from the projects to Directory.Packages.props, taking the highest where projects disagree."),
            &["Centralize", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await.ok() != Some(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                this.run_job("Centralizing package versions…", Vec::new(), window, cx, async move {
                    let file = dotnet_model::nuget::solution_packages::centralize(solution.dir(), &projects)?;
                    if solution.format != SolutionFormat::Folder {
                        let current = solution::Solution::load(&solution.path)?;
                        let listed = current.all_folders().iter().any(|(f, _)| f.files.contains(&file));
                        if !listed {
                            let folder = match current.folders.iter().find(|f| f.name.eq_ignore_ascii_case("Solution Items")) {
                                Some(f) => f.id.clone(),
                                None => {
                                    solution::edit_file(&solution.path, &SolutionEdit::CreateFolder { parent: None, name: "Solution Items".into() })?;
                                    solution::Solution::load(&solution.path)?.folders.iter().find(|f| f.name == "Solution Items").map(|f| f.id.clone()).context("folder not created")?
                                }
                            };
                            solution::edit_file(&solution.path, &SolutionEdit::AddFile { folder, path: file.clone() })?;
                        }
                    }
                    Ok(Some(file))
                });
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn customize_templates(&mut self, _: &CustomizeFileTemplates, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.model.read(cx).roots(cx).into_iter().next() else { return };
        self.run_job("Writing templates…", Vec::new(), window, cx, async move {
            let dir = templates::install(&root)?;
            Ok(Some(dir.join("template-list.json")))
        });
    }

    /// Drops dragged nodes on another: moves files and folders (copies them between
    /// projects), re-parents solution folders, projects and solution items.
    pub(super) fn drop_on(&mut self, dragged: DraggedNode, target_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((target, _)) = self.index.get(target_id).cloned() else { return };
        let steps: Vec<(Vec<PathBuf>, Step)> = dragged.nodes.iter().filter(|(id, _)| id != target_id).filter_map(|(_, kind)| self.drop_step(kind, &target, cx)).collect();
        let copies = steps.len() > 0
            && dragged.nodes.iter().any(|(_, k)| k.project().is_some_and(|p| Some(p) != target.project()) && matches!(k, NodeKind::File { .. } | NodeKind::Folder { .. }));
        self.marked.clear();
        self.run_steps(if copies { "Copying…" } else { "Moving…" }, steps, window, cx);
    }

    fn drop_step(&self, kind: &NodeKind, target: &NodeKind, cx: &App) -> Option<(Vec<PathBuf>, Step)> {
        let config = self.config(cx);
        let solution = self.solution_path(cx).filter(|(_, f)| *f != SolutionFormat::Folder).map(|(p, _)| p);
        let model = self.model.read(cx);
        let target_folder = match target {
            NodeKind::Solution => Some(None),
            NodeKind::SolutionFolder { id } => Some(Some(id.clone())),
            _ => None,
        };
        match (kind.clone(), target_folder, solution) {
            (NodeKind::SolutionFolder { id }, Some(parent), Some(solution)) => {
                Some((vec![solution.clone()], Box::pin(async move { solution::edit_file(&solution, &SolutionEdit::MoveFolder { id, parent }) })))
            }
            (NodeKind::Project { path }, Some(parent), Some(solution)) => {
                let id = model.solution.as_ref()?.project_by_path(&path)?.id.clone();
                Some((vec![solution.clone()], Box::pin(async move { solution::edit_file(&solution, &SolutionEdit::MoveProject { id, parent }) })))
            }
            (NodeKind::SolutionItem { folder, path }, Some(Some(target_folder)), Some(solution)) => Some((
                vec![solution.clone()],
                Box::pin(async move {
                    solution::edit_file(&solution, &SolutionEdit::RemoveFile { folder, path: path.clone() })?;
                    solution::edit_file(&solution, &SolutionEdit::AddFile { folder: target_folder, path })
                }),
            )),
            (NodeKind::File { path: source, project: source_project, is_link: false, .. } | NodeKind::Folder { path: source, project: source_project, is_link: false }, None, _) => {
                let (dir, target_project) = match target {
                    NodeKind::Project { path } => (path.parent()?.to_path_buf(), path.clone()),
                    NodeKind::Folder { path, project, is_link: false } => (path.clone(), project.clone()),
                    NodeKind::File { path, project, is_link: false, .. } => (path.parent()?.to_path_buf(), project.clone()),
                    _ => return None,
                };
                if dir.starts_with(&source) || source.parent() == Some(dir.as_path()) {
                    return None;
                }
                let same_project = source_project == target_project;
                Some((
                    vec![source_project.clone(), target_project.clone()],
                    Box::pin(async move {
                        let mut destination = dir.join(source.file_name().context("no file name")?);
                        if same_project {
                            if destination.exists() {
                                bail!("{} already exists there", paths::file_name(&destination));
                            }
                            std::fs::rename(&source, &destination)?;
                            edit::rename_path(&evaluate(&source_project, &config)?, &source, &destination)
                        } else {
                            // Between projects, files are copied, as in Visual Studio.
                            if destination.exists() {
                                destination = paths::copy_name(&destination);
                            }
                            copy_recursively(&source, &destination)?;
                            add_items(&target_project, &destination, &config)
                        }
                    }),
                ))
            }
            _ => None,
        }
    }
}
