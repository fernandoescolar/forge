//! Tools a project needs that aren't installed.
//!
//! Opening a folder looks at what it is (a .NET solution, a Cargo crate, an npm package,
//! a Git repository…) and checks that the tools it takes are on the project's `PATH`;
//! for .NET, also that an installed SDK can build it (`global.json`, or the newest
//! `net<N>.0` it targets). Without them builds, runs and language servers only fail in
//! the Output panel, so a missing tool is said in a notification, with where to get it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{App, AppContext as _, Context, EntityId};
use project::Project;
use util::command::{Stdio, new_command};
use workspace::{
    Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

/// Folders searched for .NET project files, at most this deep below the project folder.
const PROJECT_FILE_DEPTH: usize = 3;
const SKIPPED_FOLDERS: &[&str] = &["node_modules", "target", "bin", "obj", "dist", "build", "packages", "vendor"];

/// (workspace, project folder) already checked: one look per folder and window.
#[derive(Default)]
struct Checked(HashSet<(EntityId, PathBuf)>);
impl gpui::Global for Checked {}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        if window.is_none() {
            return;
        }
        let project = workspace.project().clone();
        cx.subscribe(&project, |_, project, event, cx| {
            if let project::Event::WorktreeAdded(_) = event {
                check_project(&project, cx);
            }
        })
        .detach();
        check_project(&project, cx);
    })
    .detach();
}

fn check_project(project: &gpui::Entity<Project>, cx: &mut Context<Workspace>) {
    if !project.read(cx).is_local() {
        return;
    }
    let roots: Vec<Arc<Path>> = project.read(cx).visible_worktrees(cx).map(|w| w.read(cx).abs_path()).collect();
    let id = cx.entity_id();
    for root in roots {
        if !cx.default_global::<Checked>().0.insert((id, root.to_path_buf())) {
            continue;
        }
        let env = project.update(cx, |p, cx| p.environment().update(cx, |env, cx| env.directory_environment(root.clone(), cx)));
        cx.spawn(async move |workspace, cx| {
            let env: HashMap<String, String> = env.await.unwrap_or_default().into_iter().collect();
            let missing = cx.background_spawn(async move { missing_tools(&root, &env).await }).await;
            workspace
                .update(cx, |workspace, cx| {
                    for tool in missing {
                        show(workspace, tool, cx);
                    }
                })
                .ok();
        })
        .detach();
    }
}

fn show(workspace: &mut Workspace, missing: Missing, cx: &mut Context<Workspace>) {
    let id = NotificationId::named(format!("forge-missing-tool-{}", missing.key).into());
    workspace.show_notification(id, cx, move |cx| {
        let url = missing.url.clone();
        cx.new(move |cx| {
            MessageNotification::new(missing.message.clone(), cx)
                .with_title(missing.title.clone())
                .show_suppress_button(false)
                .primary_message("Download")
                .primary_on_click(move |_, cx| cx.open_url(&url))
        })
    });
}

/// A tool the project needs and doesn't have, said to the user.
#[derive(Debug, Clone, PartialEq)]
struct Missing {
    /// One notification per key and window.
    key: String,
    title: String,
    message: String,
    url: String,
}

/// What a project folder needs installed.
#[derive(Debug, Default, PartialEq)]
struct Needs {
    git: bool,
    rust: bool,
    node: bool,
    /// `pnpm`, `yarn` or `bun`, from the lock file.
    package_manager: Option<&'static str>,
    go: bool,
    python: bool,
    dotnet: Option<DotnetNeeds>,
}

