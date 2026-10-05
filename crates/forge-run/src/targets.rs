//! What can be run in a workspace: .NET apps and test projects, Rust binaries, Go `main`
//! packages, `package.json` scripts and Python programs, found from their manifests and
//! sources without building anything.

use std::path::{Path, PathBuf};

use task::TaskTemplate;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    DotnetApp,
    DotnetTests,
    Cargo,
    Go,
    /// A `package.json` script.
    Node,
    /// A Python module run with `python -m`: a package's `__main__.py`, or a top-level file
    /// with an `if __name__ == "__main__":` block.
    Python,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::DotnetApp => ".NET",
            Kind::DotnetTests => "Tests",
            Kind::Cargo => "Rust",
            Kind::Go => "Go",
            Kind::Node => "Node",
            Kind::Python => "Python",
        }
    }

    /// The debug adapter Forge uses for this kind of target.
    pub fn debug_adapter(self) -> &'static str {
        match self {
            Kind::DotnetApp | Kind::DotnetTests => "netcoredbg",
            Kind::Cargo => "CodeLLDB",
            Kind::Go => "Delve",
            Kind::Node => "JavaScript",
            Kind::Python => "Debugpy",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunTarget {
    pub kind: Kind,
    pub name: String,
    /// `.csproj`, `Cargo.toml`, the Go package folder, `package.json` or the Python file or
    /// package folder; with `entry`, the target's id.
    pub manifest: PathBuf,
    /// Where the commands run.
    pub dir: PathBuf,
    /// For multi-targeted .NET apps, the framework `dotnet run` needs to be told.
    pub framework: Option<String>,
    /// What runs: the package manager of a Node script, the interpreter of a Python module.
    pub program: Option<String>,
    /// The `package.json` script, the Python module and its arguments, or the .NET app's
    /// launch profile (when it has several).
    pub entry: Option<String>,
}

impl RunTarget {
    pub fn id(&self) -> String {
        let manifest = self.manifest.to_string_lossy();
        match &self.entry {
            Some(entry) if matches!(self.kind, Kind::Node | Kind::DotnetApp) => format!("{manifest}#{entry}"),
            _ => manifest.into_owned(),
        }
    }

    /// The command that runs the target (for tests, the one the debugger builds on).
    pub fn task(&self) -> TaskTemplate {
        let quoted = |path: &Path| format!("\"{}\"", path.display());
        let (command, args): (&str, Vec<String>) = match self.kind {
            Kind::DotnetApp => {
                let mut args = vec!["run".into(), "--project".into(), quoted(&self.manifest)];
                if let Some(framework) = &self.framework {
                    args.extend(["--framework".into(), framework.clone()]);
                }
                if let Some(profile) = &self.entry {
                    args.extend(["--launch-profile".into(), format!("\"{profile}\"")]);
                }
                ("dotnet", args)
            }
            Kind::DotnetTests => ("dotnet", vec!["test".into(), quoted(&self.manifest)]),
            Kind::Cargo => ("cargo", vec!["run".into(), "--package".into(), self.name.clone()]),
            Kind::Go => ("go", vec!["run".into(), ".".into()]),
            Kind::Node => (self.program.as_deref().unwrap_or("npm"), vec!["run".into(), self.entry.clone().unwrap_or_default()]),
            Kind::Python => {
                let mut args = vec!["-m".to_string()];
                args.extend(self.entry.iter().flat_map(|e| e.split_whitespace().map(String::from)));
                (self.program.as_deref().unwrap_or("python3"), args)
            }
        };
        TaskTemplate {
            label: format!("{} {}", self.kind.label(), self.name),
            command: command.into(),
            args,
            cwd: Some(self.dir.to_string_lossy().into_owned()),
            ..TaskTemplate::default()
        }
    }

    /// `dotnet watch`: runs a .NET app and applies code changes to it while it runs (hot
    /// reload), restarting it when they can't be applied. `None` for other targets.
    pub fn watch_task(&self) -> Option<TaskTemplate> {
        if self.kind != Kind::DotnetApp {
            return None;
        }
        let mut task = self.task();
        // `dotnet run --project X …` → `dotnet watch run --project X …`.
        task.args.insert(0, "watch".into());
        task.label = format!("Hot reload {}", self.name);
        task.env.insert("DOTNET_WATCH_RESTART_ON_RUDE_EDIT".into(), "true".into());
        Some(task)
    }
}

const SKIPPED_DIRS: &[&str] = &["bin", "obj", "target", "node_modules", "vendor", "TestResults", "dist", "build", "venv", "__pycache__", "site-packages"];

/// Run targets under `root`, grouped by kind (test projects last) and sorted by name.
pub fn discover(root: &Path) -> Vec<RunTarget> {
    let mut targets = Vec::new();
    walk(root, 0, &mut |dir, files| {
        for file in files {
            match file.extension().and_then(|e| e.to_str()) {
                Some("csproj") => targets.extend(dotnet_targets(file)),
                _ if file.file_name().is_some_and(|n| n == "Cargo.toml") => targets.extend(cargo_target(dir, file)),
                _ if file.file_name().is_some_and(|n| n == "package.json") => targets.extend(node_targets(root, dir, file)),
                _ => {}
            }
        }
        if files.iter().any(|f| f.extension().is_some_and(|e| e == "go")) && is_go_main(files) {
            targets.push(RunTarget {
                kind: Kind::Go,
                name: dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| ".".into()),
                manifest: dir.to_path_buf(),
                dir: dir.to_path_buf(),
                framework: None,
                program: None,
                entry: None,
            });
        }
        if dir == root || forge_tests::python::manifest_in(dir).is_some() {
            targets.extend(python_targets(root, dir, files));
        }
    });
    let order = |kind: Kind| match kind {
        Kind::DotnetApp => 0,
        Kind::Cargo => 1,
        Kind::Go => 2,
        Kind::Node => 3,
        Kind::Python => 4,
        Kind::DotnetTests => 5,
    };
    targets.sort_by(|a, b| (order(a.kind), &a.name).cmp(&(order(b.kind), &b.name)));
    targets
}

