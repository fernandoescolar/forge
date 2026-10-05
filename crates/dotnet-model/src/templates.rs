//! File templates for "New File": the built-in ones plus a project's own, listed in
//! `.forge/templates/template-list.json` (or `.vscode/solution-explorer/template-list.json`,
//! as vscode-solution-explorer keeps them). Templates are Handlebars with `{{namespace}}`
//! and `{{name}}`, plus every evaluated MSBuild property under `props`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::msbuild::Project;
use crate::paths;

#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    pub name: String,
    /// Extension of the files it makes, without the dot.
    pub extension: String,
    pub content: String,
}

const BUILT_IN: &[(&str, &str, &str)] = &[
    ("Class", "cs", "namespace {{namespace}};\n\npublic class {{name}}\n{\n}\n"),
    ("Interface", "cs", "namespace {{namespace}};\n\npublic interface {{name}}\n{\n}\n"),
    ("Record", "cs", "namespace {{namespace}};\n\npublic record {{name}}();\n"),
    ("Enum", "cs", "namespace {{namespace}};\n\npublic enum {{name}}\n{\n}\n"),
    (
        "ApiController",
        "cs",
        "using Microsoft.AspNetCore.Mvc;\n\nnamespace {{namespace}};\n\n[ApiController]\n[Route(\"api/[controller]\")]\npublic class {{name}} : ControllerBase\n{\n}\n",
    ),
    ("Module", "fs", "module {{namespace}}.{{name}}\n\n"),
    ("Class", "vb", "Imports System\n\nNamespace {{namespace}}\n\n    Public Class {{name}}\n\n    End Class\n\nEnd Namespace\n"),
    ("Class", "ts", "export class {{name}} {\n\n}\n"),
    ("Interface", "ts", "export interface {{name}} {\n\n}\n"),
];

#[derive(Deserialize)]
struct TemplateList {
    templates: Vec<TemplateEntry>,
}

#[derive(Deserialize)]
struct TemplateEntry {
    name: String,
    extension: String,
    file: String,
}

pub const TEMPLATE_DIRS: &[&str] = &[".forge/templates", ".vscode/solution-explorer"];

fn load_list(dir: &Path) -> Option<Vec<Template>> {
    let text = std::fs::read_to_string(dir.join("template-list.json")).ok()?;
    let list: TemplateList = serde_json::from_str(&text).ok()?;
    Some(
        list.templates
            .into_iter()
            .filter_map(|entry| {
                let content = std::fs::read_to_string(dir.join(paths::from_msbuild(&entry.file))).ok()?;
                Some(Template { name: entry.name, extension: entry.extension.trim_start_matches('.').to_lowercase(), content })
            })
            .collect(),
    )
}

/// Templates for files with `extension`: the workspace's own when it has a list, else the
/// built-in ones.
pub fn templates_for(workspace_roots: &[PathBuf], extension: &str) -> Vec<Template> {
    let extension = extension.trim_start_matches('.').to_lowercase();
    let custom = workspace_roots.iter().flat_map(|root| TEMPLATE_DIRS.iter().map(move |dir| root.join(dir))).find_map(|dir| load_list(&dir));
    let all = custom.unwrap_or_else(built_in);
    all.into_iter().filter(|t| t.extension == extension).collect()
}

pub fn built_in() -> Vec<Template> {
    BUILT_IN.iter().map(|(name, ext, content)| Template { name: name.to_string(), extension: ext.to_string(), content: content.to_string() }).collect()
}

/// Writes the built-in templates to `.forge/templates` so they can be customized.
pub fn install(root: &Path) -> Result<PathBuf> {
    let dir = root.join(TEMPLATE_DIRS[0]);
    std::fs::create_dir_all(&dir)?;
    let mut entries = Vec::new();
    for template in built_in() {
        let file = format!("{}.{}-template", template.name.to_lowercase(), template.extension);
        std::fs::write(dir.join(&file), &template.content)?;
        entries.push(serde_json::json!({ "name": template.name, "extension": template.extension, "file": format!("./{file}") }));
    }
    let list = serde_json::to_string_pretty(&serde_json::json!({ "templates": entries }))?;
    std::fs::write(dir.join("template-list.json"), list)?;
    Ok(dir)
}

/// The content of a new file made from `template`.
pub fn render(template: &Template, file: &Path, project: Option<&Project>) -> Result<String> {
    let mut handlebars = handlebars::Handlebars::new();
    handlebars.register_escape_fn(handlebars::no_escape);
    let dir = file.parent().unwrap_or(Path::new("."));
    let namespace = project.map(|p| p.namespace_for(dir)).unwrap_or_else(|| "Unknown".into());
    let props: BTreeMap<String, String> = project.map(|p| p.properties.iter().into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()).unwrap_or_default();
    let data = serde_json::json!({ "namespace": namespace, "name": paths::file_stem(file), "props": props });
    handlebars.render_template(&template.content, &data).context("rendering the template")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_with_the_project_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("My-App.csproj"), "<Project Sdk=\"Microsoft.NET.Sdk\" />").unwrap();
        let project = crate::msbuild::evaluate(&root.join("My-App.csproj"), &Default::default()).unwrap();
        let class = templates_for(&[root.clone()], "cs").into_iter().find(|t| t.name == "Class").unwrap();
        let text = render(&class, &root.join("Models/User.cs"), Some(&project)).unwrap();
        assert_eq!(text, "namespace My_App.Models;\n\npublic class User\n{\n}\n");

        install(&root).unwrap();
        std::fs::write(root.join(".forge/templates/class.cs-template"), "// {{props.MSBuildProjectName}}\nclass {{name}} {}").unwrap();
        let class = templates_for(&[root.clone()], ".CS").into_iter().find(|t| t.name == "Class").unwrap();
        assert_eq!(render(&class, &root.join("A.cs"), Some(&project)).unwrap(), "// My-App\nclass A {}");
    }
}
