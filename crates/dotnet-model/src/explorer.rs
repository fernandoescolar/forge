//! The solution explorer's tree, built from a solution and its evaluated projects:
//! folders, projects, dependencies and project files with Visual Studio's ordering and
//! file nesting.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::assets::Assets;
use crate::msbuild::Project;
use crate::msbuild::items::{self, ItemOptions, ProjectEntry};
use crate::paths;
use crate::solution::{Solution, SolutionFolder, SolutionProject};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Solution,
    SolutionFolder { id: String },
    /// A file in a solution folder.
    SolutionItem { folder: String, path: PathBuf },
    Project { path: PathBuf },
    /// A project the solution lists that could not be read.
    ProjectError { path: PathBuf, message: String },
    Dependencies { project: PathBuf },
    Frameworks { project: PathBuf },
    Framework { project: PathBuf, name: String },
    Packages { project: PathBuf },
    Package { project: PathBuf, name: String, version: Option<String> },
    PackageDependency { project: PathBuf, name: String, version: String },
    ProjectReferences { project: PathBuf },
    ProjectReference { project: PathBuf, target: PathBuf },
    Assemblies { project: PathBuf },
    Assembly { project: PathBuf, name: String },
    Folder { project: PathBuf, path: PathBuf, is_link: bool },
    File { project: PathBuf, path: PathBuf, is_link: bool, missing: bool },
}

impl NodeKind {
    /// The file or folder on disk the node stands for, if any.
    pub fn path(&self) -> Option<&Path> {
        match self {
            NodeKind::SolutionItem { path, .. }
            | NodeKind::Project { path }
            | NodeKind::ProjectError { path, .. }
            | NodeKind::Folder { path, .. }
            | NodeKind::File { path, .. }
            | NodeKind::ProjectReference { target: path, .. } => Some(path),
            _ => None,
        }
    }

