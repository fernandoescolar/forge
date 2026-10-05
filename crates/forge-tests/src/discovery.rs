//! Finds test projects under a folder and the tests in them, from source alone: .NET
//! test projects here, Go modules, Cargo packages, Jest/Vitest packages and pytest
//! projects in their modules (`go`, `rust`, `node`, `python`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use forge_languages::csharp_discovery::csharp_tests;

#[derive(Clone, Debug, PartialEq)]
pub struct TestProject {
    pub name: String,
    /// The manifest: `.csproj`, `go.mod`, `Cargo.toml`, `package.json` or a Python project's
    /// `pyproject.toml` (or other marker, `python::MARKERS`).
    pub path: PathBuf,
    pub kind: Kind,
    /// Classes, packages, modules or test files, depending on the kind.
    pub classes: Vec<TestClass>,
}

impl TestProject {
    /// The folder the tests run in.
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }
}

/// What runs a project's tests.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Dotnet,
    Go,
    Rust { package: String },
    Node(NodeRunner),
    Python,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRunner {
    Jest,
    Vitest,
}

impl Kind {
    pub fn label(&self) -> &'static str {
        match self {
            Kind::Dotnet => ".NET",
            Kind::Go => "Go",
            Kind::Rust { .. } => "Rust",
            Kind::Node(NodeRunner::Jest) => "Jest",
            Kind::Node(NodeRunner::Vitest) => "Vitest",
            Kind::Python => "pytest",
        }
    }
}

/// `row` of a class with no line of its own (a Go package, a test file).
pub const NO_ROW: u32 = u32::MAX;

#[derive(Clone, Debug, PartialEq)]
pub struct TestClass {
    /// `Namespace.Class`.
    pub fqn: String,
    pub name: String,
    pub file: PathBuf,
    pub row: u32,
    pub tests: Vec<TestMethod>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TestMethod {
    /// `Namespace.Class.Method`.
    pub fqn: String,
    pub name: String,
    pub file: PathBuf,
    pub row: u32,
}

pub(crate) const SKIPPED_DIRS: &[&str] = &["bin", "obj", ".git", ".vs", ".idea", "node_modules", "target", "TestResults", "vendor", "dist", "build", "coverage", "venv", "__pycache__", "site-packages"];

/// Test projects under `root` (every kind), sorted by name; projects without tests are
/// left out.
pub fn discover(root: &Path) -> Vec<TestProject> {
    let mut projects: Vec<_> = find_files(root, "csproj", false)
        .into_iter()
        .filter(|project| is_test_project(project))
        .map(|project| load_project(&project))
        .collect();
    projects.extend(crate::go::discover(root));
    projects.extend(crate::rust::discover(root));
    projects.extend(crate::node::discover(root));
    projects.extend(crate::python::discover(root));
    projects.retain(|p| p.kind == Kind::Dotnet || p.classes.iter().any(|c| !c.tests.is_empty()));
    projects.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    projects
}

/// Files under `dir` for which `wanted` holds. Folders holding a file named `stop_at`
/// (another project's manifest) are skipped, except `dir` itself.
pub(crate) fn find_where(dir: &Path, wanted: impl Fn(&Path) -> bool, stop_at: Option<&str>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for path in entries.flatten().map(|e| e.path()) {
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                let other_project = stop_at.is_some_and(|marker| path.join(marker).exists());
                if !SKIPPED_DIRS.contains(&name) && !name.starts_with('.') && !other_project {
                    pending.push(path);
                }
            } else if wanted(&path) {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

pub fn is_test_project(project: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(project) else {
        return false;
    };
    let text = text.to_ascii_lowercase();
    ["microsoft.net.test.sdk", "<istestproject>true", "\"xunit", "\"nunit", "\"mstest"]
        .iter()
        .any(|marker| text.contains(marker))
}

fn load_project(project: &Path) -> TestProject {
    let dir = project.parent().unwrap_or(Path::new("."));
    // Partial classes can be spread over several files; key by name to merge them.
    let mut classes: BTreeMap<String, TestClass> = BTreeMap::new();
    for file in find_files(dir, "cs", true) {
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        for test in csharp_tests(&source) {
            let class = classes.entry(test.class_fqn()).or_insert_with(|| TestClass {
                fqn: test.class_fqn(),
                name: test.class.clone(),
                file: file.clone(),
                row: test.class_row,
                tests: Vec::new(),
            });
            class.tests.push(TestMethod {
                fqn: test.fqn(),
                name: test.method.clone(),
                file: file.clone(),
                row: test.row,
            });
        }
    }
    TestProject {
        name: project.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        path: project.to_path_buf(),
        kind: Kind::Dotnet,
        classes: classes.into_values().collect(),
    }
}

/// Files with `extension` under `dir`. With `stop_at_projects`, subfolders that hold their
/// own `.csproj` belong to that project and are skipped.
fn find_files(dir: &Path, extension: &str, stop_at_projects: bool) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        for path in entries {
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                let is_other_project = stop_at_projects && has_project(&path);
                if !SKIPPED_DIRS.contains(&name) && !name.starts_with('.') && !is_other_project {
                    pending.push(path);
                }
            } else if path.extension().is_some_and(|ext| ext == extension) {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn has_project(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().any(|e| e.path().extension().is_some_and(|ext| ext == "csproj")))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_test_projects_and_their_tests() {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("src/App/App.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\" />");
        write("src/App/Math.cs", "namespace App; public class Math {}");
        write(
            "tests/App.Tests/App.Tests.csproj",
            "<Project><ItemGroup><PackageReference Include=\"Microsoft.NET.Test.Sdk\" /></ItemGroup></Project>",
        );
        write(
            "tests/App.Tests/MathTests.cs",
            "namespace App.Tests;\npublic class MathTests\n{\n    [Fact]\n    public void Adds() {}\n    [Fact]\n    public void Subtracts() {}\n}\n",
        );
        write("tests/App.Tests/bin/Debug/Generated.cs", "public class G { [Fact] public void X() {} }");

        let projects = discover(root.path());
        assert_eq!(projects.len(), 1, "only the project referencing the test SDK");
        let project = &projects[0];
        assert_eq!(project.name, "App.Tests");
        assert_eq!(project.classes.len(), 1, "bin/ is skipped");
        let class = &project.classes[0];
        assert_eq!(class.fqn, "App.Tests.MathTests");
        assert_eq!(class.row, 1);
        let tests: Vec<_> = class.tests.iter().map(|t| (t.fqn.as_str(), t.row)).collect();
        assert_eq!(tests, [("App.Tests.MathTests.Adds", 4), ("App.Tests.MathTests.Subtracts", 6)]);
    }

    /// Prints what discovery finds in a real tree:
    /// `FORGE_TESTS_DISCOVER_ROOT=/path cargo test -p forge-tests -- --ignored --nocapture discovers_a_real_tree`
    #[test]
    #[ignore]
    fn discovers_a_real_tree() {
        let Ok(root) = std::env::var("FORGE_TESTS_DISCOVER_ROOT") else {
            return;
        };
        for project in discover(Path::new(&root)) {
            let tests: usize = project.classes.iter().map(|c| c.tests.len()).sum();
            println!("{}: {} classes, {} tests", project.name, project.classes.len(), tests);
            for test in project.classes.iter().flat_map(|c| &c.tests) {
                println!("FQN {}", test.fqn);
            }
        }
    }
}