fn walk(dir: &Path, depth: usize, visit: &mut dyn FnMut(&Path, &[PathBuf])) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let (mut dirs, mut files): (Vec<_>, Vec<_>) = entries.flatten().map(|e| e.path()).partition(|p| p.is_dir());
    files.sort();
    visit(dir, &files);
    dirs.sort();
    for sub in dirs {
        let name = sub.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if !name.starts_with('.') && !SKIPPED_DIRS.contains(&name) {
            walk(&sub, depth + 1, visit);
        }
    }
}

/// A project's target, or one per launch profile when it has several.
fn dotnet_targets(project: &Path) -> Vec<RunTarget> {
    let Some(target) = dotnet_target(project) else { return vec![] };
    let profiles = if target.kind == Kind::DotnetApp { forge_languages::launch_settings::profiles(&target.dir) } else { vec![] };
    if profiles.len() < 2 {
        return vec![target];
    }
    profiles
        .into_iter()
        .map(|profile| RunTarget { name: format!("{} · {}", target.name, profile.name), entry: Some(profile.name), ..target.clone() })
        .collect()
}

fn dotnet_target(project: &Path) -> Option<RunTarget> {
    let text = std::fs::read_to_string(project).ok()?;
    let lower = text.to_ascii_lowercase();
    let kind = if forge_tests::discovery::is_test_project(project) {
        Kind::DotnetTests
    } else if lower.contains("<outputtype>exe") || lower.contains("<outputtype>winexe") || ["microsoft.net.sdk.web", "microsoft.net.sdk.worker", "microsoft.net.sdk.blazorwebassembly"].iter().any(|sdk| lower.contains(sdk)) {
        Kind::DotnetApp
    } else {
        return None; // a library
    };
    // With several target frameworks, run the last one listed (usually the newest).
    let framework = text
        .split("<TargetFrameworks>")
        .nth(1)
        .and_then(|rest| rest.split("</TargetFrameworks>").next())
        .and_then(|list| list.split(';').map(str::trim).filter(|f| !f.is_empty() && !f.contains('$')).last())
        .map(String::from);
    Some(RunTarget {
        kind,
        name: project.file_stem()?.to_string_lossy().into_owned(),
        manifest: project.to_path_buf(),
        dir: project.parent()?.to_path_buf(),
        framework: framework.filter(|_| kind == Kind::DotnetApp),
        program: None,
        entry: None,
    })
}