#[derive(Debug, Default, PartialEq)]
struct DotnetNeeds {
    /// `global.json`'s `sdk.version` and `sdk.rollForward`.
    global_json: Option<(SdkVersion, String)>,
    /// The newest .NET the projects target (`net10.0` → 10), when it's .NET 5 or later.
    target: Option<(u32, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SdkVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

impl SdkVersion {
    /// `10.0.100`, `10.0.100-rc.1.25451.107`.
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().split(['-', '+']).next()?.split('.');
        let version = Self { major: parts.next()?.parse().ok()?, minor: parts.next()?.parse().ok()?, patch: parts.next().unwrap_or("0").parse().ok()? };
        Some(version)
    }

    fn feature_band(self) -> u32 {
        self.patch / 100
    }
}

impl std::fmt::Display for SdkVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Whether `installed` builds a project whose `global.json` asks for `required` with
/// `roll_forward` (the .NET rules; `latestPatch` when unset).
fn satisfies_global_json(required: SdkVersion, roll_forward: &str, installed: SdkVersion) -> bool {
    let same_minor = installed.major == required.major && installed.minor == required.minor;
    match roll_forward.to_ascii_lowercase().as_str() {
        "disable" => installed == required,
        "feature" | "latestfeature" => same_minor && installed.patch >= required.patch,
        "minor" | "latestminor" => installed.major == required.major && installed >= required,
        "major" | "latestmajor" => installed >= required,
        _ => same_minor && installed.feature_band() == required.feature_band() && installed.patch >= required.patch,
    }
}

/// The .NET version a target framework moniker needs an SDK for: `net8.0`,
/// `net10.0-windows`, `netcoreapp3.1`; `None` for .NET Framework and .NET Standard.
fn target_major(tfm: &str) -> Option<u32> {
    let tfm = tfm.trim().to_ascii_lowercase();
    let version = tfm.strip_prefix("netcoreapp").or_else(|| tfm.strip_prefix("net").filter(|rest| rest.contains('.')))?;
    version.split(['.', '-']).next()?.parse().ok()
}

/// The target frameworks in an MSBuild file (`<TargetFramework>` and `<TargetFrameworks>`).
fn target_frameworks(xml: &str) -> Vec<String> {
    let mut found = Vec::new();
    for tag in ["TargetFramework", "TargetFrameworks"] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let mut rest = xml;
        while let Some(start) = rest.find(&open) {
            rest = &rest[start + open.len()..];
            let Some(end) = rest.find(&close) else { break };
            found.extend(rest[..end].split(';').map(str::trim).filter(|t| !t.is_empty() && !t.contains('$')).map(String::from));
            rest = &rest[end..];
        }
    }
    found
}

fn needs(root: &Path) -> Needs {
    let has = |name: &str| root.join(name).exists();
    let mut needs = Needs {
        git: has(".git"),
        rust: has("Cargo.toml"),
        node: has("package.json"),
        package_manager: if has("pnpm-lock.yaml") {
            Some("pnpm")
        } else if has("yarn.lock") {
            Some("yarn")
        } else if has("bun.lockb") || has("bun.lock") {
            Some("bun")
        } else {
            None
        },
        go: has("go.mod"),
        python: has("pyproject.toml") || has("requirements.txt"),
        dotnet: None,
    };

    let mut msbuild_files = Vec::new();
    let mut solution = false;
    collect_dotnet_files(root, 0, &mut msbuild_files, &mut solution);
    let global_json = std::fs::read_to_string(root.join("global.json")).ok().and_then(|text| {
        let json: serde_json::Value = serde_json_lenient::from_str(&text).ok()?;
        let sdk = json.get("sdk")?;
        let version = SdkVersion::parse(sdk.get("version")?.as_str()?)?;
        let roll_forward = sdk.get("rollForward").and_then(|r| r.as_str()).unwrap_or("latestPatch").to_string();
        Some((version, roll_forward))
    });
    let is_dotnet = solution || global_json.is_some() || msbuild_files.iter().any(|f| f.extension().is_some_and(|e| e != "props"));
    if is_dotnet {
        let target = msbuild_files
            .iter()
            .filter_map(|f| std::fs::read_to_string(f).ok())
            .flat_map(|xml| target_frameworks(&xml))
            .filter_map(|tfm| Some((target_major(&tfm)?, tfm)))
            .max_by_key(|(major, _)| *major);
        needs.dotnet = Some(DotnetNeeds { global_json, target });
    }
    needs
}

/// Project files (`*.csproj`, `*.fsproj`, `*.vbproj`, `Directory.Build.props`) and whether
/// there is a solution, a few folders deep.
fn collect_dotnet_files(dir: &Path, depth: usize, files: &mut Vec<PathBuf>, solution: &mut bool) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            if depth < PROJECT_FILE_DEPTH && !name.starts_with('.') && !SKIPPED_FOLDERS.contains(&name.to_ascii_lowercase().as_str()) {
                collect_dotnet_files(&path, depth + 1, files, solution);
            }
            continue;
        }
        match path.extension().and_then(|e| e.to_str()) {
            Some("csproj" | "fsproj" | "vbproj") => files.push(path),
            Some("sln" | "slnx") => *solution = true,
            _ if name == "Directory.Build.props" => files.push(path),
            _ => {}
        }
    }
}

