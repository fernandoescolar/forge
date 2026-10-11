//! Errors and warnings from `dotnet build` (and restore, pack, publish) in the Problems
//! panel and the editor, like a language server's.
//!
//! The language server only reports what it can see in the files it analyses: not NuGet or MSBuild
//! errors, not projects it failed to load. So the explorer's builds also write their
//! errors and warnings to a log file (an extra MSBuild file logger, which leaves the
//! terminal output as it was), and once the build ends they are published for a language
//! server of their own. Each build replaces what the previous one reported; Clean drops it.
//! A problem the language server already shows on the same line, with the same code, is
//! left out so it isn't listed twice.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use dotnet_model::build_log::BuildProblem;
use gpui::{App, Entity, EntityId, Global};
use language::ToPoint as _;
use lsp::{LanguageServerId, NumberOrString};
use project::Project;
use project::lsp_store::DocumentDiagnosticsUpdate;
use util::ResultExt as _;

/// The language server build problems are published for. It is never started: the id only
/// keeps them apart from the real servers', so they are replaced on their own.
const BUILD_SERVER: LanguageServerId = LanguageServerId(0x4000_0000);
const SOURCE: &str = "dotnet build";

/// The files each project's last build reported problems in.
#[derive(Default)]
struct Published(HashMap<EntityId, Vec<PathBuf>>);

impl Global for Published {}

/// The arguments that make a `dotnet` command also write its errors and warnings to `log`.
pub(crate) fn logger_args(log: &Path) -> Vec<String> {
    vec![format!("-flp9:LogFile={};Verbosity=quiet;Encoding=UTF-8", log.display())]
}

/// A new log file for one build.
pub(crate) fn new_log_path() -> PathBuf {
    static BUILDS: AtomicUsize = AtomicUsize::new(0);
    let build = BUILDS.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("forge-build-{}-{build}.log", std::process::id()))
}

/// Reads a build's log and removes it.
pub(crate) fn read_log(log: &Path, dir: &Path, target: &Path) -> Vec<BuildProblem> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    std::fs::remove_file(log).ok();
    dotnet_model::build_log::parse(&text, dir, target)
}

/// Shows `problems` in place of what the project's previous build reported.
pub(crate) fn publish(project: &Entity<Project>, problems: Vec<BuildProblem>, cx: &mut App) {
    let mut by_file: BTreeMap<PathBuf, Vec<BuildProblem>> = BTreeMap::new();
    for problem in problems {
        by_file.entry(problem.file.clone()).or_default().push(problem);
    }
    let reported: Vec<PathBuf> = by_file.keys().cloned().collect();
    let previous = cx.default_global::<Published>().0.remove(&project.entity_id()).unwrap_or_default();
    for file in previous {
        by_file.entry(file).or_default();
    }

    let updates = by_file
        .into_iter()
        .filter_map(|(file, problems)| {
            let uri = lsp::Uri::from_file_path(&file).ok()?;
            let shown = shown_by_language_servers(project, &file, cx);
            let diagnostics = problems
                .iter()
                .filter(|p| !shown.iter().any(|(row, code)| *row == p.line.saturating_sub(1) && code.as_deref() == p.code.as_deref()))
                .map(to_lsp)
                .collect();
            Some(DocumentDiagnosticsUpdate {
                diagnostics: lsp::PublishDiagnosticsParams { uri, diagnostics, version: None },
                result_id: None,
                registration_id: None,
                server_id: BUILD_SERVER,
                disk_based_sources: Cow::Borrowed(&[]),
            })
        })
        .collect();
    let lsp_store = project.read(cx).lsp_store();
    lsp_store.update(cx, |store, cx| store.merge_lsp_diagnostics(language::DiagnosticSourceKind::Other, updates, |_, _, _| false, cx)).log_err();
    if !reported.is_empty() {
        cx.global_mut::<Published>().0.insert(project.entity_id(), reported);
    }
}

