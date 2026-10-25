//! The Solution Explorer's file operations, apart from the UI: each changes the disk and
//! the project or solution files that list what changed, in one transaction. A step that
//! fails (a project file that doesn't parse, a name already taken, a disk that is full)
//! undoes the ones before it, so files and project files never disagree: a moved file
//! goes back, a created one goes away, an edited project or solution gets its text back.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use dotnet_model::msbuild::edit::{self, Position};
use dotnet_model::paths;
use dotnet_model::solution::{self, SolutionEdit};

use crate::config::DotnetConfig;

/// What to put back if the operation fails, newest last.
#[derive(Default)]
pub(crate) struct Undo {
    steps: Vec<Restore>,
    kept: HashSet<PathBuf>,
}

enum Restore {
    /// A file's bytes before it was edited (`None`: it didn't exist).
    Text(PathBuf, Option<Vec<u8>>),
    /// Something moved from `from` to `to`.
    Move { from: PathBuf, to: PathBuf },
    /// A file or folder (tree) that was created.
    Created(PathBuf),
}

impl Undo {
    /// Remembers `file` as it is, before it is edited (once per operation).
    pub fn keep(&mut self, file: &Path) -> Result<()> {
        if !self.kept.insert(file.to_path_buf()) {
            return Ok(());
        }
        let text = match std::fs::read(file) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", file.display())),
        };
        self.steps.push(Restore::Text(file.to_path_buf(), text));
        Ok(())
    }

    /// Remembers the files an edit of `project` changes: the project file, or the items
    /// file of a shared project.
    pub fn keep_project(&mut self, project: &dotnet_model::Project) -> Result<()> {
        self.keep(&project.path)?;
        if let Some(items) = &project.items_file {
            self.keep(items)?;
        }
        Ok(())
    }

    fn moved(&mut self, from: &Path, to: &Path) {
        self.steps.push(Restore::Move { from: from.to_path_buf(), to: to.to_path_buf() });
    }

    fn created(&mut self, path: &Path) {
        self.steps.push(Restore::Created(path.to_path_buf()));
    }

    /// Puts everything back, newest first; what couldn't be.
    pub fn rollback(self) -> Vec<String> {
        let mut failed = Vec::new();
        for step in self.steps.into_iter().rev() {
            let result = match &step {
                Restore::Text(path, Some(text)) => std::fs::write(path, text).with_context(|| format!("restoring {}", path.display())),
                Restore::Text(path, None) => remove_from_disk(path),
                Restore::Move { from, to } => move_path(to, from).with_context(|| format!("moving {} back", to.display())),
                Restore::Created(path) => remove_from_disk(path),
            };
            if let Err(e) = result {
                failed.push(format!("{e:#}"));
            }
        }
        failed
    }
}

fn remove_from_disk(path: &Path) -> Result<()> {
    let result = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
    match result {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).with_context(|| format!("removing {}", path.display())),
        _ => Ok(()),
    }
}

/// Runs `operation`; when it fails, undoes what it did and returns its error (saying if
/// undoing failed too).
pub(crate) fn transaction<T>(operation: impl FnOnce(&mut Undo) -> Result<T>) -> Result<T> {
    let mut undo = Undo::default();
    match operation(&mut undo) {
        Ok(value) => Ok(value),
        Err(error) => {
            let failed = undo.rollback();
            if failed.is_empty() {
                Err(error)
            } else {
                Err(error.context(format!("and putting things back failed too: {}", failed.join("; "))))
            }
        }
    }
}

/// Moves a file or folder; across file systems (where renaming can't), by copying it and
/// removing the original.
pub(crate) fn move_path(from: &Path, to: &Path) -> Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => move_by_copy(from, to),
        Err(e) => Err(e).with_context(|| format!("moving {} to {}", from.display(), to.display())),
    }
}

fn move_by_copy(from: &Path, to: &Path) -> Result<()> {
    copy_recursively(from, to)?;
    remove_from_disk(from).inspect_err(|_| {
        remove_from_disk(to).ok();
    })
}

