//! `Properties/launchSettings.json` (or a file-based app's `app.run.json`, or for an
//! Aspire app host the `aspire.config.json` that points at it): the launch profiles of a
//! .NET app. `dotnet run` applies them itself (`--launch-profile`, else the
//! first one that runs the project); the debugger launches the built program directly, so
//! it applies them here.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct LaunchProfile {
    pub name: String,
    /// `environmentVariables`, plus `ASPNETCORE_URLS` from `applicationUrl`.
    pub env: Vec<(String, String)>,
    /// `commandLineArgs`, split like a shell would.
    pub args: Vec<String>,
}

/// The profiles of `app` (a `.csproj`, or the `.cs` of a file-based app) that `dotnet run`
/// can use (those whose `commandName` is `Project`), in file order.
pub fn profiles(app: &Path) -> Vec<LaunchProfile> {
    let Some(dir) = app.parent() else { return vec![] };
    let file = match app.extension().and_then(|e| e.to_str()) {
        Some("cs") => dir.join(format!("{}.run.json", app.file_stem().unwrap_or_default().to_string_lossy())),
        _ => dir.join("Properties/launchSettings.json"),
    };
    match std::fs::read_to_string(file) {
        Ok(text) => parse(&text),
        // The Aspire CLI keeps a single-file app host's profiles in its aspire.config.json.
        Err(_) if app.extension().is_some_and(|e| e == "cs") => aspire_config_profiles(app),
        Err(_) => vec![],
    }
}

/// The app host an `aspire.config.json` names (`appHost.path`, relative to the file).
pub fn aspire_config_apphost(config: &Path) -> Option<PathBuf> {
    let json: Value = serde_json_lenient::from_str(&std::fs::read_to_string(config).ok()?).ok()?;
    let path = json.get("appHost")?.get("path")?.as_str()?;
    Some(normalize(&config.parent()?.join(path)))
}

/// The profiles in the `aspire.config.json` (in `app`'s folder or above) that names `app`.
fn aspire_config_profiles(app: &Path) -> Vec<LaunchProfile> {
    let app = normalize(app);
    app.ancestors()
        .skip(1)
        .map(|dir| dir.join("aspire.config.json"))
        .find(|config| aspire_config_apphost(config).is_some_and(|named| named == app))
        .and_then(|config| std::fs::read_to_string(config).ok())
        .map(|text| parse(&text))
        .unwrap_or_default()
}

/// `a/b/../c` → `a/c`, without touching the disk.
pub fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

/// The profile `dotnet run` uses: `name`, or the first one.
pub fn profile(app: &Path, name: Option<&str>) -> Option<LaunchProfile> {
    let mut profiles = profiles(app);
    match name {
        Some(name) => profiles.into_iter().find(|p| p.name == name),
        None => (!profiles.is_empty()).then(|| profiles.remove(0)),
    }
}

pub fn parse(text: &str) -> Vec<LaunchProfile> {
    // The templates write plain JSON, but people add comments and trailing commas.
    let Ok(json) = serde_json_lenient::from_str::<Value>(text) else { return vec![] };
    let Some(profiles) = json.get("profiles").and_then(Value::as_object) else { return vec![] };
    profiles
        .iter()
        .filter(|(_, p)| p.get("commandName").and_then(Value::as_str).is_none_or(|c| c == "Project"))
        .map(|(name, p)| {
            let mut env: Vec<(String, String)> = p
                .get("environmentVariables")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect();
            if let Some(urls) = p.get("applicationUrl").and_then(Value::as_str).filter(|u| !u.is_empty()) {
                if !env.iter().any(|(k, _)| k == "ASPNETCORE_URLS") {
                    env.push(("ASPNETCORE_URLS".into(), urls.to_string()));
                }
            }
            let args = p.get("commandLineArgs").and_then(Value::as_str).and_then(shlex::split).unwrap_or_default();
            LaunchProfile { name: name.clone(), env, args }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_project_profiles() {
        let text = r#"{
          // from the web template
          "profiles": {
            "http": { "commandName": "Project", "applicationUrl": "http://localhost:5000",
                      "environmentVariables": { "ASPNETCORE_ENVIRONMENT": "Development" } },
            "https": { "commandName": "Project", "applicationUrl": "https://localhost:7000;http://localhost:5000",
                       "commandLineArgs": "--seed \"two words\"" },
            "IIS Express": { "commandName": "IISExpress" },
          }
        }"#;
        let profiles = parse(text);
        assert_eq!(profiles.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["http", "https"], "only the ones dotnet run can use");
        assert_eq!(profiles[0].env, [("ASPNETCORE_ENVIRONMENT".into(), "Development".into()), ("ASPNETCORE_URLS".into(), "http://localhost:5000".into())]);
        assert_eq!(profiles[1].args, ["--seed", "two words"]);
        assert!(parse("not json").is_empty());
    }
}
