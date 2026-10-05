//! The files and folders a project shows: its items after `Include`, `Remove` and
//! `Update`, with the implicit globs SDK projects get, links to files outside the project
//! and `DependentUpon`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::glob::{self, Spec};
use super::{ItemOpKind, Project};
use crate::paths;

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectEntry {
    /// Where the entry shows in the project, relative to it (`Link` applied).
    pub relative: PathBuf,
    /// Where the file is on disk. For folders that only exist as link parents, the
    /// folder the link would be in.
    pub full: PathBuf,
    pub is_dir: bool,
    /// Outside the project's folder.
    pub is_link: bool,
    /// The file this one is nested under, resolved to a project-relative path.
    pub dependent_upon: Option<PathBuf>,
    /// Item types including it (`Compile`, `None`, …); empty for plain folders.
    pub item_types: Vec<String>,
    /// Listed in the project but not on disk.
    pub missing: bool,
}

#[derive(Clone, Debug)]
pub struct ItemOptions {
    /// Folder names never shown (`bin`, `obj`, …), compared case-insensitively.
    pub ignored: Vec<String>,
}

impl Default for ItemOptions {
    fn default() -> Self {
        Self { ignored: ["bin", "obj", "node_modules", ".vs", ".ds_store", ".git", ".idea"].map(String::from).to_vec() }
    }
}

impl ItemOptions {
    fn is_ignored(&self, name: &str) -> bool {
        self.ignored.iter().any(|ignored| ignored.eq_ignore_ascii_case(name))
    }
}

#[derive(Default)]
struct Entries {
    list: Vec<ProjectEntry>,
    index: HashMap<String, usize>,
}

fn key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

impl Entries {
    fn get_mut(&mut self, relative: &Path) -> Option<&mut ProjectEntry> {
        self.index.get(&key(relative)).map(|&i| &mut self.list[i])
    }

    fn add(&mut self, entry: ProjectEntry) -> &mut ProjectEntry {
        let k = key(&entry.relative);
        if let Some(&i) = self.index.get(&k) {
            let existing = &mut self.list[i];
            for item_type in entry.item_types {
                if !existing.item_types.contains(&item_type) {
                    existing.item_types.push(item_type);
                }
            }
            if entry.dependent_upon.is_some() {
                existing.dependent_upon = entry.dependent_upon;
            }
            return existing;
        }
        self.ensure_parents(&entry.relative, &entry.full, entry.is_link);
        self.index.insert(k, self.list.len());
        self.list.push(entry);
        self.list.last_mut().unwrap()
    }

    fn ensure_parents(&mut self, relative: &Path, full: &Path, is_link: bool) {
        let mut parents = Vec::new();
        let (mut rel, mut abs) = (relative.parent(), full.parent());
        while let Some(r) = rel.filter(|r| !r.as_os_str().is_empty()) {
            if self.index.contains_key(&key(r)) {
                break;
            }
            parents.push((r.to_path_buf(), abs.map(Path::to_path_buf).unwrap_or_default()));
            rel = r.parent();
            abs = abs.and_then(Path::parent);
        }
        for (relative, full) in parents.into_iter().rev() {
            self.index.insert(key(&relative), self.list.len());
            self.list.push(ProjectEntry { relative, full, is_dir: true, is_link, dependent_upon: None, item_types: Vec::new(), missing: false });
        }
    }

    fn remove_where(&mut self, item_type: &str, mut pred: impl FnMut(&ProjectEntry) -> bool) {
        for entry in &mut self.list {
            if !entry.is_dir && pred(entry) {
                entry.item_types.retain(|t| !t.eq_ignore_ascii_case(item_type));
            }
        }
        self.list.retain(|e| e.is_dir || !e.item_types.is_empty());
        self.reindex();
    }

    fn reindex(&mut self) {
        self.index = self.list.iter().enumerate().map(|(i, e)| (key(&e.relative), i)).collect();
    }
}

/// Files under `base`, skipping ignored folders, with directories too when asked.
fn walk(base: &Path, options: &ItemOptions, with_dirs: bool) -> Vec<(PathBuf, bool)> {
    walkdir::WalkDir::new(base)
        .min_depth(1)
        .follow_links(true)
        .into_iter()
        .filter_entry(|e| !(e.file_type().is_dir() && options.is_ignored(&e.file_name().to_string_lossy())))
        .filter_map(Result::ok)
        .filter(|e| !options.is_ignored(&e.file_name().to_string_lossy()))
        .filter(|e| with_dirs || !e.file_type().is_dir())
        .map(|e| (e.path().to_path_buf(), e.file_type().is_dir()))
        .collect()
}

