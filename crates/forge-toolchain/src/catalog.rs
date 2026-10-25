//! The tools Forge's features run (git, .NET, Rust, Go, Node and its package managers, Bun,
//! Python, make, gh, Docker), which a project needs (from its files), whether they are
//! installed, and how to install them on this machine. No UI here.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tool {
    Git,
    Dotnet,
    Rust,
    Go,
    Node,
    Pnpm,
    Yarn,
    Bun,
    Python,
    Make,
    Gh,
    Docker,
}

impl Tool {
    pub const ALL: [Tool; 12] = [Tool::Git, Tool::Dotnet, Tool::Rust, Tool::Go, Tool::Node, Tool::Pnpm, Tool::Yarn, Tool::Bun, Tool::Python, Tool::Make, Tool::Gh, Tool::Docker];

    pub fn name(self) -> &'static str {
        match self {
            Tool::Git => "git",
            Tool::Dotnet => ".NET SDK",
            Tool::Rust => "Rust (cargo)",
            Tool::Go => "Go",
            Tool::Node => "Node.js (npm, npx)",
            Tool::Pnpm => "pnpm",
            Tool::Yarn => "Yarn",
            Tool::Bun => "Bun",
            Tool::Python => "Python",
            Tool::Make => "make",
            Tool::Gh => "GitHub CLI (gh)",
            Tool::Docker => "Docker",
        }
    }

    /// The programs it provides; it is installed when all of them are found.
    pub fn programs(self) -> &'static [&'static str] {
        match self {
            Tool::Git => &["git"],
            Tool::Dotnet => &["dotnet"],
            Tool::Rust => &["cargo", "rustc"],
            Tool::Go => &["go"],
            Tool::Node => &["node", "npm", "npx"],
            Tool::Pnpm => &["pnpm"],
            Tool::Yarn => &["yarn"],
            Tool::Bun => &["bun"],
            Tool::Python => &["python3"],
            Tool::Make => &["make"],
            Tool::Gh => &["gh"],
            Tool::Docker => &["docker"],
        }
    }

    /// Nice to have rather than needed: offered in Check Tools, not on opening a project.
    pub fn optional(self) -> bool {
        matches!(self, Tool::Gh | Tool::Docker)
    }
}

/// Why a project needs a tool: the file that says so (relative to the project's folder).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Need {
    pub tool: Tool,
    pub because: String,
}

/// Folders of test data, whose manifests say nothing about what the project needs.
const SAMPLES: &[&str] = &["fixtures", "test-fixtures", "__fixtures__", "testdata", "test-data"];

