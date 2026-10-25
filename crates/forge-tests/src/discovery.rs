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
    // Every kind of project looks for its files: list the folders once for all of them.
    discover_with(Listing::new(root))
}

/// `discover`, from `files` (every file under `root` worth looking at: in the app, the
/// worktree's, which leaves out what `.gitignore` ignores) instead of the disk's folders.
pub fn discover_in(root: &Path, files: Vec<PathBuf>) -> Vec<TestProject> {
    discover_with(Listing::from_files(root, files))
}

fn discover_with(listing: Listing) -> Vec<TestProject> {
    let root = listing.root.clone();
    LISTING.with(|l| *l.borrow_mut() = Some(std::rc::Rc::new(listing)));
    let projects = discover_listed(&root);
    LISTING.with(|l| l.borrow_mut().take());
    projects
}

/// The files of the project's local worktrees, as Zed indexed them (gitignored ones left
/// out), by worktree root (`forge_ui::worktree::files`).
pub fn worktree_files(project: &gpui::Entity<project::Project>, cx: &gpui::App) -> gpui::Task<Vec<(PathBuf, Vec<PathBuf>)>> {
    forge_ui::worktree::files(project, cx)
}

/// What `parse` makes of `file`'s text, remembered while the file stays the same (same
/// modification time and size): discovery runs after every save, and reading and parsing
/// every test file again was most of its work. `kind` keeps apart what different parsers
/// make of one file.
pub fn parsed<T: Clone + Send + Sync + 'static>(file: &Path, kind: &'static str, parse: impl FnOnce(&str) -> T) -> Option<T> {
    use std::any::Any;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    type Cache = HashMap<(&'static str, PathBuf), (std::time::SystemTime, u64, Arc<dyn Any + Send + Sync>)>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    // Files come and go; past this many, start afresh rather than keep the departed.
    const MAX_FILES: usize = 50_000;
    let cache = CACHE.get_or_init(Default::default);
    let metadata = std::fs::metadata(file).ok()?;
    let stamp = (metadata.modified().ok()?, metadata.len());
    let key = (kind, file.to_path_buf());
    if let Some((modified, len, value)) = cache.lock().ok()?.get(&key) {
        if (*modified, *len) == stamp {
            if let Some(value) = value.downcast_ref::<T>() {
                return Some(value.clone());
            }
        }
    }
    let value = parse(&std::fs::read_to_string(file).ok()?);
    let mut cache = cache.lock().ok()?;
    if cache.len() >= MAX_FILES {
        cache.clear();
    }
    cache.insert(key, (stamp.0, stamp.1, Arc::new(value.clone())));
    Some(value)
}

/// The files under a root, listed once by `discover` (skipping `SKIPPED_DIRS` and hidden
/// folders, as the walks below do).
struct Listing {
    root: PathBuf,
    files: Vec<PathBuf>,
    all: std::collections::HashSet<PathBuf>,
    /// Folders that hold a `.csproj`.
    projects: std::collections::HashSet<PathBuf>,
}

thread_local! {
    static LISTING: std::cell::RefCell<Option<std::rc::Rc<Listing>>> = const { std::cell::RefCell::new(None) };
}

impl Listing {
    /// From files already listed, keeping out the folders the walk below skips (for
    /// projects without a `.gitignore` that would).
    fn from_files(root: &Path, mut files: Vec<PathBuf>) -> Self {
        files.retain(|file| {
            file.strip_prefix(root).is_ok_and(|relative| {
                relative.parent().into_iter().flat_map(Path::components).all(|c| {
                    let name = c.as_os_str().to_string_lossy();
                    !SKIPPED_DIRS.contains(&name.as_ref()) && !name.starts_with('.')
                })
            })
        });
        files.sort();
        Self::indexed(root, files)
    }

    fn indexed(root: &Path, files: Vec<PathBuf>) -> Self {
        let projects = files.iter().filter(|f| f.extension().is_some_and(|e| e == "csproj")).filter_map(|f| f.parent().map(Path::to_path_buf)).collect();
        let all = files.iter().cloned().collect();
        Self { root: root.to_path_buf(), files, all, projects }
    }

    fn new(root: &Path) -> Self {
        let mut files = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(current) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&current) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if entry.file_type().is_ok_and(|t| t.is_dir()) || (entry.file_type().is_ok_and(|t| t.is_symlink()) && path.is_dir()) {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                    if !SKIPPED_DIRS.contains(&name) && !name.starts_with('.') {
                        pending.push(path);
                    }
                } else {
                    files.push(path);
                }
            }
        }
        files.sort();
        Self::indexed(root, files)
    }

    /// `find_where` over the listing: files under `dir` for which `wanted` holds, leaving
    /// out folders below `dir` that hold a file named `stop_at`.
    fn find(&self, dir: &Path, wanted: impl Fn(&Path) -> bool, blocked: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
        self.files
            .iter()
            .filter(|f| f.starts_with(dir) && wanted(f))
            .filter(|f| !f.parent().into_iter().flat_map(Path::ancestors).take_while(|a| *a != dir && a.starts_with(dir)).any(&blocked))
            .cloned()
            .collect()
    }
}