    /// The project the node belongs to.
    pub fn project(&self) -> Option<&Path> {
        match self {
            NodeKind::Project { path } | NodeKind::ProjectError { path, .. } => Some(path),
            NodeKind::Dependencies { project }
            | NodeKind::Frameworks { project }
            | NodeKind::Framework { project, .. }
            | NodeKind::Packages { project }
            | NodeKind::Package { project, .. }
            | NodeKind::PackageDependency { project, .. }
            | NodeKind::ProjectReferences { project }
            | NodeKind::ProjectReference { project, .. }
            | NodeKind::Assemblies { project }
            | NodeKind::Assembly { project, .. }
            | NodeKind::Folder { project, .. }
            | NodeKind::File { project, .. } => Some(project),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Stable across rebuilds, for selection and expansion state.
    pub id: String,
    pub kind: NodeKind,
    pub label: String,
    /// Shown dimmed after the label: versions, "link", target frameworks.
    pub detail: Option<String>,
    pub children: Vec<Node>,
    pub expanded_by_default: bool,
}

#[derive(Clone, Debug)]
pub struct TreeOptions {
    pub items: ItemOptions,
    /// Nest `appsettings.Development.json` under `appsettings.json`, `Index.cshtml.cs`
    /// under `Index.cshtml`, ….
    pub nest_by_name: bool,
}

impl Default for TreeOptions {
    fn default() -> Self {
        Self { items: ItemOptions::default(), nest_by_name: true }
    }
}

/// A project as loaded for the tree: evaluated, or the error that stopped it.
pub type LoadedProjects = HashMap<PathBuf, Result<Project, String>>;

/// The nodes under a project: dependencies, then its folders and files. This is the
/// expensive part of the tree (it walks the project's folder), so callers cache it per
/// project and rebuild only the projects that change.
pub fn project_children(project: &Project, options: &TreeOptions) -> Vec<Node> {
    let mut children = Vec::new();
    if !project.is_shared() {
        children.push(dependencies_node(project));
    }
    children.extend(file_nodes(project, options));
    children
}

/// Children of every project that evaluated.
pub type ProjectChildren = HashMap<PathBuf, std::sync::Arc<Vec<Node>>>;

pub fn build(solution: &Solution, projects: &LoadedProjects, options: &TreeOptions) -> Node {
    let children: ProjectChildren = projects
        .iter()
        .filter_map(|(path, project)| Some((path.clone(), std::sync::Arc::new(project_children(project.as_ref().ok()?, options)))))
        .collect();
    build_with(solution, projects, &children)
}

/// The tree, with each project's children already computed.
pub fn build_with(solution: &Solution, projects: &LoadedProjects, children: &ProjectChildren) -> Node {
    let mut nodes: Vec<Node> = solution.folders.iter().map(|f| folder_node(f, projects, children)).collect();
    nodes.extend(solution.projects.iter().map(|p| project_node(p, projects, children)));
    let children = nodes;
    let count = solution.all_projects().len();
    Node {
        id: format!("solution:{}", solution.path.display()),
        kind: NodeKind::Solution,
        label: solution.name(),
        detail: Some(if count == 1 { "1 project".into() } else { format!("{count} projects") }),
        children,
        expanded_by_default: true,
    }
}

fn folder_node(folder: &SolutionFolder, projects: &LoadedProjects, cached: &ProjectChildren) -> Node {
    let mut children: Vec<Node> = folder.folders.iter().map(|f| folder_node(f, projects, cached)).collect();
    children.extend(folder.projects.iter().map(|p| project_node(p, projects, cached)));
    let mut files: Vec<&PathBuf> = folder.files.iter().collect();
    files.sort_by_key(|f| paths::file_name(f).to_lowercase());
    children.extend(files.into_iter().map(|path| Node {
        id: format!("item:{}:{}", folder.id, path.display()),
        kind: NodeKind::SolutionItem { folder: folder.id.clone(), path: path.clone() },
        label: paths::file_name(path),
        detail: None,
        children: Vec::new(),
        expanded_by_default: false,
    }));
    Node {
        id: format!("folder:{}", folder.id),
        kind: NodeKind::SolutionFolder { id: folder.id.clone() },
        label: folder.name.clone(),
        detail: None,
        children,
        expanded_by_default: true,
    }
}

fn project_node(entry: &SolutionProject, projects: &LoadedProjects, cached: &ProjectChildren) -> Node {
    let id = format!("project:{}", entry.path.display());
    match projects.get(&entry.path) {
        Some(Ok(project)) => {
            let children = cached.get(&entry.path).map(|c| c.as_ref().clone()).unwrap_or_default();
            let frameworks = project.target_frameworks();
            Node {
                id,
                kind: NodeKind::Project { path: entry.path.clone() },
                label: entry.name.clone(),
                detail: (!frameworks.is_empty()).then(|| frameworks.join(", ")),
                children,
                expanded_by_default: false,
            }
        }
        Some(Err(message)) => Node {
            id,
            kind: NodeKind::ProjectError { path: entry.path.clone(), message: message.clone() },
            label: entry.name.clone(),
            detail: Some(if entry.path.exists() { "(unavailable)".into() } else { "(not found)".into() }),
            children: Vec::new(),
            expanded_by_default: false,
        },
        None => Node {
            id,
            kind: NodeKind::Project { path: entry.path.clone() },
            label: entry.name.clone(),
            detail: Some("(loading)".into()),
            children: Vec::new(),
            expanded_by_default: false,
        },
    }
}

fn group(id: String, kind: NodeKind, label: &str, children: Vec<Node>) -> Node {
    Node { id, kind, label: label.into(), detail: None, children, expanded_by_default: false }
}

fn dependencies_node(project: &Project) -> Node {
    let path = project.path.clone();
    let key = path.display().to_string();
    let assets = Assets::load(&project.assets_file());
    let framework = project.target_frameworks().into_iter().next();
    let mut children = Vec::new();

    let frameworks = project.target_frameworks();
    if !frameworks.is_empty() {
        children.push(group(
            format!("frameworks:{key}"),
            NodeKind::Frameworks { project: path.clone() },
            "Frameworks",
            frameworks
                .into_iter()
                .map(|name| Node {
                    id: format!("framework:{key}:{name}"),
                    kind: NodeKind::Framework { project: path.clone(), name: name.clone() },
                    label: name,
                    detail: None,
                    children: Vec::new(),
                    expanded_by_default: false,
                })
                .collect(),
        ));
    }

    let mut packages: Vec<_> = project.package_references.iter().collect();
    packages.sort_by_key(|p| p.name.to_lowercase());
    children.push(group(
        format!("packages:{key}"),
        NodeKind::Packages { project: path.clone() },
        "Packages",
        packages
            .into_iter()
            .map(|package| {
                let mut visited = HashSet::new();
                Node {
                    id: format!("package:{key}:{}", package.name),
                    kind: NodeKind::Package { project: path.clone(), name: package.name.clone(), version: package.version.clone() },
                    label: package.name.clone(),
                    detail: package.version.clone(),
                    children: assets.as_ref().map(|a| package_dependencies(a, framework.as_deref(), &path, &package.name, &format!("package:{key}:{}", package.name), &mut visited, 0)).unwrap_or_default(),
                    expanded_by_default: false,
                }
            })
            .collect(),
    ));

    let mut references: Vec<_> = project.project_references.iter().collect();
    references.sort_by_key(|r| paths::file_stem(r).to_lowercase());
    children.push(group(
        format!("projectrefs:{key}"),
        NodeKind::ProjectReferences { project: path.clone() },
        "Projects",
        references
            .into_iter()
            .map(|target| Node {
                id: format!("projectref:{key}:{}", target.display()),
                kind: NodeKind::ProjectReference { project: path.clone(), target: target.clone() },
                label: paths::file_stem(target),
                detail: (!target.exists()).then(|| "(not found)".into()),
                children: Vec::new(),
                expanded_by_default: false,
            })
            .collect(),
    ));

    if !project.references.is_empty() {
        let mut assemblies: Vec<_> = project.references.iter().collect();
        assemblies.sort_by_key(|r| r.name.to_lowercase());
        children.push(group(
            format!("assemblies:{key}"),
            NodeKind::Assemblies { project: path.clone() },
            "Assemblies",
            assemblies
                .into_iter()
                .map(|assembly| Node {
                    id: format!("assembly:{key}:{}", assembly.name),
                    kind: NodeKind::Assembly { project: path.clone(), name: assembly.name.clone() },
                    label: assembly.name.clone(),
                    detail: assembly.version.clone(),
                    children: Vec::new(),
                    expanded_by_default: false,
                })
                .collect(),
        ));
    }

    group(format!("dependencies:{key}"), NodeKind::Dependencies { project: path }, "Dependencies", children)
}

fn package_dependencies(assets: &Assets, framework: Option<&str>, project: &Path, name: &str, parent_id: &str, visited: &mut HashSet<String>, depth: usize) -> Vec<Node> {
    if depth > 8 || !visited.insert(name.to_lowercase()) {
        return Vec::new();
    }
    let nodes = assets
        .dependencies(framework, name)
        .into_iter()
        .map(|dep| {
            let id = format!("{parent_id}/{}", dep.name);
            Node {
                children: package_dependencies(assets, framework, project, &dep.name, &id, visited, depth + 1),
                id,
                kind: NodeKind::PackageDependency { project: project.to_path_buf(), name: dep.name.clone(), version: dep.version.clone() },
                label: dep.name.clone(),
                detail: Some(dep.version.clone()),
                expanded_by_default: false,
            }
        })
        .collect();
    visited.remove(&name.to_lowercase());
    nodes
}

/// The project's files and folders as tree nodes.
pub fn file_nodes(project: &Project, options: &TreeOptions) -> Vec<Node> {
    let entries = items::entries(project, &options.items);
    nest(project, &entries, Path::new(""), options)
}

fn is_child_of(entry: &ProjectEntry, dir: &Path) -> bool {
    entry.relative.parent().unwrap_or(Path::new("")) == dir
}

fn nest(project: &Project, entries: &[ProjectEntry], dir: &Path, options: &TreeOptions) -> Vec<Node> {
    let in_dir: Vec<&ProjectEntry> = entries.iter().filter(|e| is_child_of(e, dir)).collect();
    let files: Vec<&ProjectEntry> = in_dir.iter().copied().filter(|e| !e.is_dir).collect();

    // Which file each file nests under: `DependentUpon` first, then name patterns.
    let mut parent_of: HashMap<PathBuf, PathBuf> = HashMap::new();
    for file in &files {
        if let Some(dependent) = &file.dependent_upon
            && files.iter().any(|f| paths::to_forward(&f.relative).eq_ignore_ascii_case(&paths::to_forward(dependent))) && *dependent != file.relative {
                parent_of.insert(file.relative.clone(), dependent.clone());
                continue;
            }
        if options.nest_by_name
            && let Some(parent) = name_parent(file, &files) {
                parent_of.insert(file.relative.clone(), parent);
            }
    }

    fn file_node(project: &Project, entry: &ProjectEntry, files: &[&ProjectEntry], parent_of: &HashMap<PathBuf, PathBuf>, depth: usize) -> Node {
        // The depth limit also stops loops between files that nest under each other.
        let children = if depth < 4 {
            files
                .iter()
                .filter(|f| parent_of.get(&f.relative) == Some(&entry.relative))
                .map(|f| file_node(project, f, files, parent_of, depth + 1))
                .collect()
        } else {
            Vec::new()
        };
        Node {
            id: format!("file:{}:{}", project.path.display(), entry.relative.display()),
            kind: NodeKind::File { project: project.path.clone(), path: entry.full.clone(), is_link: entry.is_link, missing: entry.missing },
            label: paths::file_name(&entry.relative),
            detail: if entry.missing { Some("(missing)".into()) } else if entry.is_link { Some("link".into()) } else { None },
            children,
            expanded_by_default: false,
        }
    }

    let mut folders: Vec<Node> = Vec::new();
    let mut top_files: Vec<Node> = Vec::new();
    let mut ordered: Vec<Node> = Vec::new();
    for entry in &in_dir {
        if entry.is_dir {
            let node = Node {
                id: format!("dir:{}:{}", project.path.display(), entry.relative.display()),
                kind: NodeKind::Folder { project: project.path.clone(), path: entry.full.clone(), is_link: entry.is_link },
                label: paths::file_name(&entry.relative),
                detail: None,
                children: nest(project, entries, &entry.relative, options),
                expanded_by_default: false,
            };
            if project.is_fsharp() { ordered.push(node) } else { folders.push(node) }
        } else if !parent_of.contains_key(&entry.relative) {
            let node = file_node(project, entry, &files, &parent_of, 0);
            if project.is_fsharp() { ordered.push(node) } else { top_files.push(node) }
        }
    }
    if project.is_fsharp() {
        return ordered;
    }
    let rank = |label: &str| match label.to_lowercase().as_str() {
        "properties" => 0,
        "wwwroot" => 1,
        _ => 2,
    };
    folders.sort_by(|a, b| rank(&a.label).cmp(&rank(&b.label)).then_with(|| natural_cmp(&a.label, &b.label)));
    top_files.sort_by(|a, b| natural_cmp(&a.label, &b.label));
    folders.extend(top_files);
    folders
}

/// `appsettings.Development.json` → `appsettings.json`; `Index.cshtml.cs` → `Index.cshtml`.
fn name_parent(file: &ProjectEntry, siblings: &[&ProjectEntry]) -> Option<PathBuf> {
    let name = paths::file_name(&file.relative);
    let find = |candidate: &str| siblings.iter().find(|s| paths::file_name(&s.relative).eq_ignore_ascii_case(candidate)).map(|s| s.relative.clone());
    // `X.ext.more` under `X.ext`.
    if let Some((stem, _)) = name.rsplit_once('.')
        && stem.contains('.')
            && let Some(parent) = find(stem) {
                return Some(parent);
            }
    // `Foo.Bar.ext` under `Foo.ext`.
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() >= 3 {
        let candidate = format!("{}.{}", parts[0], parts[parts.len() - 1]);
        if !candidate.eq_ignore_ascii_case(&name) {
            return find(&candidate);
        }
    }
    None
}

/// Case-insensitive ordering with numbers compared by value (`File2` before `File10`).
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, _) => return std::cmp::Ordering::Less,
            (_, None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = a.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    a.next();
                }
                let mut nb = String::new();
                while let Some(c) = b.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    b.next();
                }
                let ordering = na.len().cmp(&nb.len()).then(na.cmp(&nb));
                if ordering != std::cmp::Ordering::Equal {
                    return ordering;
                }
            }
            (Some(x), Some(y)) => {
                let ordering = x.to_lowercase().cmp(y.to_lowercase());
                if ordering != std::cmp::Ordering::Equal {
                    return ordering;
                }
                a.next();
                b.next();
            }
        }
    }
}

