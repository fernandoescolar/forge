//! `<config>/dotnet.json`: optional settings for the solution explorer and NuGet. Every
//! field has a default, so the file only needs what the user wants to change.

use std::collections::HashMap;

use dotnet_model::msbuild::EvalOptions;
use dotnet_model::msbuild::items::ItemOptions;
use dotnet_model::explorer::TreeOptions;
use gpui::{App, Global};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, schemars::JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct DotnetConfig {
    /// MSBuild `Configuration` used to evaluate projects.
    pub configuration: String,
    /// MSBuild `Platform` used to evaluate projects.
    pub platform: String,
    /// Global MSBuild properties for evaluating projects (like `-p:Name=Value`), for
    /// conditions the explorer should follow.
    pub properties: HashMap<String, String>,
    /// Folders never shown in projects.
    pub ignored_folders: Vec<String>,
    /// Item type for new files by extension (`*` for the rest).
    pub item_types: HashMap<String, String>,
    /// Nest files by name (`appsettings.Development.json` under `appsettings.json`).
    pub nest_files: bool,
    /// Open the project file when a project is clicked.
    pub open_project_on_click: bool,
    /// Reveal the active editor's file in the explorer.
    pub track_active_file: bool,
    /// Replacements for the `dotnet` command lines, by name (`build`, `test`, …).
    pub custom_commands: HashMap<String, Vec<String>>,
    /// How deep to look for solutions under each folder of the workspace.
    pub solution_search_depth: usize,
    /// The file, relative to each project's folder, that "Move usings to …" turns a file's
    /// `using` directives into `global using` directives in.
    pub global_usings_file: String,
    pub nuget: NuGetConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, schemars::JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct NuGetConfig {
    pub include_prerelease: bool,
    /// Version hints after `PackageReference`/`PackageVersion` in project files.
    pub version_hints: bool,
    /// Package and version completions in project files.
    pub completions: bool,
}

impl Default for NuGetConfig {
    fn default() -> Self {
        Self { include_prerelease: false, version_hints: true, completions: true }
    }
}

impl Default for DotnetConfig {
    fn default() -> Self {
        Self {
            configuration: "Debug".into(),
            platform: "AnyCPU".into(),
            properties: HashMap::new(),
            ignored_folders: ItemOptions::default().ignored,
            item_types: HashMap::new(),
            nest_files: true,
            open_project_on_click: false,
            track_active_file: true,
            custom_commands: HashMap::new(),
            solution_search_depth: 4,
            global_usings_file: "GlobalUsings.cs".into(),
            nuget: NuGetConfig::default(),
        }
    }
}

impl DotnetConfig {
    pub fn eval_options(&self) -> EvalOptions {
        let mut overrides: Vec<(String, String)> = self.properties.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        overrides.sort();
        EvalOptions { configuration: self.configuration.clone(), platform: self.platform.clone(), overrides, ..Default::default() }
    }

    pub fn tree_options(&self) -> TreeOptions {
        TreeOptions { items: ItemOptions { ignored: self.ignored_folders.clone() }, nest_by_name: self.nest_files }
    }

    pub fn item_types(&self) -> Vec<(String, String)> {
        let mut types = dotnet_model::msbuild::edit::default_item_types();
        for (ext, item_type) in &self.item_types {
            types.retain(|(e, _)| !e.eq_ignore_ascii_case(ext));
            types.insert(0, (ext.trim_start_matches('.').to_string(), item_type.clone()));
        }
        types
    }
}

impl Global for DotnetConfig {}

pub fn path() -> std::path::PathBuf {
    paths::config_dir().join("dotnet.json")
}

/// Reads the config, falling back to defaults (and logging) when it is missing or broken.
pub fn load() -> DotnetConfig {
    let Ok(text) = std::fs::read_to_string(path()) else { return DotnetConfig::default() };
    serde_json::from_str(&text).unwrap_or_else(|error| {
        log::error!("invalid {}: {error}", path().display());
        DotnetConfig::default()
    })
}

pub fn get(cx: &App) -> DotnetConfig {
    cx.try_global::<DotnetConfig>().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_keeps_defaults() {
        let config: DotnetConfig = serde_json::from_str(r#"{ "configuration": "Release", "nuget": { "includePrerelease": true }, "itemTypes": { "xaml": "Page" } }"#).unwrap();
        assert_eq!(config.configuration, "Release");
        assert!(config.nuget.include_prerelease && config.nuget.version_hints);
        assert!(config.nest_files);
        assert_eq!(dotnet_model::msbuild::edit::item_type_for(std::path::Path::new("A.xaml"), &config.item_types()), "Page");
        assert_eq!(dotnet_model::msbuild::edit::item_type_for(std::path::Path::new("A.cs"), &config.item_types()), "Compile");
    }
}

/// The .NET page of the Settings tab; changes made there apply right away.
pub fn register_settings(cx: &mut App) {
    use forge_ui::settings_registry::{SettingChanged, SettingsFile, SettingsPage, SettingsRegistry};
    let file = SettingsFile::Config("dotnet.json".into());
    let page = SettingsPage {
        id: "dotnet".into(),
        title: ".NET".into(),
        file: file.clone(),
        schema: serde_json::to_value(schemars::schema_for!(DotnetConfig)).unwrap_or_default(),
        keys: None,
        defaults: serde_json::to_value(DotnetConfig::default()).unwrap_or_default(),
        order: 20,
        actions: vec![],
    };
    let registry = SettingsRegistry::global(cx);
    registry.update(cx, |registry, cx| registry.register(page, cx));
    cx.subscribe(&registry, move |_, event: &SettingChanged, cx| {
        if event.file == file {
            cx.set_global(load());
        }
    })
    .detach();
}