/// The tools a project needs, from its files (as the worktree lists them, under `root`).
/// Forge itself uses git (history, agents' worktrees, commits), so git is always needed.
pub fn needed(root: &Path, files: &[PathBuf]) -> Vec<Need> {
    let mut needs: Vec<Need> = vec![Need { tool: Tool::Git, because: "Forge's Git features and agents' worktrees".into() }];
    let mut add = |tool: Tool, file: &Path| {
        if !needs.iter().any(|n| n.tool == tool) {
            let shown = file.strip_prefix(root).unwrap_or(file).to_string_lossy().into_owned();
            needs.push(Need { tool, because: shown });
        }
    };
    for file in files {
        // A test's sample projects aren't what the project is built with.
        let relative = file.strip_prefix(root).unwrap_or(file);
        let sample = relative.parent().into_iter().flat_map(Path::components).any(|c| SAMPLES.contains(&c.as_os_str().to_string_lossy().as_ref()));
        if sample {
            continue;
        }
        let name = file.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let extension = file.extension().and_then(|e| e.to_str()).unwrap_or_default();
        if matches!(extension, "csproj" | "fsproj" | "vbproj" | "sln" | "slnx") || name == "global.json" {
            add(Tool::Dotnet, file);
        }
        match name {
            "Cargo.toml" => add(Tool::Rust, file),
            "go.mod" => add(Tool::Go, file),
            "package.json" => {
                add(Tool::Node, file);
                // `"packageManager": "pnpm@9.1.0"` (or yarn) picks the package manager.
                let declared = std::fs::read_to_string(file).ok().and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok()).and_then(|json| json.get("packageManager")?.as_str().map(str::to_string));
                match declared.as_deref().and_then(|d| d.split('@').next()) {
                    Some("pnpm") => add(Tool::Pnpm, file),
                    Some("yarn") => add(Tool::Yarn, file),
                    Some("bun") => add(Tool::Bun, file),
                    _ => {}
                }
            }
            "pnpm-lock.yaml" | "pnpm-workspace.yaml" => add(Tool::Pnpm, file),
            "yarn.lock" => add(Tool::Yarn, file),
            "bun.lock" | "bun.lockb" => add(Tool::Bun, file),
            "pyproject.toml" | "requirements.txt" | "setup.py" | "setup.cfg" | "Pipfile" => add(Tool::Python, file),
            "Makefile" | "makefile" | "GNUmakefile" => add(Tool::Make, file),
            "compose.yaml" | "compose.yml" | "docker-compose.yml" | "docker-compose.yaml" | "Dockerfile" => add(Tool::Docker, file),
            _ => {}
        }
    }
    if std::fs::read_to_string(root.join(".git/config")).is_ok_and(|config| config.contains("github.com")) {
        needs.push(Need { tool: Tool::Gh, because: "the repository is on GitHub (pull requests in Forge)".into() });
    }
    needs.sort_by_key(|n| n.tool);
    needs
}

/// Where a tool is: on the `PATH` (the project's shell environment), or only in a place
/// its installer puts it that the `PATH` doesn't have yet (until a new login).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    OnPath,
    OffPath(PathBuf),
    Missing,
}

/// Folders installers use that a `PATH` set before installing may lack.
pub fn usual_places(home: &Path) -> Vec<PathBuf> {
    ["~/.cargo/bin", "~/.dotnet", "~/go/bin", "~/.local/share/pnpm", "~/Library/pnpm", "~/.local/bin", "~/.yarn/bin", "~/.bun/bin"]
        .iter()
        .map(|p| home.join(p.trim_start_matches("~/")))
        .chain(["/usr/local/go/bin", "/usr/local/bin", "/opt/homebrew/bin", "/usr/local/share/dotnet"].map(PathBuf::from))
        .collect()
}