/// The rows (from 0) and codes of what other servers report in `file`, if it is open.
fn shown_by_language_servers(project: &Entity<Project>, file: &Path, cx: &App) -> Vec<(u32, Option<String>)> {
    let project = project.read(cx);
    let Some(path) = project.find_project_path(file, cx) else { return Vec::new() };
    let Some(buffer) = project.get_open_buffer(&path, cx) else { return Vec::new() };
    let buffer = buffer.read(cx);
    let snapshot = buffer.snapshot();
    buffer
        .buffer_diagnostics(None)
        .into_iter()
        .filter(|entry| entry.diagnostic.source.as_deref() != Some(SOURCE))
        .map(|entry| {
            let code = entry.diagnostic.code.as_ref().map(|code| match code {
                NumberOrString::Number(number) => number.to_string(),
                NumberOrString::String(text) => text.clone(),
            });
            (entry.range.start.to_point(&snapshot).row, code)
        })
        .collect()
}

fn to_lsp(problem: &BuildProblem) -> lsp::Diagnostic {
    let line = problem.line.saturating_sub(1);
    let start = lsp::Position::new(line, problem.column.saturating_sub(1));
    // Without a column the problem is about its whole line.
    let end = if problem.column == 0 {
        lsp::Position::new(line, u32::MAX)
    } else {
        lsp::Position::new(problem.end_line.max(problem.line).saturating_sub(1), problem.end_column.max(problem.column).saturating_sub(1))
    };
    lsp::Diagnostic {
        range: lsp::Range::new(start, end),
        severity: Some(if problem.error { lsp::DiagnosticSeverity::ERROR } else { lsp::DiagnosticSeverity::WARNING }),
        code: problem.code.clone().map(NumberOrString::String),
        source: Some(SOURCE.to_string()),
        message: lsp::DiagnosticMessage::String(problem.message.clone()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn problem(file: &str, line: u32, code: &str, message: &str) -> BuildProblem {
        BuildProblem { file: file.into(), line, column: 1, end_line: line, end_column: 1, error: true, code: Some(code.into()), message: message.into() }
    }

    /// A build's problems show up in the Problems panel, the next build replaces them, and
    /// one the language server already shows isn't listed twice.
    #[gpui::test]
    async fn builds_replace_their_problems(cx: &mut TestAppContext) {
        cx.update(workspace::AppState::test);
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/root", serde_json::json!({ "App": { "App.csproj": "<Project />\n", "Program.cs": "a;\nb;\nc;\n" } })).await;
        let project = Project::test(fs, ["/root".as_ref()], cx).await;
        let summary = |cx: &mut TestAppContext| project.read_with(cx, |p, cx| p.diagnostic_summary(false, cx).error_count);

        let first = vec![problem("/root/App/Program.cs", 1, "CS0103", "no a"), problem("/root/App/App.csproj", 0, "NU1101", "no package")];
        cx.update(|cx| publish(&project, first, cx));
        assert_eq!(summary(cx), 2);

        // The language server reports line 2 itself.
        let buffer = project.update(cx, |p, cx| p.open_local_buffer("/root/App/Program.cs", cx)).await.unwrap();
        project.read_with(cx, |p, _| p.lsp_store()).update(cx, |store, cx| {
            let diagnostic = lsp::Diagnostic {
                range: lsp::Range::new(lsp::Position::new(1, 0), lsp::Position::new(1, 1)),
                severity: Some(lsp::DiagnosticSeverity::ERROR),
                code: Some(NumberOrString::String("CS0103".into())),
                message: lsp::DiagnosticMessage::String("no b".into()),
                ..Default::default()
            };
            let params = lsp::PublishDiagnosticsParams { uri: lsp::Uri::from_file_path("/root/App/Program.cs").unwrap(), diagnostics: vec![diagnostic], version: None };
            store.update_diagnostics(LanguageServerId(0), params, None, language::DiagnosticSourceKind::Pushed, &[], cx).unwrap();
        });
        let second = vec![problem("/root/App/Program.cs", 2, "CS0103", "no b"), problem("/root/App/Program.cs", 3, "CS0103", "no c")];
        cx.update(|cx| publish(&project, second, cx));
        let rows = buffer.read_with(cx, |b, _| {
            let snapshot = b.snapshot();
            b.buffer_diagnostics(Some(BUILD_SERVER)).iter().map(|d| d.range.start.to_point(&snapshot).row).collect::<Vec<_>>()
        });
        assert_eq!(rows, vec![2], "line 2 is the language server's, and line 1 went with the first build");
        assert_eq!(summary(cx), 2, "the project file's problem went with the first build");

        // Clean.
        cx.update(|cx| publish(&project, Vec::new(), cx));
        assert_eq!(summary(cx), 1);
    }
}