/// `%(LinkBase)/%(RecursiveDir)%(Filename)%(Extension)` and friends.
fn link_path(template: &str, full: &Path, recursive_dir: &Path, link_base: &str) -> PathBuf {
    let ext = full.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let mut recursive = paths::to_msbuild(recursive_dir);
    if !recursive.is_empty() {
        recursive.push('\\');
    }
    let text = template
        .replace("%(Extension)", &ext)
        .replace("%(Filename)", &paths::file_stem(full))
        .replace("%(RecursiveDir)", &recursive)
        .replace("%(LinkBase)", link_base)
        .replace("%(Identity)", &paths::file_name(full));
    paths::normalize(&paths::from_msbuild(text.trim_start_matches(['\\', '/'])))
}

/// The entries of a project, in the order MSBuild would produce them.
pub fn entries(project: &Project, options: &ItemOptions) -> Vec<ProjectEntry> {
    let dir = project.dir().to_path_buf();
    let mut entries = Entries::default();
    let props = &project.properties;

    let default_items = project.is_sdk() && !project.is_fsharp() && !props.is_false("EnableDefaultItems");
    if default_items {
        let code_ext = project.code_extension();
        let compile = !props.is_false("EnableDefaultCompileItems");
        let resources = !props.is_false("EnableDefaultEmbeddedResourceItems");
        let none = !props.is_false("EnableDefaultNoneItems");
        for (full, is_dir) in walk(&dir, options, true) {
            if full == project.path || matches!(paths::extension(&full).as_str(), "user" | "vssscc") {
                continue;
            }
            let Some(relative) = paths::relative(&dir, &full) else { continue };
            if relative.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')) {
                continue;
            }
            let ext = paths::extension(&full);
            let item_type = if is_dir {
                None
            } else if ext == code_ext {
                compile.then_some("Compile")
            } else if ext == "resx" {
                resources.then_some("EmbeddedResource")
            } else {
                none.then_some("None")
            };
            if !is_dir && item_type.is_none() {
                continue;
            }
            entries.add(ProjectEntry {
                relative,
                full,
                is_dir,
                is_link: false,
                dependent_upon: None,
                item_types: item_type.map(String::from).into_iter().collect(),
                missing: false,
            });
        }
    }

    let mut walk_cache: HashMap<PathBuf, Vec<(PathBuf, bool)>> = HashMap::new();
    for op in &project.items {
        let specs = glob::specs(&dir, &op.value);
        match op.kind {
            ItemOpKind::Include => {
                let excludes = op.exclude.as_deref().map(|e| glob::specs(&dir, e)).unwrap_or_default();
                for spec in &specs {
                    let matches: Vec<(PathBuf, bool)> = if spec.is_glob() {
                        let files = walk_cache.entry(spec.base.clone()).or_insert_with(|| walk(&spec.base, options, op.item_type == "Folder"));
                        files.iter().filter(|(path, _)| spec.matches(path)).cloned().collect()
                    } else {
                        let is_dir = op.item_type == "Folder" || spec.pattern.is_dir();
                        vec![(spec.pattern.clone(), is_dir)]
                    };
                    for (full, is_dir) in matches {
                        if glob::any_match(&excludes, &full) {
                            continue;
                        }
                        add_item(&mut entries, project, op, spec, full, is_dir);
                    }
                }
            }
            ItemOpKind::Remove => {
                entries.remove_where(&op.item_type, |entry| glob::any_match(&specs, &entry.full));
            }
            ItemOpKind::Update => {
                let matching: Vec<PathBuf> = entries.list.iter().filter(|e| !e.is_dir && glob::any_match(&specs, &e.full)).map(|e| e.relative.clone()).collect();
                for relative in matching {
                    let Some(entry) = entries.get_mut(&relative) else { continue };
                    if let Some(dependent) = &op.dependent_upon {
                        entry.dependent_upon = Some(resolve_dependent(&relative, dependent));
                    }
                    if op.link.is_some() || op.link_base.is_some() {
                        let full = entry.full.clone();
                        let spec = specs.iter().find(|s| s.matches(&full)).unwrap();
                        let new_relative = display_path(project, op, spec, &full);
                        if new_relative != relative {
                            let mut moved = entries.list.remove(entries.index[&key(&relative)]);
                            entries.reindex();
                            moved.relative = new_relative;
                            entries.add(moved);
                        }
                    }
                }
            }
        }
    }
    entries.list
}

fn resolve_dependent(relative: &Path, dependent: &str) -> PathBuf {
    let parent = relative.parent().unwrap_or(Path::new(""));
    paths::normalize(&parent.join(paths::from_msbuild(dependent)))
}