/// Finds the path of node ids from the root to the node for `file`, to reveal it.
pub fn path_to_file(root: &Node, file: &Path) -> Option<Vec<String>> {
    if root.kind.path() == Some(file) && matches!(root.kind, NodeKind::File { .. } | NodeKind::SolutionItem { .. } | NodeKind::Project { .. }) {
        return Some(vec![root.id.clone()]);
    }
    for child in &root.children {
        if matches!(child.kind, NodeKind::Dependencies { .. }) {
            continue;
        }
        if let Some(mut path) = path_to_file(child, file) {
            path.insert(0, root.id.clone());
            return Some(path);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msbuild::{EvalOptions, evaluate};

    fn touch(root: &Path, name: &str) {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    fn labels(nodes: &[Node]) -> Vec<String> {
        nodes.iter().map(|n| n.label.clone()).collect()
    }

    #[test]
    fn orders_and_nests_like_visual_studio() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for file in ["Program.cs", "appsettings.json", "appsettings.Development.json", "Pages/Index.cshtml", "Pages/Index.cshtml.cs", "wwwroot/site.css", "Properties/launchSettings.json", "Models/File10.cs", "Models/File2.cs", "Form.cs", "Form.Designer.cs"] {
            touch(&root, file);
        }
        std::fs::write(root.join("Web.csproj"), r#"<Project Sdk="Microsoft.NET.Sdk.Web"><ItemGroup><Compile Update="Form.Designer.cs"><DependentUpon>Form.cs</DependentUpon></Compile><PackageReference Include="Serilog" Version="3.1.1" /></ItemGroup></Project>"#).unwrap();
        let project = evaluate(&root.join("Web.csproj"), &EvalOptions::default()).unwrap();
        let nodes = file_nodes(&project, &TreeOptions::default());
        assert_eq!(labels(&nodes), vec!["Properties", "wwwroot", "Models", "Pages", "appsettings.json", "Form.cs", "Program.cs"]);
        assert_eq!(labels(&nodes[2].children), vec!["File2.cs", "File10.cs"]);
        assert_eq!(labels(&nodes[3].children), vec!["Index.cshtml"]);
        assert_eq!(labels(&nodes[3].children[0].children), vec!["Index.cshtml.cs"]);
        assert_eq!(labels(&nodes[4].children), vec!["appsettings.Development.json"]);
        assert_eq!(labels(&nodes[5].children), vec!["Form.Designer.cs"]);

        let solution = Solution::from_folder(&root, vec![root.join("Web.csproj")]);
        let mut loaded = LoadedProjects::new();
        loaded.insert(root.join("Web.csproj"), Ok(project));
        let tree = build(&solution, &loaded, &TreeOptions::default());
        let project_node = &tree.children[0];
        assert_eq!(project_node.children[0].label, "Dependencies");
        let packages = project_node.children[0].children.iter().find(|n| n.label == "Packages").unwrap();
        assert_eq!(packages.children[0].detail.as_deref(), Some("3.1.1"));
        let path = path_to_file(&tree, &root.join("Pages/Index.cshtml.cs")).unwrap();
        assert_eq!(path.len(), 5);
    }
}

#[cfg(test)]
mod timing {
    use super::*;

    /// `FORGE_DOTNET_BIG_SOLUTION=/path/Big.sln cargo test -p dotnet-model --release timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn big_solution() {
        let Ok(path) = std::env::var("FORGE_DOTNET_BIG_SOLUTION") else { return };
        let start = std::time::Instant::now();
        let solution = Solution::load(Path::new(&path)).unwrap();
        let mut projects = LoadedProjects::new();
        for p in solution.all_projects() {
            projects.insert(p.path.clone(), crate::evaluate(&p.path, &Default::default()).map_err(|e| e.to_string()));
        }
        let evaluated = start.elapsed();
        let tree = build(&solution, &projects, &TreeOptions::default());
        let built = start.elapsed();
        let cloned = tree.clone();
        println!("evaluate {evaluated:?}, tree {:?}, clone {:?}, nodes {}", built - evaluated, start.elapsed() - built, count(&cloned));
    }

    fn count(node: &Node) -> usize {
        1 + node.children.iter().map(count).sum::<usize>()
    }
}