/// What `needs` asks for that isn't installed, given the project's environment.
async fn missing_tools(root: &Path, env: &HashMap<String, String>) -> Vec<Missing> {
    if !root.is_dir() {
        return Vec::new();
    }
    let needs = needs(root);
    let path = env.get("PATH").cloned().or_else(|| std::env::var("PATH").ok());
    let found = |program: &str| which::which_in(program, path.as_ref(), root).ok();
    let missing_tool = |key: &str, name: &str, why: &str, programs: &[&str], url: &str| -> Option<Missing> {
        let absent: Vec<_> = programs.iter().filter(|p| found(p).is_none()).map(|p| format!("`{p}`")).collect();
        (!absent.is_empty()).then(|| Missing {
            key: key.into(),
            title: format!("{name} isn't installed"),
            message: format!("This project uses {why}, but {} wasn't found. Install {name}, then reopen the project.", absent.join(" and ")),
            url: url.into(),
        })
    };

    let mut missing = Vec::new();
    if needs.git {
        missing.extend(missing_tool("git", "Git", "Git (.git)", &["git"], "https://git-scm.com/downloads"));
    }
    if needs.rust {
        missing.extend(missing_tool("rust", "Rust", "Rust (Cargo.toml)", &["cargo", "rustc"], "https://rustup.rs"));
    }
    if needs.node {
        missing.extend(missing_tool("node", "Node.js", "Node.js (package.json)", &["node", "npm", "npx"], "https://nodejs.org/en/download"));
    }
    if let Some(manager) = needs.package_manager {
        let url = match manager {
            "pnpm" => "https://pnpm.io/installation",
            "yarn" => "https://yarnpkg.com/getting-started/install",
            _ => "https://bun.sh",
        };
        missing.extend(missing_tool(manager, manager, &format!("{manager} (its lock file)"), &[manager], url));
    }
    if needs.go {
        missing.extend(missing_tool("go", "Go", "Go (go.mod)", &["go"], "https://go.dev/dl/"));
    }
    if needs.python && found("python3").is_none() && found("python").is_none() {
        missing.extend(missing_tool("python", "Python", "Python", &["python3"], "https://www.python.org/downloads/"));
    }
    if let Some(dotnet) = needs.dotnet {
        match found("dotnet") {
            None => missing.push(Missing {
                key: "dotnet".into(),
                title: ".NET SDK isn't installed".into(),
                message: "This is a .NET project, but `dotnet` wasn't found. Building, running, tests and the C# language server need the .NET SDK. Install it, then reopen the project.".into(),
                url: dotnet_download(dotnet.target.as_ref().map(|(major, _)| *major)),
            }),
            Some(program) => {
                let sdks = installed_sdks(&program, root, env).await;
                missing.extend(missing_sdk(&dotnet, &sdks));
            }
        }
    }
    missing
}

fn dotnet_download(major: Option<u32>) -> String {
    match major {
        Some(major) => format!("https://dotnet.microsoft.com/download/dotnet/{major}.0"),
        None => "https://dotnet.microsoft.com/download".into(),
    }
}

/// `dotnet --list-sdks`: `10.0.100 [/usr/local/share/dotnet/sdk]` per line.
async fn installed_sdks(dotnet: &Path, root: &Path, env: &HashMap<String, String>) -> Vec<SdkVersion> {
    let mut command = new_command(dotnet);
    command.arg("--list-sdks").current_dir(root).envs(env).stdin(Stdio::null());
    match command.output().await {
        Ok(output) => String::from_utf8_lossy(&output.stdout).lines().filter_map(|line| SdkVersion::parse(line.split_whitespace().next()?)).collect(),
        Err(error) => {
            log::warn!("`dotnet --list-sdks` failed: {error}");
            Vec::new()
        }
    }
}

