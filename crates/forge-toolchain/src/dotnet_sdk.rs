//! Whether an installed .NET SDK can build the project: the one `global.json` asks for (with
//! its `rollForward`), or one at least as new as the newest `net<N>.0` the projects target.
//! With `dotnet` installed but the wrong SDK, builds, runs, tests and the C# language server
//! only fail in the Output panel.

use std::path::{Path, PathBuf};

/// What the project's .NET files ask of the SDK.
#[derive(Debug, Default, PartialEq)]
pub struct DotnetNeeds {
    /// `global.json`'s `sdk.version` and `sdk.rollForward`.
    pub global_json: Option<(SdkVersion, String)>,
    /// The newest .NET the projects target (`net10.0` → 10), when it's .NET 5 or later.
    pub target: Option<(u32, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SdkVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

impl SdkVersion {
    /// `10.0.100`, `10.0.100-rc.1.25451.107`.
    pub fn parse(text: &str) -> Option<Self> {
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

/// An SDK the project needs and none of the installed ones is.
#[derive(Debug, Clone, PartialEq)]
pub struct SdkGap {
    pub title: String,
    pub message: String,
    pub url: String,
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

/// What the project files (`*.csproj`, `*.fsproj`, `*.vbproj`, `Directory.Build.props`) and
/// the `global.json` at `root` ask for, from the files the worktree lists.
pub fn needs(root: &Path, files: &[PathBuf]) -> DotnetNeeds {
    let global_json = std::fs::read_to_string(root.join("global.json")).ok().and_then(|text| {
        let json: serde_json::Value = serde_json_lenient::from_str(&text).ok()?;
        let sdk = json.get("sdk")?;
        let version = SdkVersion::parse(sdk.get("version")?.as_str()?)?;
        let roll_forward = sdk.get("rollForward").and_then(|r| r.as_str()).unwrap_or("latestPatch").to_string();
        Some((version, roll_forward))
    });
    let target = files
        .iter()
        .filter(|f| matches!(f.extension().and_then(|e| e.to_str()), Some("csproj" | "fsproj" | "vbproj")) || f.file_name().is_some_and(|n| n == "Directory.Build.props"))
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .flat_map(|xml| target_frameworks(&xml))
        .filter_map(|tfm| Some((target_major(&tfm)?, tfm)))
        .max_by_key(|(major, _)| *major);
    DotnetNeeds { global_json, target }
}

/// `dotnet --list-sdks`: `10.0.100 [/usr/local/share/dotnet/sdk]` per line.
pub fn installed_sdks(dotnet: &Path, root: &Path, path: Option<&str>) -> Vec<SdkVersion> {
    let mut command = forge_process::std_command(dotnet);
    command.arg("--list-sdks").current_dir(root).stdin(std::process::Stdio::null());
    if let Some(path) = path {
        command.env("PATH", path);
    }
    match forge_process::output(command) {
        Ok(output) => String::from_utf8_lossy(&output.stdout).lines().filter_map(|line| SdkVersion::parse(line.split_whitespace().next()?)).collect(),
        Err(error) => {
            log::warn!("`dotnet --list-sdks` failed: {error}");
            Vec::new()
        }
    }
}

pub fn download_url(major: Option<u32>) -> String {
    match major {
        Some(major) => format!("https://dotnet.microsoft.com/download/dotnet/{major}.0"),
        None => "https://dotnet.microsoft.com/download".into(),
    }
}

/// The SDK the project needs when none of `sdks` will do.
pub fn gap(needs: &DotnetNeeds, sdks: &[SdkVersion]) -> Option<SdkGap> {
    let installed = if sdks.is_empty() { "none".to_string() } else { sdks.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ") };
    if let Some((required, roll_forward)) = &needs.global_json {
        if !sdks.iter().any(|sdk| satisfies_global_json(*required, roll_forward, *sdk)) {
            return Some(SdkGap {
                title: format!(".NET SDK {required} isn't installed"),
                message: format!(
                    "global.json asks for the .NET SDK {required} (rollForward: {roll_forward}); the installed SDKs are: {installed}. Building, running, tests and the C# language server fail until it is installed."
                ),
                url: download_url(Some(required.major)),
            });
        }
    }
    if let Some((major, tfm)) = &needs.target {
        if !sdks.iter().any(|sdk| sdk.major >= *major) {
            return Some(SdkGap {
                title: format!(".NET {major} SDK isn't installed"),
                message: format!(
                    "This project targets {tfm}, which needs the .NET {major} SDK or later; the installed SDKs are: {installed}. Building, running, tests and the C# language server fail until it is installed."
                ),
                url: download_url(Some(*major)),
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
        let gap = gap(&needs, &[v("8.0.404"), v("9.0.100")]).expect("no .NET 10 SDK");
        assert_eq!(gap.title, ".NET 10 SDK isn't installed");
        assert!(gap.message.contains("8.0.404, 9.0.100"), "{}", gap.message);
        assert_eq!(gap.url, "https://dotnet.microsoft.com/download/dotnet/10.0");
        assert_eq!(super::gap(&needs, &[v("10.0.100")]), None);

        let pinned = DotnetNeeds { global_json: Some((v("9.0.100"), "latestPatch".into())), target: Some((8, "net8.0".into())) };
        assert!(super::gap(&pinned, &[v("10.0.100")]).is_some_and(|g| g.title.contains("9.0.100")));
    }

    #[test]
    fn reads_what_the_project_files_ask_for() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/Api")).unwrap();
        let api = root.join("src/Api/Api.csproj");
        std::fs::write(&api, "<Project><PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup></Project>").unwrap();
        std::fs::write(root.join("global.json"), r#"{ "sdk": { "version": "10.0.100", "rollForward": "latestFeature" } }"#).unwrap();
        let needs = needs(root, &[api, root.join("global.json")]);
        assert_eq!(needs, DotnetNeeds { global_json: Some((v("10.0.100"), "latestFeature".into())), target: Some((10, "net10.0".into())) });
    }
}