fn display_path(project: &Project, op: &super::ItemOp, spec: &Spec, full: &Path) -> PathBuf {
    let dir = project.dir();
    let inside = paths::is_within(full, dir);
    match (&op.link, inside) {
        (Some(link), _) => link_path(link, full, &spec.recursive_dir(full), op.link_base.as_deref().unwrap_or("")),
        (None, true) => paths::relative(dir, full).unwrap_or_else(|| full.to_path_buf()),
        (None, false) => link_path("%(LinkBase)\\%(RecursiveDir)%(Filename)%(Extension)", full, &spec.recursive_dir(full), op.link_base.as_deref().unwrap_or("")),
    }
}

fn add_item(entries: &mut Entries, project: &Project, op: &super::ItemOp, spec: &Spec, full: PathBuf, is_dir: bool) {
    let relative = display_path(project, op, spec, &full);
    if relative.as_os_str().is_empty() {
        return;
    }
    let is_link = !paths::is_within(&full, project.dir());
    let missing = !full.exists();
    let item_types = if is_dir || op.item_type == "Folder" { Vec::new() } else { vec![op.item_type.clone()] };
    let dependent_upon = op.dependent_upon.as_deref().map(|d| resolve_dependent(&relative, d));
    entries.add(ProjectEntry { relative, full, is_dir, is_link, dependent_upon, item_types, missing });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msbuild::{EvalOptions, evaluate};

    fn touch(dir: &Path, name: &str) {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    fn relatives(entries: &[ProjectEntry]) -> Vec<String> {
        let mut list: Vec<String> = entries.iter().map(|e| e.relative.to_string_lossy().replace('\\', "/")).collect();
        list.sort();
        list
    }

    #[test]
    fn sdk_projects_show_their_folder_minus_removed_and_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for file in ["App/Program.cs", "App/Models/User.cs", "App/bin/Debug/App.dll", "App/obj/x.json", "App/Old/Legacy.cs", "App/appsettings.json", "App/Empty/.keep", "Shared/Common/Util.cs"] {
            touch(&root, file);
        }
        std::fs::create_dir_all(root.join("App/Assets")).unwrap();
        std::fs::write(
            root.join("App/App.csproj"),
            r#"<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <Compile Remove="Old\**" />
    <Compile Include="..\Shared\**\*.cs" LinkBase="Shared" />
    <None Update="appsettings.json"><DependentUpon>Program.cs</DependentUpon></None>
  </ItemGroup>
</Project>"#,
        )
        .unwrap();
        let project = evaluate(&root.join("App/App.csproj"), &EvalOptions::default()).unwrap();
        let entries = entries(&project, &ItemOptions::default());
        assert_eq!(
            relatives(&entries),
            vec!["Assets", "Empty", "Models", "Models/User.cs", "Old", "Program.cs", "Shared", "Shared/Common", "Shared/Common/Util.cs", "appsettings.json"]
        );
        let util = entries.iter().find(|e| e.relative.ends_with("Util.cs")).unwrap();
        assert!(util.is_link);
        assert_eq!(util.full, root.join("Shared/Common/Util.cs"));
        let settings = entries.iter().find(|e| e.relative == Path::new("appsettings.json")).unwrap();
        assert_eq!(settings.dependent_upon.as_deref(), Some(Path::new("Program.cs")));
        assert_eq!(settings.item_types, vec!["None"]);
    }

    #[test]
    fn fsharp_projects_keep_their_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for file in ["Types.fs", "Logic/Core.fs", "Program.fs", "notes.txt"] {
            touch(&root, file);
        }
        std::fs::write(
            root.join("App.fsproj"),
            r#"<Project Sdk="Microsoft.NET.Sdk"><ItemGroup><Compile Include="Types.fs" /><Compile Include="Logic\Core.fs" /><Compile Include="Program.fs" /><Compile Include="Missing.fs" /><Folder Include="Docs\" /></ItemGroup></Project>"#,
        )
        .unwrap();
        let project = evaluate(&root.join("App.fsproj"), &EvalOptions::default()).unwrap();
        let entries = entries(&project, &ItemOptions::default());
        let order: Vec<String> = entries.iter().map(|e| e.relative.to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(order, vec!["Types.fs", "Logic", "Logic/Core.fs", "Program.fs", "Missing.fs", "Docs"]);
        assert!(entries[4].missing);
        assert!(entries[5].is_dir);
    }

    #[test]
    fn link_templates() {
        assert_eq!(link_path("%(LinkBase)\\%(RecursiveDir)%(Filename)%(Extension)", Path::new("/s/a/b.cs"), Path::new("a"), "Lib"), PathBuf::from("Lib/a/b.cs"));
        assert_eq!(link_path("Properties\\%(Filename)%(Extension)", Path::new("/s/x.cs"), Path::new(""), ""), PathBuf::from("Properties/x.cs"));
    }
}
