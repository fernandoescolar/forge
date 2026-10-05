//! `Properties/launchSettings.json`: the launch profiles of a .NET app. `dotnet run`
//! applies them itself (`--launch-profile`, else the first one that runs the project); the
//! debugger launches the built program directly, so it applies them here.

use std::path::Path;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct LaunchProfile {
    pub name: String,
    /// `environmentVariables`, plus `ASPNETCORE_URLS` from `applicationUrl`.
    pub env: Vec<(String, String)>,
    /// `commandLineArgs`, split like a shell would.
    pub args: Vec<String>,
}

/// The profiles of the project in `project_dir` that `dotnet run` can use (those whose
/// `commandName` is `Project`), in file order.
pub fn profiles(project_dir: &Path) -> Vec<LaunchProfile> {
    let Ok(text) = std::fs::read_to_string(project_dir.join("Properties/launchSettings.json")) else { return vec![] };
    parse(&text)
}

/// The profile `dotnet run` uses: `name`, or the first one.
pub fn profile(project_dir: &Path, name: Option<&str>) -> Option<LaunchProfile> {
    let mut profiles = profiles(project_dir);
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