fn cargo_target(dir: &Path, manifest: &Path) -> Option<RunTarget> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let toml: toml::Value = toml::from_str(&text).ok()?;
    let name = toml.get("package")?.get("name")?.as_str()?.to_string();
    let has_bin = dir.join("src/main.rs").exists() || toml.get("bin").is_some_and(|bins| bins.as_array().is_some_and(|b| !b.is_empty()));
    has_bin.then(|| RunTarget { kind: Kind::Cargo, name, manifest: manifest.to_path_buf(), dir: dir.to_path_buf(), framework: None, program: None, entry: None })
}

/// Scripts that npm runs around others, or on install: not something to pick and run.
const LIFECYCLE_SCRIPTS: &[&str] = &["install", "preinstall", "postinstall", "prepare", "prepublish", "prepublishOnly", "prepack", "postpack"];

/// A target per script of `manifest`, run with the package manager the project uses. Names
/// say which package a script belongs to, except at the workspace's root.
fn node_targets(root: &Path, dir: &Path, manifest: &Path) -> Vec<RunTarget> {
    let Some(json) = std::fs::read_to_string(manifest).ok().and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok()) else {
        return vec![];
    };
    let Some(scripts) = json.get("scripts").and_then(|s| s.as_object()) else { return vec![] };
    let program = package_manager(root, dir, &json);
    let package = json.get("name").and_then(|n| n.as_str()).map(String::from).or_else(|| Some(dir.file_name()?.to_string_lossy().into_owned()));
    scripts
        .keys()
        .filter(|script| {
            let hook = ["pre", "post"].iter().any(|p| script.strip_prefix(p).is_some_and(|rest| scripts.contains_key(rest)));
            !hook && !LIFECYCLE_SCRIPTS.contains(&script.as_str())
        })
        .map(|script| RunTarget {
            kind: Kind::Node,
            name: match &package {
                Some(package) if dir != root => format!("{package} › {script}"),
                _ => script.clone(),
            },
            manifest: manifest.to_path_buf(),
            dir: dir.to_path_buf(),
            framework: None,
            program: Some(program.clone()),
            entry: Some(script.clone()),
        })
        .collect()
}

/// `packageManager` from the manifest, or the one whose lockfile is nearest (up to `root`).
fn package_manager(root: &Path, dir: &Path, json: &serde_json::Value) -> String {
    if let Some(declared) = json.get("packageManager").and_then(|p| p.as_str()).and_then(|p| p.split('@').next()).filter(|p| !p.is_empty()) {
        return declared.to_string();
    }
    for ancestor in dir.ancestors() {
        for (lockfile, manager) in [("pnpm-lock.yaml", "pnpm"), ("yarn.lock", "yarn"), ("bun.lock", "bun"), ("bun.lockb", "bun"), ("package-lock.json", "npm")] {
            if ancestor.join(lockfile).exists() {
                return manager.into();
            }
        }
        if ancestor == root {
            break;
        }
    }
    "npm".into()
}