/// The listing `discover` made, when `dir` is in it.
fn listing_for(dir: &Path) -> Option<std::rc::Rc<Listing>> {
    LISTING.with(|l| l.borrow().clone()).filter(|l| dir.starts_with(&l.root))
}

fn discover_listed(root: &Path) -> Vec<TestProject> {
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
    if let Some(listing) = listing_for(dir) {
        return listing.find(dir, wanted, |folder| stop_at.is_some_and(|marker| listing.all.contains(&folder.join(marker))));
    }
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
        let Some(tests) = parsed(&file, "csharp-tests", csharp_tests) else {
            continue;
        };
        for test in tests {
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
    if let Some(listing) = listing_for(dir) {
        return listing.find(dir, |f| f.extension().is_some_and(|e| e == extension), |folder| stop_at_projects && listing.projects.contains(folder));
    }
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

    /// One listing for every kind finds what each kind's own walk found, nested projects
    /// and skipped folders included.
    #[test]
    fn the_shared_listing_finds_what_walking_finds() {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("Outer.Tests/Outer.Tests.csproj", "<Project><ItemGroup><PackageReference Include=\"xunit\" /></ItemGroup></Project>");
        write("Outer.Tests/A.cs", "namespace O; public class A { [Fact] public void T() {} }");
        write("Outer.Tests/Inner/Inner.Tests.csproj", "<Project><ItemGroup><PackageReference Include=\"xunit\" /></ItemGroup></Project>");
        write("Outer.Tests/Inner/B.cs", "namespace I; public class B { [Fact] public void T() {} }");
        write("go/go.mod", "module example.com/m\n");
        write("go/a_test.go", "package m\nimport \"testing\"\nfunc TestA(t *testing.T) {}\n");
        write("go/sub/go.mod", "module example.com/sub\n");
        write("go/sub/b_test.go", "package sub\nimport \"testing\"\nfunc TestB(t *testing.T) {}\n");
        write("web/package.json", "{\"devDependencies\":{\"vitest\":\"1\"}}");
        write("web/src/a.test.ts", "test('works', () => {});");
        write("web/node_modules/x/b.test.ts", "test('no', () => {});");
        write("py/pyproject.toml", "[tool.pytest.ini_options]\n");
        write("py/test_a.py", "def test_a():\n    pass\n");
        let listed = discover(root.path());
        let walked = discover_listed(root.path());
        assert_eq!(listed, walked);
        assert!(listed.len() >= 5, "{listed:#?}");

        // From a list of the files (as the worktree gives them), the same.
        let mut files = Vec::new();
        let mut pending = vec![root.path().to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                if entry.path().is_dir() { pending.push(entry.path()) } else { files.push(entry.path()) }
            }
        }
        assert_eq!(discover_in(root.path(), files), listed);
    }

    /// A test file is parsed again only once it changed.
    #[test]
    fn parses_files_again_only_when_they_change() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a_test.go");
        std::fs::write(&file, "func TestA(t *testing.T) {}").unwrap();
        let parses = std::cell::Cell::new(0);
        let count = |text: &str| {
            parses.set(parses.get() + 1);
            text.len()
        };
        assert_eq!(parsed(&file, "test-count", count), Some(27));
        assert_eq!(parsed(&file, "test-count", count), Some(27));
        assert_eq!(parses.get(), 1, "the second time comes from the cache");
        std::fs::write(&file, "func TestA(t *testing.T) {}\nfunc TestB(t *testing.T) {}").unwrap();
        assert_eq!(parsed(&file, "test-count", count), Some(55));
        assert_eq!(parses.get(), 2, "a changed file is parsed again");
        assert_eq!(parsed(&dir.path().join("gone.go"), "test-count", count), None);
    }

    /// The worktree's files leave out what `.gitignore` ignores.
    #[gpui::test]
    async fn worktree_files_leave_out_ignored_ones(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            serde_json::json!({ ".git": {}, ".gitignore": "generated/\n", "src": { "a_test.go": "" }, "generated": { "b_test.go": "" } }),
        )
        .await;
        let project = project::Project::test(fs, ["/repo".as_ref()], cx).await;
        let listed = cx.update(|cx| worktree_files(&project, cx)).await;
        let files: Vec<PathBuf> = listed.into_iter().flat_map(|(_, files)| files).collect();
        assert!(files.contains(&PathBuf::from("/repo/src/a_test.go")), "{files:?}");
        assert!(!files.iter().any(|f| f.starts_with("/repo/generated")), "{files:?}");
    }

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

