//! `obj/project.assets.json`, written by `dotnet restore`: the versions packages resolved to
//! and their dependency graph per target framework.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: String,
    /// Names of the packages it depends on.
    pub dependencies: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Assets {
    /// Target framework (as the project names it, e.g. `net8.0`) → packages by lowercase name.
    frameworks: Vec<(String, HashMap<String, ResolvedPackage>)>,
}

impl Assets {
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Option<Self> {
        let json: Value = serde_json::from_str(text).ok()?;
        let targets = json.get("targets")?.as_object()?;
        // `project.frameworks` keys are the short names; `targets` keys are full monikers in
        // the same order (".NETCoreApp,Version=v8.0" or "net8.0" in newer SDKs).
        let short_names: Vec<String> = json
            .pointer("/project/frameworks")
            .and_then(Value::as_object)
            .map(|f| f.keys().cloned().collect())
            .unwrap_or_default();
        let mut frameworks = Vec::new();
        for (index, (moniker, packages)) in targets.iter().filter(|(k, _)| !k.contains('/')).enumerate() {
            let mut map = HashMap::new();
            for (key, info) in packages.as_object().into_iter().flatten() {
                if info.get("type").and_then(Value::as_str) == Some("project") {
                    continue;
                }
                let Some((name, version)) = key.split_once('/') else { continue };
                let dependencies = info
                    .get("dependencies")
                    .and_then(Value::as_object)
                    .map(|deps| deps.keys().cloned().collect())
                    .unwrap_or_default();
                map.insert(name.to_lowercase(), ResolvedPackage { name: name.to_string(), version: version.to_string(), dependencies });
            }
            let name = short_names.get(index).cloned().unwrap_or_else(|| moniker.clone());
            frameworks.push((name, map));
        }
        Some(Self { frameworks })
    }

    fn framework(&self, framework: Option<&str>) -> Option<&HashMap<String, ResolvedPackage>> {
        framework
            .and_then(|f| self.frameworks.iter().find(|(name, _)| name.eq_ignore_ascii_case(f)))
            .or(self.frameworks.first())
            .map(|(_, packages)| packages)
    }

    pub fn package(&self, framework: Option<&str>, name: &str) -> Option<&ResolvedPackage> {
        self.framework(framework)?.get(&name.to_lowercase())
    }

    pub fn resolved_version(&self, framework: Option<&str>, name: &str) -> Option<String> {
        self.package(framework, name).map(|p| p.version.clone())
    }

    /// The packages a package depends on, as resolved.
    pub fn dependencies(&self, framework: Option<&str>, name: &str) -> Vec<&ResolvedPackage> {
        let Some(packages) = self.framework(framework) else { return Vec::new() };
        let Some(package) = packages.get(&name.to_lowercase()) else { return Vec::new() };
        let mut deps: Vec<_> = package.dependencies.iter().filter_map(|d| packages.get(&d.to_lowercase())).collect();
        deps.sort_by_key(|p| p.name.to_lowercase());
        deps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_resolved_versions_and_dependencies() {
        let text = r#"{
          "version": 3,
          "targets": {
            "net8.0": {
              "Serilog/3.1.1": { "type": "package", "dependencies": { "System.Text.Json": "8.0.0" } },
              "System.Text.Json/8.0.0": { "type": "package" },
              "Lib/1.0.0": { "type": "project" }
            }
          },
          "project": { "frameworks": { "net8.0": { "dependencies": { "Serilog": { "version": "[3.1.1, )" } } } } }
        }"#;
        let assets = Assets::parse(text).unwrap();
        assert_eq!(assets.resolved_version(Some("net8.0"), "serilog").as_deref(), Some("3.1.1"));
        assert_eq!(assets.dependencies(None, "Serilog")[0].name, "System.Text.Json");
        assert!(assets.package(None, "Lib").is_none());
    }
}