/// Copies a file or a folder tree.
pub(crate) fn copy_recursively(from: &Path, to: &Path) -> Result<()> {
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

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(files_under(&path));
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// `project`, evaluated, with its files kept for undoing.
fn project(path: &Path, config: &DotnetConfig, undo: &mut Undo) -> Result<dotnet_model::Project> {
    let project = dotnet_model::evaluate(path, &config.eval_options()).with_context(|| format!("reading {}", paths::file_name(path)))?;
    undo.keep_project(&project)?;
    Ok(project)
}

/// Lists in `project_path` what was just put at `path`: the file, or every file of the
/// folder (an empty folder, as a folder). The project is evaluated once, after the files
/// are there, so the SDK's globs cover what they include.
fn add_items(project_path: &Path, path: &Path, config: &DotnetConfig, undo: &mut Undo) -> Result<()> {
    let project = project(project_path, config, undo)?;
    let item_types = config.item_types();
    if path.is_dir() {
        let files = files_under(path);
        if files.is_empty() {
            return edit::add_folder(&project, path);
        }
        for file in files {
            edit::add_file(&project, &file, &edit::item_type_for(&file, &item_types), None)?;
        }
        Ok(())
    } else {
        edit::add_file(&project, path, &edit::item_type_for(path, &item_types), None)
    }
}

/// A new file with `content` in `project`, listed (after `anchor`'s file when given, for
/// projects whose order matters).
pub(crate) fn create_file(project_path: &Path, path: &Path, content: &str, anchor: Option<(&Path, Position)>, config: &DotnetConfig) -> Result<()> {
    transaction(|undo| {
        if path.exists() {
            bail!("{} already exists", paths::file_name(path));
        }
        if let Some(parent) = path.parent().filter(|p| !p.exists()) {
            undo.created(top_missing(parent));
            std::fs::create_dir_all(parent)?;
        }
        undo.created(path);
        std::fs::write(path, content)?;
        let project = project(project_path, config, undo)?;
        edit::add_file(&project, path, &edit::item_type_for(path, &config.item_types()), anchor)
    })
}

/// The highest folder of `dir` that doesn't exist yet: what creating `dir` creates.
fn top_missing(dir: &Path) -> &Path {
    let mut top = dir;
    while let Some(parent) = top.parent().filter(|p| !p.exists()) {
        top = parent;
    }
    top
}

/// A new, empty folder in `project`.
pub(crate) fn create_folder(project_path: &Path, path: &Path, config: &DotnetConfig) -> Result<()> {
    transaction(|undo| {
        if path.exists() {
            bail!("{} already exists", paths::file_name(path));
        }
        undo.created(top_missing(path));
        std::fs::create_dir_all(path)?;
        edit::add_folder(&project(project_path, config, undo)?, path)
    })
}

/// Renames a file or folder: in its project, or as a solution item of `folder`.
pub(crate) fn rename(path: &Path, to: &Path, project_path: Option<&Path>, solution_item: Option<(&Path, &str)>, config: &DotnetConfig) -> Result<()> {
    transaction(|undo| {
        let only_case = to.to_string_lossy().to_lowercase() == path.to_string_lossy().to_lowercase();
        if to.exists() && !only_case {
            bail!("{} already exists", paths::file_name(to));
        }
        move_path(path, to)?;
        undo.moved(path, to);
        if let Some(project_path) = project_path {
            edit::rename_path(&project(project_path, config, undo)?, path, to)?;
        }
        if let Some((solution, folder)) = solution_item {
            undo.keep(solution)?;
            solution::edit_file(solution, &SolutionEdit::RemoveFile { folder: folder.to_string(), path: path.to_path_buf() })?;
            solution::edit_file(solution, &SolutionEdit::AddFile { folder: folder.to_string(), path: to.to_path_buf() })?;
        }
        Ok(())
    })
}

/// Renames a project file, in the solution (`solution`: its file and the project's id)
/// and in the projects that reference it.
pub(crate) fn rename_project(path: &Path, to: &Path, solution: Option<(&Path, &str)>, referencing: &[PathBuf], config: &DotnetConfig) -> Result<()> {
    transaction(|undo| {
        if to.exists() {
            bail!("{} already exists", paths::file_name(to));
        }
        move_path(path, to)?;
        undo.moved(path, to);
        if let Some((solution, id)) = solution {
            undo.keep(solution)?;
            solution::edit_file(solution, &SolutionEdit::RenameProject { id: id.to_string(), path: to.to_path_buf() })?;
        }
        for other in referencing {
            edit::rename_path(&project(other, config, undo)?, path, to)?;
        }
        Ok(())
    })
}

/// Pastes `sources` (each with the project it is in) into `dir` of `project`: moves them
/// for a cut, copies them otherwise. All or nothing.
pub(crate) fn paste(sources: &[(PathBuf, Option<PathBuf>)], dir: &Path, project_path: &Path, cut: bool, config: &DotnetConfig) -> Result<()> {
    transaction(|undo| {
        for (source, source_project) in sources {
            if dir.starts_with(source) {
                bail!("cannot paste a folder into itself");
            }
            let mut target = dir.join(source.file_name().context("no file name")?);
            if target.exists() {
                if cut && target == *source {
                    continue;
                }
                target = paths::copy_name(&target);
            }
            if cut {
                move_path(source, &target)?;
                undo.moved(source, &target);
                match source_project {
                    Some(sp) if sp == project_path => edit::rename_path(&project(project_path, config, undo)?, source, &target)?,
                    other => {
                        if let Some(sp) = other {
                            edit::remove_path(&project(sp, config, undo)?, source)?;
                        }
                        add_items(project_path, &target, config, undo)?;
                    }
                }
            } else {
                undo.created(&target);
                copy_recursively(source, &target)?;
                add_items(project_path, &target, config, undo)?;
            }
        }
        Ok(())
    })
}

/// A copy of `path` next to it (`X copy.cs`), in `project`; returns where.
pub(crate) fn duplicate(path: &Path, project_path: &Path, config: &DotnetConfig) -> Result<PathBuf> {
    transaction(|undo| {
        let target = paths::copy_name(path);
        undo.created(&target);
        copy_recursively(path, &target)?;
        add_items(project_path, &target, config, undo)?;
        Ok(target)
    })
}

/// Moves (in the same project) or copies (from another project, as Visual Studio does)
/// `source` into `dir` of `target_project`.
pub(crate) fn drop_into(source: &Path, source_project: &Path, dir: &Path, target_project: &Path, config: &DotnetConfig) -> Result<()> {
    transaction(|undo| {
        let mut destination = dir.join(source.file_name().context("no file name")?);
        if source_project == target_project {
            if destination.exists() {
                bail!("{} already exists there", paths::file_name(&destination));
            }
            move_path(source, &destination)?;
            undo.moved(source, &destination);
            edit::rename_path(&project(source_project, config, undo)?, source, &destination)
        } else {
            if destination.exists() {
                destination = paths::copy_name(&destination);
            }
            undo.created(&destination);
            copy_recursively(source, &destination)?;
            add_items(target_project, &destination, config, undo)
        }
    })
}

/// Takes `path` out of `project`'s items, then deletes it with `delete` (to the Trash);
/// a link only leaves the project. When deleting fails, the project gets its items back.
pub(crate) async fn remove(path: &Path, project_path: &Path, is_link: bool, config: &DotnetConfig, delete: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    let mut undo = Undo::default();
    let edited = (|| {
        let project = project(project_path, config, &mut undo)?;
        edit::remove_path(&project, path)
    })();
    let result = match edited {
        Ok(()) if is_link => Ok(()),
        Ok(()) => delete.await,
        Err(e) => Err(e),
    };
    if let Err(error) = result {
        let failed = undo.rollback();
        if !failed.is_empty() {
            return Err(error.context(format!("and putting things back failed too: {}", failed.join("; "))));
        }
        return Err(error);
    }
    Ok(())
}

/// Solution edits made one after another, all or none (adding files to a folder it
/// creates first, a new solution with its projects).
pub(crate) fn edit_solution(solution: &Path, edits: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    transaction(|undo| {
        undo.keep(solution)?;
        edits(solution)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD_STYLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Project ToolsVersion="15.0" xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <PropertyGroup>
    <OutputType>Library</OutputType>
  </PropertyGroup>
  <ItemGroup>
    <Compile Include="A.cs" />
  </ItemGroup>
</Project>
"#;

    /// An old-style project (it lists every file), with `A.cs`.
    fn project_dir(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{name}.csproj")), OLD_STYLE).unwrap();
        std::fs::write(dir.join("A.cs"), "class A {}").unwrap();
        dir.join(format!("{name}.csproj"))
    }

    fn text(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    /// A project file that no longer parses: the next edit of it fails.
    fn break_project(project: &Path) {
        std::fs::write(project, "<Project><ItemGroup>").unwrap();
    }

    fn config() -> DotnetConfig {
        DotnetConfig::default()
    }

    #[test]
    fn creates_files_and_folders_and_lists_them() {
        let root = tempfile::tempdir().unwrap();
        let project = project_dir(root.path(), "App");
        let dir = project.parent().unwrap();
        create_file(&project, &dir.join("Models/B.cs"), "class B {}", None, &config()).unwrap();
        assert!(text(&project).contains(r#"Include="Models\B.cs""#), "{}", text(&project));
        assert!(create_file(&project, &dir.join("A.cs"), "", None, &config()).unwrap_err().to_string().contains("already exists"));
        create_folder(&project, &dir.join("Empty"), &config()).unwrap();
        assert!(dir.join("Empty").is_dir() && text(&project).contains(r#"Folder Include="Empty\""#));
    }

    /// The project file fails to parse: the new file (and the folders made for it) go.
    #[test]
    fn a_failed_create_leaves_nothing_behind() {
        let root = tempfile::tempdir().unwrap();
        let project = project_dir(root.path(), "App");
        let dir = project.parent().unwrap().to_path_buf();
        break_project(&project);
        let before = text(&project);
        assert!(create_file(&project, &dir.join("New/Deep/B.cs"), "class B {}", None, &config()).is_err());
        assert!(!dir.join("New").exists(), "the folders made for it are gone");
        assert!(create_folder(&project, &dir.join("Empty/Inner"), &config()).is_err());
        assert!(!dir.join("Empty").exists());
        assert_eq!(text(&project), before);
    }

    #[test]
    fn renames_files_in_their_project() {
        let root = tempfile::tempdir().unwrap();
        let project = project_dir(root.path(), "App");
        let dir = project.parent().unwrap();
        rename(&dir.join("A.cs"), &dir.join("B.cs"), Some(&project), None, &config()).unwrap();
        assert!(dir.join("B.cs").is_file() && !dir.join("A.cs").exists());
        assert!(text(&project).contains(r#"Include="B.cs""#) && !text(&project).contains(r#"Include="A.cs""#));
    }

    /// The project edit fails: the file gets its old name back.
    #[test]
    fn a_failed_rename_moves_the_file_back() {
        let root = tempfile::tempdir().unwrap();
        let project = project_dir(root.path(), "App");
        let dir = project.parent().unwrap();
        break_project(&project);
        assert!(rename(&dir.join("A.cs"), &dir.join("B.cs"), Some(&project), None, &config()).is_err());
        assert!(dir.join("A.cs").is_file() && !dir.join("B.cs").exists());
    }

    /// A project that references the renamed one fails to update: the project file gets
    /// its name back, and the solution and the other references their text.
    #[test]
    fn a_failed_project_rename_puts_everything_back() {
        let root = tempfile::tempdir().unwrap();
        let app = project_dir(root.path(), "App");
        let (good, bad) = (project_dir(root.path(), "Good"), project_dir(root.path(), "Bad"));
        for referencing in [&good, &bad] {
            let reference = format!(r#"<ItemGroup><ProjectReference Include="..\App\App.csproj" /></ItemGroup>"#);
            std::fs::write(referencing, OLD_STYLE.replace("</Project>", &format!("  {reference}\n</Project>"))).unwrap();
        }
        let solution = root.path().join("All.slnx");
        std::fs::write(&solution, "<Solution>\n  <Project Path=\"App/App.csproj\" />\n</Solution>\n").unwrap();
        let (good_before, solution_before) = (text(&good), text(&solution));
        break_project(&bad);
        let to = app.with_file_name("Core.csproj");
        let error = rename_project(&app, &to, Some((&solution, "App/App.csproj")), &[good.clone(), bad.clone()], &config());
        assert!(error.is_err());
        assert!(app.is_file() && !to.exists(), "the project file has its name back");
        assert_eq!(text(&good), good_before, "the reference that was updated is restored");
        assert_eq!(text(&solution), solution_before);
    }

    #[test]
    fn pastes_copies_and_moves_between_projects() {
        let root = tempfile::tempdir().unwrap();
        let (app, lib) = (project_dir(root.path(), "App"), project_dir(root.path(), "Lib"));
        let (app_dir, lib_dir) = (app.parent().unwrap(), lib.parent().unwrap());
        std::fs::write(app_dir.join("B.cs"), "class B {}").unwrap();
        std::fs::write(&app, text(&app).replace(r#"<Compile Include="A.cs" />"#, r#"<Compile Include="A.cs" /><Compile Include="B.cs" />"#)).unwrap();

        paste(&[(app_dir.join("A.cs"), Some(app.clone()))], lib_dir, &lib, false, &config()).unwrap();
        assert!(lib_dir.join("A copy.cs").is_file() && app_dir.join("A.cs").is_file(), "a copy, named apart from Lib's own A.cs");
        assert!(text(&lib).contains(r#"Include="A copy.cs""#));

        paste(&[(app_dir.join("B.cs"), Some(app.clone()))], lib_dir, &lib, true, &config()).unwrap();
        assert!(lib_dir.join("B.cs").is_file() && !app_dir.join("B.cs").exists());
        assert!(text(&lib).contains(r#"Include="B.cs""#) && !text(&app).contains(r#"Include="B.cs""#));
    }

    /// The second of two cut files is missing: the first goes back where it was, and
    /// both projects get their text back.
    #[test]
    fn a_failed_paste_undoes_the_files_already_pasted() {
        let root = tempfile::tempdir().unwrap();
        let (app, lib) = (project_dir(root.path(), "App"), project_dir(root.path(), "Lib"));
        let (app_dir, lib_dir) = (app.parent().unwrap().to_path_buf(), lib.parent().unwrap().to_path_buf());
        std::fs::create_dir_all(lib_dir.join("In")).unwrap();
        let (app_before, lib_before) = (text(&app), text(&lib));
        let sources = [(app_dir.join("A.cs"), Some(app.clone())), (app_dir.join("Missing.cs"), Some(app.clone()))];
        assert!(paste(&sources, &lib_dir.join("In"), &lib, true, &config()).is_err());
        assert!(app_dir.join("A.cs").is_file() && !lib_dir.join("In/A.cs").exists());
        assert_eq!((text(&app), text(&lib)), (app_before, lib_before));
    }

    #[test]
    fn duplicates_folders_with_their_files() {
        let root = tempfile::tempdir().unwrap();
        let app = project_dir(root.path(), "App");
        let dir = app.parent().unwrap();
        std::fs::create_dir_all(dir.join("Models")).unwrap();
        std::fs::write(dir.join("Models/M.cs"), "").unwrap();
        let copy = duplicate(&dir.join("Models"), &app, &config()).unwrap();
        assert_eq!(copy, dir.join("Models copy"));
        assert!(copy.join("M.cs").is_file() && text(&app).contains(r#"Include="Models copy\M.cs""#));
    }

    /// A copy whose project can't be updated is removed again.
    #[test]
    fn a_failed_copy_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let (app, lib) = (project_dir(root.path(), "App"), project_dir(root.path(), "Lib"));
        let (app_dir, lib_dir) = (app.parent().unwrap(), lib.parent().unwrap());
        break_project(&lib);
        assert!(drop_into(&app_dir.join("A.cs"), &app, lib_dir, &lib, &config()).is_err());
        assert!(!lib_dir.join("A copy.cs").exists());
        assert!(duplicate(&lib_dir.join("A.cs"), &lib, &config()).is_err());
        assert!(!lib_dir.join("A copy.cs").exists());
    }

    #[test]
    fn moves_within_a_project() {
        let root = tempfile::tempdir().unwrap();
        let app = project_dir(root.path(), "App");
        let dir = app.parent().unwrap();
        std::fs::create_dir_all(dir.join("Sub")).unwrap();
        drop_into(&dir.join("A.cs"), &app, &dir.join("Sub"), &app, &config()).unwrap();
        assert!(dir.join("Sub/A.cs").is_file() && text(&app).contains(r#"Include="Sub\A.cs""#));
    }

    /// Deleting fails (the Trash refused): the file stays listed in its project.
    #[test]
    fn a_failed_delete_keeps_the_item() {
        let root = tempfile::tempdir().unwrap();
        let app = project_dir(root.path(), "App");
        let file = app.parent().unwrap().join("A.cs");
        let before = text(&app);
        let refused = async { Err(anyhow::anyhow!("the Trash is full")) };
        assert!(futures::executor::block_on(remove(&file, &app, false, &config(), refused)).is_err());
        assert_eq!(text(&app), before);
        futures::executor::block_on(remove(&file, &app, false, &config(), async { Ok(()) })).unwrap();
        assert!(!text(&app).contains(r#"Include="A.cs""#));
    }

    #[test]
    fn solution_edits_are_all_or_nothing() {
        let root = tempfile::tempdir().unwrap();
        let solution = root.path().join("All.slnx");
        std::fs::write(&solution, "<Solution>\n</Solution>\n").unwrap();
        let before = text(&solution);
        let failed = edit_solution(&solution, |s| {
            solution::edit_file(s, &SolutionEdit::CreateFolder { parent: None, name: "Solution Items".into() })?;
            bail!("the second edit failed")
        });
        assert!(failed.is_err());
        assert_eq!(text(&solution), before);
    }

    /// Where renaming can't (another file system), moving copies and removes.
    #[test]
    fn moves_by_copying_when_it_must() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("from/inner")).unwrap();
        std::fs::write(root.path().join("from/inner/f.txt"), "x").unwrap();
        move_by_copy(&root.path().join("from"), &root.path().join("to")).unwrap();
        assert!(!root.path().join("from").exists());
        assert_eq!(std::fs::read_to_string(root.path().join("to/inner/f.txt")).unwrap(), "x");
    }
}
