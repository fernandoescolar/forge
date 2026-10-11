//! The build configuration (Debug, Release…) and target framework chosen in the title bar,
//! which every `dotnet` command Forge runs follows: Run, Hot Reload and Debug, the Tests
//! panel and the gutter's test buttons, and the Solution Explorer's Build, Pack, Publish…
//!
//! The commands carry the choice as arguments (`-c Release -f net8.0`), so the debug
//! locators, which only see their task, read it back from there.

use gpui::{App, Global};

/// What to build. `None` leaves it to the project: `Debug`, its platform, and for a
/// project with several target frameworks, the last one it lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActiveBuild {
    pub configuration: Option<String>,
    /// A solution platform (`Any CPU`, `x64`…).
    pub platform: Option<String>,
    pub framework: Option<String>,
}

impl Global for ActiveBuild {}

impl ActiveBuild {
    pub fn get(cx: &App) -> Self {
        cx.try_global::<Self>().cloned().unwrap_or_default()
    }

    pub fn set(self, cx: &mut App) {
        cx.set_global(self);
    }

    /// The configuration, `Debug` by default.
    pub fn configuration_name(&self) -> &str {
        self.configuration.as_deref().unwrap_or("Debug")
    }

    /// The framework to run a project that targets `frameworks` with: the chosen one when
    /// the project targets it, else the last it lists. `None` when it targets only one,
    /// which needs no choosing.
    pub fn framework_for(&self, frameworks: &[String]) -> Option<String> {
        if frameworks.len() < 2 {
            return None;
        }
        self.framework.clone().filter(|chosen| frameworks.contains(chosen)).or_else(|| frameworks.last().cloned())
    }

    /// The chosen framework, when a project targeting `frameworks` has it. For commands
    /// that otherwise cover every framework (`dotnet test`).
    pub fn chosen_framework_in(&self, frameworks: &[String]) -> Option<String> {
        self.framework.clone().filter(|chosen| frameworks.len() > 1 && frameworks.contains(chosen))
    }

    /// `-c` and the platform, for `dotnet build`, `run`, `test`, `pack`, `publish`…
    pub fn configuration_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(configuration) = &self.configuration {
            args.extend(["-c".to_string(), configuration.clone()]);
        }
        if let Some(platform) = self.msbuild_platform() {
            args.push(format!("-p:Platform={platform}"));
        }
        args
    }

    /// The platform as MSBuild names it (`AnyCPU`), unless it is the default one.
    pub fn msbuild_platform(&self) -> Option<String> {
        let platform = self.platform.as_deref()?.trim();
        (!platform.is_empty() && !platform.eq_ignore_ascii_case("any cpu") && !platform.eq_ignore_ascii_case("anycpu")).then(|| platform.to_string())
    }

    /// The configuration, platform and framework in a command line built from this
    /// (`-c X`, `--configuration X`, `-p:Platform=Y`, `-f F`, `--framework F`).
    pub fn from_args(args: &[String]) -> Self {
        let unquote = |arg: &str| arg.trim_matches('"').to_string();
        let mut build = Self::default();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-c" | "--configuration" => build.configuration = args.next().map(|a| unquote(a)),
                "-f" | "--framework" => build.framework = args.next().map(|a| unquote(a)),
                _ => {
                    if let Some(platform) = arg.strip_prefix("-p:Platform=") {
                        build.platform = Some(unquote(platform));
                    }
                }
            }
        }
        build
    }

    /// The `-c`, platform and `-f` arguments of `args`, to pass on to another command.
    pub fn forwarded_args(args: &[String]) -> Vec<String> {
        let build = Self::from_args(args);
        let mut forwarded = build.configuration_args();
        if let Some(framework) = build.framework {
            forwarded.extend(["-f".to_string(), framework]);
        }
        forwarded
    }

    /// A short label for the title bar: `Debug`, `Release · net8.0`, `Release | x64`.
    pub fn label(&self) -> String {
        let mut label = self.configuration_name().to_string();
        if let Some(platform) = self.msbuild_platform() {
            label.push_str(&format!(" | {platform}"));
        }
        if let Some(framework) = &self.framework {
            label.push_str(&format!(" · {framework}"));
        }
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frameworks(list: &[&str]) -> Vec<String> {
        list.iter().map(|f| f.to_string()).collect()
    }

    #[test]
    fn picks_the_chosen_framework_when_the_project_has_it() {
        let build = ActiveBuild { framework: Some("net8.0".into()), ..Default::default() };
        assert_eq!(build.framework_for(&frameworks(&["net8.0", "net9.0"])).as_deref(), Some("net8.0"));
        assert_eq!(build.framework_for(&frameworks(&["net9.0", "net10.0"])).as_deref(), Some("net10.0"), "not targeted: the last one");
        assert_eq!(build.framework_for(&frameworks(&["net9.0"])), None, "a single framework needs no choosing");
        assert_eq!(ActiveBuild::default().framework_for(&frameworks(&["net8.0", "net9.0"])).as_deref(), Some("net9.0"));
    }

    #[test]
    fn writes_and_reads_its_arguments() {
        let build = ActiveBuild { configuration: Some("Release".into()), platform: Some("x64".into()), framework: Some("net8.0".into()) };
        assert_eq!(build.configuration_args(), vec!["-c", "Release", "-p:Platform=x64"]);
        let args: Vec<String> = ["build", "\"/a/App.csproj\"", "-c", "Release", "-p:Platform=x64", "--framework", "net8.0"].map(String::from).to_vec();
        assert_eq!(ActiveBuild::from_args(&args), build);
        assert_eq!(ActiveBuild::forwarded_args(&args), vec!["-c", "Release", "-p:Platform=x64", "-f", "net8.0"]);
        assert_eq!(build.label(), "Release | x64 · net8.0");

        let any_cpu = ActiveBuild { platform: Some("Any CPU".into()), ..Default::default() };
        assert!(any_cpu.configuration_args().is_empty());
        assert_eq!(any_cpu.label(), "Debug");
    }
}