/// Whether `tool` is installed, looking on `path` (a `PATH` value) and in `places`.
pub fn find(tool: Tool, path: Option<&std::ffi::OsStr>, places: &[PathBuf]) -> Found {
    let mut off_path = None;
    for program in tool.programs() {
        let resolved = forge_process::resolve(program, path);
        if resolved.is_absolute() {
            continue;
        }
        match places.iter().map(|dir| dir.join(program)).find(|p| p.is_file()) {
            Some(found) => off_path = off_path.or(Some(found)),
            None => return Found::Missing,
        }
    }
    match off_path {
        Some(found) => Found::OffPath(found),
        None => Found::OnPath,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageManager {
    Brew,
    Pacman,
    Apt,
    Dnf,
    Zypper,
}

impl PackageManager {
    /// The one this machine uses, looked up on `path`: Homebrew on macOS, the
    /// distribution's own on Linux.
    pub fn detect(os: &str, path: Option<&std::ffi::OsStr>) -> Option<Self> {
        let has = |program: &str| forge_process::resolve(program, path).is_absolute();
        let candidates: &[(PackageManager, &str)] = if os == "macos" {
            &[(PackageManager::Brew, "brew")]
        } else {
            &[(PackageManager::Pacman, "pacman"), (PackageManager::Apt, "apt-get"), (PackageManager::Dnf, "dnf"), (PackageManager::Zypper, "zypper"), (PackageManager::Brew, "brew")]
        };
        candidates.iter().find(|(_, program)| has(program)).map(|(manager, _)| *manager)
    }

    fn install(self, packages: &[&str]) -> String {
        let packages = packages.join(" ");
        match self {
            PackageManager::Brew => format!("brew install {packages}"),
            PackageManager::Pacman => format!("sudo pacman -S --needed {packages}"),
            PackageManager::Apt => format!("sudo apt-get install -y {packages}"),
            PackageManager::Dnf => format!("sudo dnf install -y {packages}"),
            PackageManager::Zypper => format!("sudo zypper install -y {packages}"),
        }
    }
}

/// How to install a tool here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// A shell command, run in a terminal the user sees (where `sudo` asks for the password).
    Run { command: String, admin: bool, note: Option<&'static str> },
    /// No way Forge knows: where to get it.
    Manual { url: &'static str },
}

/// How to install `tool` on `os` with `manager`: the package manager when it has the tool,
/// else the tool's own installer when it needs no administrator (rustup, pnpm, .NET's
/// install script), else a download page.
pub fn plan(tool: Tool, os: &str, manager: Option<PackageManager>) -> Plan {
    use PackageManager::*;
    let run = |command: String| Plan::Run { admin: command.starts_with("sudo "), command, note: None };
    let package: Option<&[&str]> = match (tool, manager) {
        (_, None) => None,
        (Tool::Git, Some(_)) => Some(&["git"]),
        (Tool::Dotnet, Some(Pacman)) => Some(&["dotnet-sdk"]),
        (Tool::Dotnet, Some(Brew)) => Some(&["--cask", "dotnet-sdk"]),
        (Tool::Dotnet, Some(_)) => None,
        (Tool::Rust, Some(Pacman | Brew)) => Some(&["rustup"]),
        (Tool::Rust, Some(_)) => None,
        (Tool::Go, Some(Apt)) => Some(&["golang-go"]),
        (Tool::Go, Some(Dnf)) => Some(&["golang"]),
        (Tool::Go, Some(_)) => Some(&["go"]),
        (Tool::Node, Some(Brew)) => Some(&["node"]),
        (Tool::Node, Some(_)) => Some(&["nodejs", "npm"]),
        (Tool::Pnpm, Some(Pacman | Brew)) => Some(&["pnpm"]),
        (Tool::Pnpm, Some(_)) => None,
        (Tool::Yarn, Some(Pacman | Brew)) => Some(&["yarn"]),
        (Tool::Yarn, Some(_)) => None,
        (Tool::Bun, Some(Brew)) => Some(&["oven-sh/bun/bun"]),
        (Tool::Bun, Some(_)) => None,
        (Tool::Python, Some(Pacman)) => Some(&["python"]),
        (Tool::Python, Some(Brew)) => Some(&["python"]),
        (Tool::Python, Some(_)) => Some(&["python3"]),
        (Tool::Make, Some(_)) => Some(&["make"]),
        (Tool::Gh, Some(Pacman)) => Some(&["github-cli"]),
        (Tool::Gh, Some(_)) => Some(&["gh"]),
        (Tool::Docker, Some(Pacman)) => Some(&["docker"]),
        (Tool::Docker, Some(Apt)) => Some(&["docker.io"]),
        (Tool::Docker, Some(Dnf)) => Some(&["moby-engine"]),
        (Tool::Docker, Some(Zypper)) => Some(&["docker"]),
        (Tool::Docker, Some(Brew)) => Some(&["--cask", "docker"]),
    };
    // On macOS, git and make come with Apple's command line tools.
    if os == "macos" && matches!(tool, Tool::Git | Tool::Make) && manager.is_none() {
        return Plan::Run { command: "xcode-select --install".into(), admin: false, note: Some("installs Apple's command line tools, git and make among them") };
    }
    if let (Some(manager), Some(packages)) = (manager, package) {
        let mut command = manager.install(packages);
        // rustup installs no toolchain by itself.
        if tool == Tool::Rust {
            command.push_str(" && rustup default stable");
        }
        return run(command);
    }
    match tool {
        Tool::Rust => Plan::Run {
            command: "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y".into(),
            admin: false,
            note: Some("rustup's installer, into ~/.cargo (no administrator needed)"),
        },
        Tool::Dotnet => Plan::Run {
            command: "curl -fsSL https://dot.net/v1/dotnet-install.sh | bash -s -- --channel LTS".into(),
            admin: false,
            note: Some("Microsoft's install script, into ~/.dotnet (add it to your PATH)"),
        },
        Tool::Pnpm => Plan::Run { command: "curl -fsSL https://get.pnpm.io/install.sh | sh -".into(), admin: false, note: Some("pnpm's installer, into your home folder") },
        Tool::Yarn => run("corepack enable yarn".into()),
        Tool::Bun => Plan::Run { command: "curl -fsSL https://bun.sh/install | bash".into(), admin: false, note: Some("Bun's installer, into ~/.bun") },
        Tool::Git => Plan::Manual { url: "https://git-scm.com/downloads" },
        Tool::Go => Plan::Manual { url: "https://go.dev/dl/" },
        Tool::Node => Plan::Manual { url: "https://nodejs.org/en/download" },
        Tool::Python => Plan::Manual { url: "https://www.python.org/downloads/" },
        Tool::Make => Plan::Manual { url: "https://www.gnu.org/software/make/" },
        Tool::Gh => Plan::Manual { url: "https://cli.github.com" },
        Tool::Docker => Plan::Manual { url: "https://docs.docker.com/get-docker/" },
    }
}

/// One command for several plans: the package manager's in one call (one password), then
/// the others one after another.
pub fn combined(tools: &[Tool], os: &str, manager: Option<PackageManager>) -> Option<String> {
    let mut commands: Vec<String> = Vec::new();
    let mut managed: Vec<String> = Vec::new();
    for &tool in tools {
        if let Plan::Run { command, .. } = plan(tool, os, manager) {
            match manager.and_then(|m| command.strip_prefix(&m.install(&[])).map(|rest| (m, rest.to_string()))) {
                // `sudo pacman -S --needed go` → `go` (anything after `&&` stays a command).
                Some((_, rest)) if !rest.contains("&&") && !rest.contains("--cask") => managed.push(rest.trim().to_string()),
                _ => commands.push(command),
            }
        }
    }
    if let (Some(manager), false) = (manager, managed.is_empty()) {
        commands.insert(0, manager.install(&managed.iter().map(String::as_str).collect::<Vec<_>>()));
    }
    (!commands.is_empty()).then(|| commands.join(" && "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_a_project_needs_from_its_files() {
        let root = tempfile::tempdir().unwrap();
        let file = |name: &str, text: &str| {
            let path = root.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        };
        let files = vec![
            file("src/Api/Api.csproj", ""),
            file("tool/Cargo.toml", ""),
            file("web/package.json", r#"{"packageManager": "pnpm@9.1.0"}"#),
            file("Makefile", ""),
            file("compose.yaml", ""),
            file("tests/fixtures/sample/go.mod", ""),
        ];
        let needs = needed(root.path(), &files);
        let tools: Vec<Tool> = needs.iter().map(|n| n.tool).collect();
        assert_eq!(tools, [Tool::Git, Tool::Dotnet, Tool::Rust, Tool::Node, Tool::Pnpm, Tool::Make, Tool::Docker]);
        assert_eq!(needs.iter().find(|n| n.tool == Tool::Pnpm).unwrap().because, "web/package.json");
        assert!(Tool::Docker.optional() && !Tool::Pnpm.optional());
    }

    #[test]
    fn finds_tools_on_the_path_or_where_installers_put_them() {
        let bin = tempfile::tempdir().unwrap();
        let cargo_home = tempfile::tempdir().unwrap();
        for program in ["git"] {
            std::fs::write(bin.path().join(program), "").unwrap();
        }
        for program in ["cargo", "rustc"] {
            std::fs::write(cargo_home.path().join(program), "").unwrap();
        }
        let path = Some(bin.path().as_os_str());
        let places = vec![cargo_home.path().to_path_buf()];
        assert_eq!(find(Tool::Git, path, &places), Found::OnPath);
        assert_eq!(find(Tool::Rust, path, &places), Found::OffPath(cargo_home.path().join("cargo")));
        assert_eq!(find(Tool::Go, path, &places), Found::Missing);
    }

    #[test]
    fn installs_with_the_package_manager_or_the_tools_own_installer() {
        use PackageManager::*;
        assert_eq!(plan(Tool::Go, "linux", Some(Pacman)), Plan::Run { command: "sudo pacman -S --needed go".into(), admin: true, note: None });
        assert_eq!(plan(Tool::Go, "linux", Some(Apt)), Plan::Run { command: "sudo apt-get install -y golang-go".into(), admin: true, note: None });
        assert!(matches!(plan(Tool::Rust, "linux", Some(Pacman)), Plan::Run { command, .. } if command == "sudo pacman -S --needed rustup && rustup default stable"));
        assert!(matches!(plan(Tool::Rust, "linux", Some(Apt)), Plan::Run { command, admin: false, .. } if command.contains("sh.rustup.rs")));
        assert!(matches!(plan(Tool::Dotnet, "linux", Some(Dnf)), Plan::Run { command, admin: false, .. } if command.contains("dotnet-install.sh")));
        assert_eq!(plan(Tool::Dotnet, "macos", Some(Brew)), Plan::Run { command: "brew install --cask dotnet-sdk".into(), admin: false, note: None });
        assert!(matches!(plan(Tool::Git, "macos", None), Plan::Run { command, .. } if command == "xcode-select --install"));
        assert_eq!(plan(Tool::Go, "linux", None), Plan::Manual { url: "https://go.dev/dl/" });
    }

    /// What a real folder needs and how this machine would install what is missing:
    /// `FORGE_TOOLCHAIN_FOLDER=/path cargo test -p forge-toolchain -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn surveys_a_real_folder() {
        let Ok(folder) = std::env::var("FORGE_TOOLCHAIN_FOLDER") else { return };
        let root = PathBuf::from(folder);
        let mut files = Vec::new();
        let mut pending = vec![root.clone()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.path().is_dir() {
                    if !name.starts_with('.') && !["node_modules", "target", "vendor", "bin", "obj", "dist"].contains(&name.as_str()) {
                        pending.push(entry.path());
                    }
                } else {
                    files.push(entry.path());
                }
            }
        }
        let path = std::env::var_os("PATH");
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let manager = PackageManager::detect(std::env::consts::OS, path.as_deref());
        println!("package manager: {manager:?}");
        for need in needed(&root, &files) {
            let found = find(need.tool, path.as_deref(), &usual_places(&home));
            println!("{:<22} {:<10} because {}", need.tool.name(), format!("{found:?}").split('(').next().unwrap_or_default(), need.because);
        }
        for tool in Tool::ALL {
            println!("install {:<22} {:?}", tool.name(), plan(tool, std::env::consts::OS, manager));
        }
    }

    #[test]
    fn combines_package_manager_installs_into_one_call() {
        let command = combined(&[Tool::Go, Tool::Rust, Tool::Make, Tool::Pnpm], "linux", Some(PackageManager::Pacman)).unwrap();
        assert_eq!(command, "sudo pacman -S --needed go make pnpm && sudo pacman -S --needed rustup && rustup default stable");
        let apt = combined(&[Tool::Go, Tool::Pnpm], "linux", Some(PackageManager::Apt)).unwrap();
        assert_eq!(apt, "sudo apt-get install -y golang-go && curl -fsSL https://get.pnpm.io/install.sh | sh -");
        assert_eq!(combined(&[Tool::Go], "linux", None), None, "nothing to run, only a download page");
    }
}