/// The SDK the project needs when none of `sdks` will do.
fn missing_sdk(needs: &DotnetNeeds, sdks: &[SdkVersion]) -> Option<Missing> {
    let installed = if sdks.is_empty() { "none".to_string() } else { sdks.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ") };
    if let Some((required, roll_forward)) = &needs.global_json {
        if !sdks.iter().any(|sdk| satisfies_global_json(*required, roll_forward, *sdk)) {
            return Some(Missing {
                key: format!("dotnet-sdk-{required}"),
                title: format!(".NET SDK {required} isn't installed"),
                message: format!(
                    "global.json asks for the .NET SDK {required} (rollForward: {roll_forward}); the installed SDKs are: {installed}. Building, running, tests and the C# language server fail until it is installed."
                ),
                url: dotnet_download(Some(required.major)),
            });
        }
    }
    if let Some((major, tfm)) = &needs.target {
        if !sdks.iter().any(|sdk| sdk.major >= *major) {
            return Some(Missing {
                key: format!("dotnet-sdk-{major}"),
                title: format!(".NET {major} SDK isn't installed"),
                message: format!(
                    "This project targets {tfm}, which needs the .NET {major} SDK or later; the installed SDKs are: {installed}. Building, running, tests and the C# language server fail until it is installed."
                ),
                url: dotnet_download(Some(*major)),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> SdkVersion {
        SdkVersion::parse(text).unwrap()
    }

    #[test]
    fn reads_target_frameworks() {
        let xml = "<Project><PropertyGroup><TargetFrameworks>net8.0;net10.0-windows;$(Extra)</TargetFrameworks></PropertyGroup><TargetFramework>netstandard2.0</TargetFramework></Project>";
        let tfms = target_frameworks(xml);
        assert_eq!(tfms, ["netstandard2.0", "net8.0", "net10.0-windows"]);
        let majors: Vec<_> = tfms.iter().map(|t| target_major(t)).collect();
        assert_eq!(majors, [None, Some(8), Some(10)]);
        assert_eq!(target_major("netcoreapp3.1"), Some(3));
        assert_eq!(target_major("net48"), None, ".NET Framework needs no SDK");
    }

    #[test]
    fn follows_global_json_roll_forward() {
        assert!(satisfies_global_json(v("8.0.100"), "latestPatch", v("8.0.110")));
        assert!(!satisfies_global_json(v("8.0.100"), "latestPatch", v("8.0.200")), "another feature band");
        assert!(satisfies_global_json(v("8.0.100"), "latestFeature", v("8.0.200")));
        assert!(!satisfies_global_json(v("8.0.100"), "latestMinor", v("9.0.100")));
        assert!(satisfies_global_json(v("8.0.100"), "latestMajor", v("10.0.100")));
        assert!(!satisfies_global_json(v("8.0.110"), "disable", v("8.0.111")));
        assert_eq!(v("10.0.100-rc.2.25502.107"), v("10.0.100"));
    }

    #[test]
    fn asks_for_the_sdk_a_project_targets() {
        let needs = DotnetNeeds { global_json: None, target: Some((10, "net10.0".into())) };
        let missing = missing_sdk(&needs, &[v("8.0.404"), v("9.0.100")]).expect("no .NET 10 SDK");
        assert_eq!(missing.title, ".NET 10 SDK isn't installed");
        assert!(missing.message.contains("8.0.404, 9.0.100"), "{}", missing.message);
        assert_eq!(missing.url, "https://dotnet.microsoft.com/download/dotnet/10.0");
        assert_eq!(missing_sdk(&needs, &[v("10.0.100")]), None);

        let pinned = DotnetNeeds { global_json: Some((v("9.0.100"), "latestPatch".into())), target: Some((8, "net8.0".into())) };
        assert!(missing_sdk(&pinned, &[v("10.0.100")]).is_some_and(|m| m.title.contains("9.0.100")));
    }

    #[test]
    fn tells_what_a_folder_is() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("package.json"), "{}").unwrap();
        std::fs::write(root.join("pnpm-lock.yaml"), "").unwrap();
        std::fs::create_dir_all(root.join("src/Api")).unwrap();
        std::fs::write(root.join("src/Api/Api.csproj"), "<Project><PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup></Project>").unwrap();
        std::fs::create_dir_all(root.join("node_modules/x")).unwrap();
        std::fs::write(root.join("node_modules/x/Old.csproj"), "<TargetFramework>net11.0</TargetFramework>").unwrap();

        let needs = needs(root);
        assert!(needs.git && needs.node && !needs.rust);
        assert_eq!(needs.package_manager, Some("pnpm"));
        assert_eq!(needs.dotnet, Some(DotnetNeeds { global_json: None, target: Some((10, "net10.0".into())) }), "node_modules is skipped");
    }

    #[gpui::test]
    async fn says_which_tools_are_missing(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("App.csproj"), "<TargetFramework>net10.0</TargetFramework>").unwrap();
        let empty_path = HashMap::from([("PATH".to_string(), dir.path().join("nothing").to_string_lossy().into_owned())]);
        let root = dir.path().to_path_buf();
        let missing = cx.background_executor.spawn(async move { missing_tools(&root, &empty_path).await }).await;
        let keys: Vec<_> = missing.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, ["rust", "dotnet"]);
        assert!(missing[0].message.contains("`cargo` and `rustc`"), "{}", missing[0].message);
        assert_eq!(missing[1].url, "https://dotnet.microsoft.com/download/dotnet/10.0");
    }
}