/// Python programs in a project folder: its top-level files with a `__main__` block, and
/// packages with a `__main__.py` (beside it, or under `src/`). Django's `manage.py` runs
/// the development server.
fn python_targets(root: &Path, dir: &Path, files: &[PathBuf]) -> Vec<RunTarget> {
    let program = forge_tests::python::interpreter(dir, Some(root));
    let target = |name: String, manifest: PathBuf, dir: &Path, entry: String| RunTarget {
        kind: Kind::Python,
        name,
        manifest,
        dir: dir.to_path_buf(),
        framework: None,
        program: Some(program.clone()),
        entry: Some(entry),
    };
    let mut targets = Vec::new();
    for file in files {
        let Some(stem) = file.file_stem().and_then(|s| s.to_str()) else { continue };
        let is_module = file.extension().is_some_and(|e| e == "py") && !forge_tests::python::is_test_file(file) && !stem.contains(['.', '-']);
        if !is_module || !std::fs::read_to_string(file).is_ok_and(|text| has_main_block(&text)) {
            continue;
        }
        let (name, entry) = if stem == "manage" { ("manage.py runserver".to_string(), "manage runserver".to_string()) } else { (format!("{stem}.py"), stem.to_string()) };
        targets.push(target(name, file.clone(), dir, entry));
    }
    for parent in [dir.to_path_buf(), dir.join("src")] {
        let Ok(entries) = std::fs::read_dir(&parent) else { continue };
        let mut packages: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.join("__main__.py").is_file()).collect();
        packages.sort();
        for package in packages {
            let Some(name) = package.file_name().and_then(|n| n.to_str()).map(String::from) else { continue };
            targets.push(target(name.clone(), package.clone(), &parent, name));
        }
    }
    targets
}

fn has_main_block(source: &str) -> bool {
    source.lines().any(|line| {
        let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        compact.starts_with("if__name__==") && compact.contains("__main__")
    })
}

