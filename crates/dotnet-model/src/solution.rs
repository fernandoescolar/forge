//! One model for `.sln` and `.slnx` solutions, finding them in a folder, and edits that
//! work the same on both formats.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use crate::paths;
use crate::sln::{self, SlnEditor, SlnFile};
use crate::slnx::{self, SlnxEditor, SlnxFile};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SolutionFormat {
    Sln,
    Slnx,
    /// No solution file: the projects found under a folder.
    Folder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectKind {
    /// A regular MSBuild project.
    Default,
    /// A shared project (`.shproj`, items in a `.projitems`).
    Shared,
    /// Projects that have no references node, such as Node.js or deployment projects.
    NoReferences,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SolutionProject {
    /// The guid in a `.sln`, the relative path in a `.slnx`.
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub kind: ProjectKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SolutionFolder {
    /// The guid in a `.sln`, the full name (`/a/b/`) in a `.slnx`.
    pub id: String,
    pub name: String,
    pub folders: Vec<SolutionFolder>,
    pub projects: Vec<SolutionProject>,
    /// Solution items: files shown in the folder.
    pub files: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Solution {
    pub path: PathBuf,
    pub format: SolutionFormat,
    pub folders: Vec<SolutionFolder>,
    pub projects: Vec<SolutionProject>,
    pub configurations: Vec<String>,
}

impl Solution {
    pub fn name(&self) -> String {
        if self.format == SolutionFormat::Folder { paths::file_name(&self.path) } else { paths::file_stem(&self.path) }
    }

    pub fn dir(&self) -> &Path {
        if self.format == SolutionFormat::Folder { &self.path } else { self.path.parent().unwrap_or(Path::new(".")) }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        match paths::extension(path).as_str() {
            "sln" => Self::from_sln(path, &text),
            "slnx" => Self::from_slnx(path, &text),
            _ => bail!("{} is not a solution", path.display()),
        }
    }

    pub fn from_sln(path: &Path, text: &str) -> Result<Self> {
        let file = SlnFile::parse(text)?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let parent_of = |guid: &str| file.nested.get(guid).cloned();

        fn build_folder(file: &SlnFile, dir: &Path, guid: &str) -> SolutionFolder {
            let entry = file.project(guid).unwrap();
            let mut folder = SolutionFolder {
                id: entry.guid.clone(),
                name: entry.name.clone(),
                folders: Vec::new(),
                projects: Vec::new(),
                files: entry.solution_items.iter().map(|item| paths::resolve(dir, item)).collect(),
            };
            for child in file.projects.iter().filter(|p| file.nested.get(&p.guid).is_some_and(|parent| parent == guid)) {
                if child.is_folder() {
                    folder.folders.push(build_folder(file, dir, &child.guid));
                } else {
                    folder.projects.push(project(dir, child));
                }
            }
            folder
        }

        fn project(dir: &Path, entry: &sln::SlnProject) -> SolutionProject {
            let path = paths::resolve(dir, &entry.path);
            let ext = paths::extension(&path);
            let kind = if entry.type_guid == sln::SHARED_PROJECT_TYPE || ext == "shproj" {
                ProjectKind::Shared
            } else if ext == "njsproj" || ext == "deployproj" || ext == "sqlproj" {
                ProjectKind::NoReferences
            } else {
                ProjectKind::Default
            };
            SolutionProject { id: entry.guid.clone(), name: entry.name.clone(), path, kind }
        }

        let mut solution = Solution {
            path: path.to_path_buf(),
            format: SolutionFormat::Sln,
            folders: Vec::new(),
            projects: Vec::new(),
            configurations: file.configurations.clone(),
        };
        for entry in file.projects.iter().filter(|p| parent_of(&p.guid).is_none_or(|parent| file.project(&parent).is_none())) {
            if entry.is_folder() {
                solution.folders.push(build_folder(&file, dir, &entry.guid));
            } else {
                solution.projects.push(project(dir, entry));
            }
        }
        solution.sort();
        Ok(solution)
    }

    pub fn from_slnx(path: &Path, text: &str) -> Result<Self> {
        let file = SlnxFile::parse(text)?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let project = |relative: &str| {
            let path = paths::resolve(dir, relative);
            let kind = if paths::extension(&path) == "shproj" { ProjectKind::Shared } else { ProjectKind::Default };
            SolutionProject { id: relative.to_string(), name: paths::file_stem(&path), path, kind }
        };

        fn build(file: &SlnxFile, names: &[String], parent: &str, dir: &Path, project: &dyn Fn(&str) -> SolutionProject) -> Vec<SolutionFolder> {
            names
                .iter()
                .filter(|name| slnx::folder_parent(name).unwrap_or_else(|| "/".into()).eq_ignore_ascii_case(parent))
                .map(|name| {
                    let entry = file.folders.iter().find(|f| f.name.eq_ignore_ascii_case(name));
                    SolutionFolder {
                        id: name.clone(),
                        name: slnx::folder_leaf(name).to_string(),
                        folders: build(file, names, name, dir, project),
                        projects: entry.map(|f| f.projects.iter().map(|p| project(p)).collect()).unwrap_or_default(),
                        files: entry.map(|f| f.files.iter().map(|p| paths::resolve(dir, p)).collect()).unwrap_or_default(),
                    }
                })
                .collect()
        }

        let names = file.all_folder_names();
        let mut solution = Solution {
            path: path.to_path_buf(),
            format: SolutionFormat::Slnx,
            folders: build(&file, &names, "/", dir, &project),
            projects: file.projects.iter().map(|p| project(p)).collect(),
            configurations: file.configurations(),
        };
        solution.sort();
        Ok(solution)
    }

    /// A stand-in solution for a folder with projects but no solution file.
    pub fn from_folder(root: &Path, projects: Vec<PathBuf>) -> Self {
        let mut solution = Solution {
            path: root.to_path_buf(),
            format: SolutionFormat::Folder,
            folders: Vec::new(),
            projects: projects
                .into_iter()
                .map(|path| SolutionProject {
                    id: path.to_string_lossy().into_owned(),
                    name: paths::file_stem(&path),
                    kind: if paths::extension(&path) == "shproj" { ProjectKind::Shared } else { ProjectKind::Default },
                    path,
                })
                .collect(),
            configurations: vec!["Debug|Any CPU".into(), "Release|Any CPU".into()],
        };
        solution.sort();
        solution
    }

    fn sort(&mut self) {
        fn sort_folder(folder: &mut SolutionFolder) {
            folder.folders.sort_by_key(|f| f.name.to_lowercase());
            folder.projects.sort_by_key(|p| p.name.to_lowercase());
            folder.folders.iter_mut().for_each(sort_folder);
        }
        self.folders.sort_by_key(|f| f.name.to_lowercase());
        self.projects.sort_by_key(|p| p.name.to_lowercase());
        self.folders.iter_mut().for_each(sort_folder);
    }

    /// Every project, depth first.
    pub fn all_projects(&self) -> Vec<&SolutionProject> {
        fn collect<'a>(folder: &'a SolutionFolder, out: &mut Vec<&'a SolutionProject>) {
            folder.folders.iter().for_each(|f| collect(f, out));
            out.extend(folder.projects.iter());
        }
        let mut out = Vec::new();
        self.folders.iter().for_each(|f| collect(f, &mut out));
        out.extend(self.projects.iter());
        out
    }

    /// Every folder, depth first, with its parent's id.
    pub fn all_folders(&self) -> Vec<(&SolutionFolder, Option<&str>)> {
        fn collect<'a>(folder: &'a SolutionFolder, parent: Option<&'a str>, out: &mut Vec<(&'a SolutionFolder, Option<&'a str>)>) {
            out.push((folder, parent));
            folder.folders.iter().for_each(|f| collect(f, Some(&folder.id), out));
        }
        let mut out = Vec::new();
        self.folders.iter().for_each(|f| collect(f, None, &mut out));
        out
    }

    pub fn folder(&self, id: &str) -> Option<&SolutionFolder> {
        self.all_folders().into_iter().map(|(f, _)| f).find(|f| f.id.eq_ignore_ascii_case(id))
    }

    pub fn project_by_path(&self, path: &Path) -> Option<&SolutionProject> {
        let path = paths::normalize(path);
        self.all_projects().into_iter().find(|p| p.path == path)
    }

    /// The solution folder holding a project, if any.
    pub fn parent_of_project(&self, id: &str) -> Option<&SolutionFolder> {
        self.all_folders().into_iter().map(|(f, _)| f).find(|f| f.projects.iter().any(|p| p.id == id))
    }
}

/// An edit to a solution, expressed with the ids of its model.
#[derive(Clone, Debug, PartialEq)]
pub enum SolutionEdit {
    CreateFolder { parent: Option<String>, name: String },
    DeleteFolder { id: String },
    RenameFolder { id: String, name: String },
    MoveFolder { id: String, parent: Option<String> },
    MoveProject { id: String, parent: Option<String> },
    AddProject { path: PathBuf, parent: Option<String> },
    RemoveProject { id: String },
    /// After the project file was renamed on disk.
    RenameProject { id: String, path: PathBuf },
    AddFile { folder: String, path: PathBuf },
    RemoveFile { folder: String, path: PathBuf },
}

/// Applies an edit to a solution's text and returns the new text.
pub fn apply_edit(solution_path: &Path, text: &str, edit: &SolutionEdit) -> Result<String> {
    let dir = solution_path.parent().unwrap_or(Path::new("."));
    let relative = |path: &Path| paths::relative(dir, path).context("the file is on another drive than the solution");
    match paths::extension(solution_path).as_str() {
        "sln" => {
            let mut editor = SlnEditor::new(text);
            match edit {
                SolutionEdit::CreateFolder { parent, name } => {
                    editor.create_folder(name, parent.as_deref())?;
                }
                SolutionEdit::DeleteFolder { id } | SolutionEdit::RemoveProject { id } => editor.remove(id)?,
                SolutionEdit::RenameFolder { id, name } => editor.rename(id, name, None)?,
                SolutionEdit::MoveFolder { id, parent } | SolutionEdit::MoveProject { id, parent } => editor.move_to(id, parent.as_deref())?,
                SolutionEdit::AddProject { path, parent } => {
                    let guid = crate::msbuild::project_guid(path).unwrap_or_else(sln::new_guid);
                    editor.add_project(&paths::file_stem(path), &paths::to_msbuild(&relative(path)?), sln::project_type_for(path), &guid, parent.as_deref())?;
                }
                SolutionEdit::RenameProject { id, path } => editor.rename(id, &paths::file_stem(path), Some(&paths::to_msbuild(&relative(path)?)))?,
                SolutionEdit::AddFile { folder, path } => editor.add_solution_item(folder, &paths::to_msbuild(&relative(path)?))?,
                SolutionEdit::RemoveFile { folder, path } => editor.remove_solution_item(folder, &paths::to_msbuild(&relative(path)?))?,
            }
            Ok(editor.into_string())
        }
        "slnx" => {
            let mut editor = SlnxEditor::new(text);
            match edit {
                SolutionEdit::CreateFolder { parent, name } => {
                    editor.create_folder(parent.as_deref(), name)?;
                }
                SolutionEdit::DeleteFolder { id } => editor.delete_folder(id)?,
                SolutionEdit::RenameFolder { id, name } => {
                    editor.rename_folder(id, name)?;
                }
                SolutionEdit::MoveFolder { id, parent } => {
                    editor.move_folder(id, parent.as_deref())?;
                }
                SolutionEdit::MoveProject { id, parent } => editor.move_project(id, parent.as_deref())?,
                SolutionEdit::AddProject { path, parent } => editor.add_project(&paths::to_forward(&relative(path)?), parent.as_deref())?,
                SolutionEdit::RemoveProject { id } => editor.remove_project(id)?,
                SolutionEdit::RenameProject { id, path } => editor.rename_project(id, &paths::to_forward(&relative(path)?))?,
                SolutionEdit::AddFile { folder, path } => editor.add_file(folder, &paths::to_forward(&relative(path)?))?,
                SolutionEdit::RemoveFile { folder, path } => editor.remove_file(folder, &paths::to_forward(&relative(path)?))?,
            }
            Ok(editor.into_string())
        }
        _ => bail!("only .sln and .slnx solutions can be edited"),
    }
}

/// Applies an edit to a solution file on disk.
pub fn edit_file(solution_path: &Path, edit: &SolutionEdit) -> Result<()> {
    let text = std::fs::read_to_string(solution_path)?;
    let updated = apply_edit(solution_path, &text, edit)?;
    if updated != text {
        std::fs::write(solution_path, updated)?;
    }
    Ok(())
}

pub const SKIPPED_DIRS: &[&str] = &["bin", "obj", ".git", ".vs", ".idea", "node_modules", "target", "TestResults", "packages", ".vscode"];

fn skipped(entry: &walkdir::DirEntry) -> bool {
    entry.depth() > 0 && entry.file_type().is_dir() && entry.file_name().to_str().is_some_and(|name| SKIPPED_DIRS.contains(&name) || name.starts_with('.'))
}

/// Solutions under `root`: the ones at its top first, then deeper ones, each group sorted.
pub fn find_solutions(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut found: Vec<(usize, PathBuf)> = walkdir::WalkDir::new(root)
        .max_depth(max_depth)
        .into_iter()
        .filter_entry(|e| !skipped(e))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && matches!(paths::extension(e.path()).as_str(), "sln" | "slnx"))
        .map(|e| (e.depth(), e.path().to_path_buf()))
        .collect();
    found.sort();
    found.into_iter().map(|(_, path)| path).collect()
}

pub const PROJECT_EXTENSIONS: &[&str] = &["csproj", "fsproj", "vbproj", "shproj", "esproj", "sqlproj", "proj"];

/// Project files under `root`, not looking inside a project's own folder for more.
pub fn find_projects(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .max_depth(max_depth)
        .into_iter()
        .filter_entry(|e| !skipped(e))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && PROJECT_EXTENSIONS.contains(&paths::extension(e.path()).as_str()))
        .map(|e| e.path().to_path_buf())
        .collect();
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sln_model_nests_folders_and_resolves_paths() {
        let text = include_str!("../tests/fixtures/sample.sln");
        let solution = Solution::from_sln(Path::new("/repo/All.sln"), text).unwrap();
        assert_eq!(solution.name(), "All");
        assert_eq!(solution.folders.len(), 1);
        assert_eq!(solution.folders[0].name, "src");
        assert_eq!(solution.folders[0].projects[0].path, PathBuf::from("/repo/src/App/App.csproj"));
        assert_eq!(solution.folders[0].files, vec![PathBuf::from("/repo/README.md")]);
        assert_eq!(solution.projects[0].name, "Tests");
        assert_eq!(solution.all_projects().len(), 2);
    }

    #[test]
    fn edits_through_both_formats() {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in [("a.sln", crate::sln::empty_solution()), ("b.slnx", crate::slnx::empty_solution())] {
            let path = dir.path().join(name);
            std::fs::write(&path, text).unwrap();
            let project = dir.path().join("src/App/App.csproj");
            edit_file(&path, &SolutionEdit::CreateFolder { parent: None, name: "src".into() }).unwrap();
            let folder = Solution::load(&path).unwrap().folders[0].id.clone();
            edit_file(&path, &SolutionEdit::AddProject { path: project.clone(), parent: Some(folder.clone()) }).unwrap();
            edit_file(&path, &SolutionEdit::AddFile { folder: folder.clone(), path: dir.path().join("README.md") }).unwrap();
            let solution = Solution::load(&path).unwrap();
            assert_eq!(solution.folders[0].projects[0].path, project, "{name}");
            assert_eq!(solution.folders[0].files, vec![dir.path().join("README.md")], "{name}");

            let id = solution.folders[0].projects[0].id.clone();
            edit_file(&path, &SolutionEdit::MoveProject { id: id.clone(), parent: None }).unwrap();
            let solution = Solution::load(&path).unwrap();
            assert_eq!(solution.projects.len(), 1, "{name}");
            edit_file(&path, &SolutionEdit::RemoveProject { id }).unwrap();
            edit_file(&path, &SolutionEdit::DeleteFolder { id: folder }).unwrap();
            let solution = Solution::load(&path).unwrap();
            assert!(solution.projects.is_empty() && solution.folders.is_empty(), "{name}");
        }
    }

    #[test]
    fn finds_solutions_top_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/obj")).unwrap();
        std::fs::write(dir.path().join("src/Deep.sln"), "").unwrap();
        std::fs::write(dir.path().join("src/obj/Skip.sln"), "").unwrap();
        std::fs::write(dir.path().join("Top.slnx"), "").unwrap();
        let found = find_solutions(dir.path(), 4);
        assert_eq!(found, vec![dir.path().join("Top.slnx"), dir.path().join("src/Deep.sln")]);
    }
}
