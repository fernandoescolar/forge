//! Run C# tests from the gutter, like Zed does for Go and Rust.
//!
//! `csharp/runnables.scm` marks test methods (`csharp-test`) and the classes that hold them
//! (`csharp-test-class`). Both run `dotnet test` on the nearest `.csproj`, filtered down to
//! that method or class.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use anyhow::Result;
use collections::HashMap;
use gpui::{App, Entity, Task};
use language::{Buffer, ContextLocation, ContextProvider, LanguageToolchainStore};
use regex::Regex;
use task::{TaskTemplate, TaskTemplates, TaskVariables, VariableName};

const PROJECT: VariableName = VariableName::Custom(Cow::Borrowed("CSHARP_PROJECT"));
const PROJECT_DIR: VariableName = VariableName::Custom(Cow::Borrowed("CSHARP_PROJECT_DIR"));
const TEST_NAME: VariableName = VariableName::Custom(Cow::Borrowed("CSHARP_TEST_NAME"));
const TEST_FILTER: VariableName = VariableName::Custom(Cow::Borrowed("CSHARP_TEST_FILTER"));

const CLASS_CAPTURE: VariableName = VariableName::Custom(Cow::Borrowed("_class_name"));
const METHOD_CAPTURE: VariableName = VariableName::Custom(Cow::Borrowed("_method_name"));

pub struct CSharpContextProvider;

impl ContextProvider for CSharpContextProvider {
    fn build_context(
        &self,
        variables: &TaskVariables,
        location: ContextLocation<'_>,
        _: Option<HashMap<String, String>>,
        _: Arc<dyn LanguageToolchainStore>,
        cx: &mut App,
    ) -> Task<Result<TaskVariables>> {
        let buffer = location.file_location.buffer.read(cx);
        let file_path = buffer.file().and_then(|file| Some(file.as_local()?.abs_path(cx)));
        let namespace = namespace_of(&buffer.text());

        let mut context = TaskVariables::default();
        if let Some(project) = file_path.as_deref().and_then(nearest_project) {
            if let Some(dir) = project.parent() {
                context.insert(PROJECT_DIR, dir.to_string_lossy().into_owned());
            }
            context.insert(PROJECT, project.to_string_lossy().into_owned());
        }
        if let Some(class) = variables.get(&CLASS_CAPTURE) {
            let (name, filter) = test_filter(
                namespace.as_deref(),
                class,
                variables.get(&METHOD_CAPTURE),
            );
            context.insert(TEST_NAME, name);
            context.insert(TEST_FILTER, filter);
        }
        Task::ready(Ok(context))
    }

    fn associated_tasks(&self, _: Option<Entity<Buffer>>, _: &App) -> Task<Option<TaskTemplates>> {
        Task::ready(Some(TaskTemplates(vec![TaskTemplate {
            label: format!("dotnet test {}", TEST_NAME.template_value()),
            command: "dotnet".into(),
            // Zed hands task args to the shell unquoted; paths can hold spaces.
            args: vec![
                "test".into(),
                PROJECT.template_value_with_whitespace(),
                "--filter".into(),
                TEST_FILTER.template_value_with_whitespace(),
            ],
            cwd: Some(PROJECT_DIR.template_value()),
            tags: vec!["csharp-test".into(), "csharp-test-class".into()],
            ..TaskTemplate::default()
        }])))
    }
}

/// The first `namespace` declaration, block-scoped or file-scoped.
pub(crate) fn namespace_of(source: &str) -> Option<String> {
    static NAMESPACE: LazyLock<Regex> =
        // Files saved by Visual Studio or Rider often start with a UTF-8 BOM.
        LazyLock::new(|| Regex::new(r"(?m)^[\s\u{FEFF}]*namespace\s+([\w.]+)").unwrap());
    Some(NAMESPACE.captures(source)?[1].to_string())
}

/// The closest `.csproj` in the file's directory or above it.
pub fn nearest_project(file: &Path) -> Option<PathBuf> {
    file.ancestors().skip(1).find_map(|dir| {
        let mut projects = std::fs::read_dir(dir)
            .ok()?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "csproj"))
            .collect::<Vec<_>>();
        projects.sort();
        projects.into_iter().next()
    })
}

/// Display name and `dotnet test --filter` expression for a test method, or for every
/// test in a class when there is no method.
fn test_filter(namespace: Option<&str>, class: &str, method: Option<&str>) -> (String, String) {
    let class = match namespace {
        Some(namespace) => format!("{namespace}.{class}"),
        None => class.to_string(),
    };
    match method {
        // Exact match only: `dotnet test` routes the filter through MSBuild, which turns
        // `\` into `/` on Unix, so `(` cannot be escaped to also catch NUnit's
        // parameterized `Method(1,2)` cases. Those still run from the class.
        Some(method) => (
            format!("{class}.{method}"),
            format!("FullyQualifiedName={class}.{method}"),
        ),
        // The trailing dot keeps `FooTests` from also running `FooTestsExtra`.
        None => (class.clone(), format!("FullyQualifiedName~{class}.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_namespaces() {
        assert_eq!(namespace_of("using X;\nnamespace A.B;\nclass C {}").as_deref(), Some("A.B"));
        assert_eq!(namespace_of("namespace A.B\n{\n}").as_deref(), Some("A.B"));
        assert_eq!(namespace_of("\u{FEFF}namespace A.B;\nclass C {}").as_deref(), Some("A.B"));
        assert_eq!(namespace_of("class C {}"), None);
    }

    #[test]
    fn builds_filters() {
        assert_eq!(
            test_filter(Some("Ns"), "FooTests", Some("Bar")),
            ("Ns.FooTests.Bar".into(), "FullyQualifiedName=Ns.FooTests.Bar".into())
        );
        assert_eq!(
            test_filter(None, "FooTests", None),
            ("FooTests".into(), "FullyQualifiedName~FooTests.".into())
        );
    }
}