fn is_go_main(files: &[PathBuf]) -> bool {
    files.iter().filter(|f| f.extension().is_some_and(|e| e == "go") && !f.to_string_lossy().ends_with("_test.go")).any(|f| {
        std::fs::read_to_string(f).is_ok_and(|text| text.lines().any(|l| l.trim() == "package main") && text.contains("func main("))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_apps_tests_crates_and_go_mains() {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("src/Api/Api.csproj", "<Project Sdk=\"Microsoft.NET.Sdk.Web\"></Project>");
        write("src/Cli/Cli.csproj", "<Project><PropertyGroup><OutputType>Exe</OutputType><TargetFrameworks>net8.0;net10.0</TargetFrameworks></PropertyGroup></Project>");
        write("src/Lib/Lib.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\"></Project>");
        write("tests/Lib.Tests/Lib.Tests.csproj", "<Project><ItemGroup><PackageReference Include=\"Microsoft.NET.Test.Sdk\" /></ItemGroup></Project>");
        write("tool/Cargo.toml", "[package]\nname = \"tool\"\n");
        write("tool/src/main.rs", "fn main() {}");
        write("lib/Cargo.toml", "[package]\nname = \"lib\"\n");
        write("lib/src/lib.rs", "");
        write("cmd/server/main.go", "package main\n\nfunc main() {}\n");
        write("pkg/util/util.go", "package util\n");
        write("src/Api/Properties/launchSettings.json", r#"{"profiles": {"http": {"commandName": "Project"}, "https": {"commandName": "Project"}, "IIS Express": {"commandName": "IISExpress"}}}"#);
        write("src/Cli/Properties/launchSettings.json", r#"{"profiles": {"Cli": {"commandName": "Project"}}}"#);
        write("src/Api/bin/Debug/Copy.csproj", "<Project Sdk=\"Microsoft.NET.Sdk.Web\"></Project>");
        write("package.json", r#"{"scripts": {"dev": "vite", "predev": "x", "postinstall": "y"}}"#);
        write("pnpm-lock.yaml", "");
        write("web/package.json", r#"{"name": "@acme/web", "packageManager": "yarn@4.1.0", "scripts": {"start": "next start"}}"#);
        write("svc/pyproject.toml", "[project]\nname = \"svc\"\n");
        write("svc/main.py", "def main():\n    pass\n\nif __name__ == '__main__':\n    main()\n");
        write("svc/util.py", "def helper():\n    pass\n");
        write("svc/src/svc/__main__.py", "print('hi')\n");
        write("svc/manage.py", "if __name__ == \"__main__\":\n    pass\n");
        write("loose/script.py", "if __name__ == '__main__':\n    pass\n");

        let targets = discover(root.path());
        let cli = targets.iter().find(|t| t.name == "Cli").unwrap();
        assert_eq!(cli.task().args[3..], ["--framework".to_string(), "net10.0".to_string()]);
        let command = |name: &str| targets.iter().find(|t| t.name == name).map(|t| (t.task().command, t.task().args)).unwrap();
        let https = targets.iter().find(|t| t.name == "Api · https").unwrap();
        assert_eq!(https.task().args[3..], ["--launch-profile".to_string(), "\"https\"".to_string()]);
        assert!(https.id().ends_with("Api.csproj#https"), "each profile is its own target");
        assert_eq!(command("dev"), ("pnpm".to_string(), vec!["run".to_string(), "dev".to_string()]));
        assert_eq!(command("@acme/web › start"), ("yarn".to_string(), vec!["run".to_string(), "start".to_string()]));
        assert_eq!(command("manage.py runserver"), ("python3".to_string(), vec!["-m".to_string(), "manage".to_string(), "runserver".to_string()]));
        let package = targets.iter().find(|t| t.name == "svc").unwrap();
        assert!(package.dir.ends_with("svc/src"), "runs from the folder that holds the package");
        let found: Vec<_> = targets.into_iter().map(|t| (t.kind, t.name)).collect();
        assert_eq!(
            found,
            [
                (Kind::DotnetApp, "Api · http".to_string()),
                (Kind::DotnetApp, "Api · https".to_string()),
                (Kind::DotnetApp, "Cli".to_string()),
                (Kind::Cargo, "tool".to_string()),
                (Kind::Go, "server".to_string()),
                (Kind::Node, "@acme/web › start".to_string()),
                (Kind::Node, "dev".to_string()),
                (Kind::Python, "main.py".to_string()),
                (Kind::Python, "manage.py runserver".to_string()),
                (Kind::Python, "svc".to_string()),
                (Kind::DotnetTests, "Lib.Tests".to_string()),
            ]
        );
    }

    #[test]
    fn watches_dotnet_apps() {
        let app = RunTarget { kind: Kind::DotnetApp, name: "Api".into(), manifest: "/p/Api/Api.csproj".into(), dir: "/p/Api".into(), framework: None, program: None, entry: Some("https".into()) };
        let watch = app.watch_task().unwrap();
        assert_eq!(watch.args, ["watch", "run", "--project", "\"/p/Api/Api.csproj\"", "--launch-profile", "\"https\""]);
        assert_eq!(watch.env.get("DOTNET_WATCH_RESTART_ON_RUDE_EDIT").map(String::as_str), Some("true"));
        let tool = RunTarget { kind: Kind::Cargo, ..app };
        assert!(tool.watch_task().is_none());
    }

    #[test]
    fn builds_run_commands() {
        let target = RunTarget { kind: Kind::DotnetApp, name: "Api".into(), manifest: "/p/Api/Api.csproj".into(), dir: "/p/Api".into(), framework: None, program: None, entry: None };
        let task = target.task();
        assert_eq!((task.command.as_str(), task.args.clone()), ("dotnet", vec!["run".into(), "--project".into(), "\"/p/Api/Api.csproj\"".into()]));
        assert_eq!(task.cwd.as_deref(), Some("/p/Api"));
        assert_eq!(Kind::Go.debug_adapter(), "Delve");
        let script = RunTarget { kind: Kind::Node, name: "dev".into(), manifest: "/p/package.json".into(), dir: "/p".into(), framework: None, program: Some("npm".into()), entry: Some("dev".into()) };
        assert_eq!(script.id(), "/p/package.json#dev", "scripts of one package have their own ids");
    }

    /// `FORGE_RUN_DISCOVER_ROOT=/path cargo test -p forge-run -- --ignored --nocapture discovers_a_real_tree`
    #[test]
    #[ignore]
    fn discovers_a_real_tree() {
        let Ok(root) = std::env::var("FORGE_RUN_DISCOVER_ROOT") else {
            return;
        };
        for target in discover(Path::new(&root)) {
            println!("TARGET {:?} {} ({})", target.kind, target.name, target.dir.display());
        }
    }
}
