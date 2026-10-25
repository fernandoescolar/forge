//! The Tests panel's tools for agents (`run_tests`), the tests that failed for
//! `user_context`, and debugging tests for `start_debugging` (through Run's launcher).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_ui::ide_tools::{IdeTool, ToolContext, ToolEffect, ToolReply, closed, ide_tools, str_arg, tail};
use gpui::{App, AsyncWindowContext};
use serde_json::{Value, json};

use crate::discovery::{NO_ROW, TestProject};
use crate::runner::Scope;
use crate::trx::Outcome;
use crate::TestPanel;

/// Lines of a run's output quoted when nothing could run.
const TAIL_LINES: usize = 80;

pub(crate) fn init(_: &mut App) {
    ide_tools().register(IdeTool::new(
        "run_tests",
        "Run tests",
        "Run tests in Forge's Tests panel (.NET, Go, Rust, Jest, Vitest, pytest), where the user sees them run and their results in the editor. \
Returns how many passed, failed and were skipped, and each failure with its file, line, message and stack; or the build output when nothing could run. \
With no arguments it runs every test. Waits for the run to end.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Only the tests in this file, or in the files under this folder (relative to the project root)." },
                "name": { "type": "string", "description": "Only the tests whose name (with its class and namespace) contains this, ignoring case." }
            }
        }),
        ToolEffect::RunsCode,
        |ide, args, mut cx| Box::pin(async move { run_tests(ide, args, &mut cx).await }),
    ));
    ide_tools().add_context(
        "failed-tests",
        Arc::new(|ide, workspace, cx| {
            let panel = workspace.panel::<TestPanel>(cx)?;
            let panel = panel.read(cx);
            let failed: Vec<String> = panel
                .projects()
                .iter()
                .flat_map(|p| p.classes.iter().flat_map(|c| &c.tests))
                .filter(|t| panel.results_for(&t.fqn).iter().any(|r| r.outcome == Outcome::Failed))
                .map(|t| format!("{} ({})", t.fqn, ide.show(&t.file)))
                .take(10)
                .collect();
            (!failed.is_empty()).then(|| format!("Tests that failed in their last run: {}", failed.join(", ")))
        }),
    );
}

/// Which tests of `projects` a call means: `None` for all of them, else the method FQNs
/// per project manifest. `path` is a test file or a folder of them; `name` a part of the
/// test's name (or of its class and namespace).
pub fn select_tests(projects: &[TestProject], path: Option<&Path>, name: Option<&str>) -> Option<Vec<(PathBuf, Vec<String>)>> {
    if path.is_none() && name.is_none() {
        return None;
    }
    let name = name.map(str::to_lowercase);
    let mut jobs = Vec::new();
    for project in projects {
        let methods: Vec<String> = project
            .classes
            .iter()
            .flat_map(|c| &c.tests)
            .filter(|t| path.is_none_or(|p| t.file.starts_with(p)))
            .filter(|t| name.as_ref().is_none_or(|n| t.fqn.to_lowercase().contains(n.as_str())))
            .map(|t| t.fqn.clone())
            .collect();
        if !methods.is_empty() {
            jobs.push((project.path.clone(), methods));
        }
    }
    Some(jobs)
}

/// Starts debugging the tests a `start_debugging` call names, as a test's Debug button does.
pub fn start_test_session(ide: &ToolContext, path: Option<&Path>, name: Option<&str>, cx: &mut AsyncWindowContext) -> Result<(), String> {
    let panel = ide
        .workspace
        .update_in(cx, |ws, window, cx| {
            ws.open_panel::<TestPanel>(window, cx);
            ws.panel::<TestPanel>(cx)
        })
        .map_err(|_| closed())?
        .ok_or("The Tests panel isn't ready yet; try again in a moment.")?;
    panel
        .update_in(cx, |panel, window, cx| {
            let jobs = select_tests(panel.projects(), path, name).unwrap_or_default();
            let Some((manifest, fqns)) = jobs.into_iter().next() else { return Err("No test matches that path or name.".to_string()) };
            let label = format!("Debug {}", name.unwrap_or("tests"));
            if panel.debug_scope(&manifest, Scope::Methods(fqns), label.into(), window, cx) { Ok(()) } else { Err("Forge can't debug those tests (Jest and Vitest tests aren't supported).".to_string()) }
        })
        .map_err(|_| closed())?
}

/// Runs tests in the Tests panel (shown, with its progress and the gutter results) and
/// reports how they went: counts, and each failure with its place and message.
async fn run_tests(ide: ToolContext, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let path = str_arg(&args, "path").map(|p| ide.resolve(&p)).transpose()?;
    let name = str_arg(&args, "name");
    let panel = ide
        .workspace
        .update_in(cx, |ws, window, cx| {
            ws.open_panel::<TestPanel>(window, cx);
            ws.panel::<TestPanel>(cx)
        })
        .map_err(|_| closed())?
        .ok_or("The Tests panel isn't ready yet; try again in a moment.")?;
    let (started, scope) = panel
        .update_in(cx, |panel, window, cx| {
            if panel.is_running() {
                return Err("Tests are already running in the Tests panel; try again when they finish.".to_string());
            }
            if panel.projects().is_empty() {
                return Err("Forge found no test projects here.".to_string());
            }
            let selection = select_tests(panel.projects(), path.as_deref(), name.as_deref());
            match &selection {
                Some(jobs) if jobs.is_empty() => return Err("No test matches that path or name.".to_string()),
                Some(jobs) => panel.run_scopes(jobs.iter().map(|(m, fqns)| (m.clone(), Scope::Methods(fqns.clone()))).collect(), window, cx),
                None => panel.run_all(window, cx),
            }
            Ok((panel.is_running(), selection))
        })
        .map_err(|_| closed())??;
    if !started {
        return Err("The Tests panel didn't start the run.".into());
    }
    while panel.read_with(cx, |p, _| p.is_running()) {
        cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
    }
    Ok(panel.read_with(cx, |panel, _| {
        let in_scope = |fqn: &str| scope.as_ref().is_none_or(|jobs| jobs.iter().any(|(_, fqns)| fqns.iter().any(|f| f == fqn)));
        let (mut passed, mut skipped, mut not_run) = (0, 0, 0);
        let mut failures = Vec::new();
        for project in panel.projects() {
            for test in project.classes.iter().flat_map(|c| &c.tests).filter(|t| in_scope(&t.fqn)) {
                let results = panel.results_for(&test.fqn);
                if results.is_empty() {
                    not_run += 1;
                } else if let Some(failed) = results.iter().find(|r| r.outcome == Outcome::Failed) {
                    let place = if test.row == NO_ROW { ide.show(&test.file) } else { format!("{}:{}", ide.show(&test.file), test.row + 1) };
                    let mut text = format!("FAILED {} ({place})", failed.display_name);
                    if let Some(message) = &failed.message {
                        text.push_str(&format!("\n  {}", tail(message, 15).replace('\n', "\n  ")));
                    }
                    if let Some(stack) = &failed.stack_trace {
                        text.push_str(&format!("\n  {}", stack.lines().take(6).collect::<Vec<_>>().join("\n  ")));
                    }
                    failures.push(text);
                } else if results.iter().all(|r| r.outcome == Outcome::Skipped) {
                    skipped += 1;
                } else {
                    passed += 1;
                }
            }
        }
        let mut report = format!("{passed} passed, {} failed, {skipped} skipped, {not_run} not run.", failures.len());
        if !failures.is_empty() {
            report.push_str(&format!("\n\n{}", failures.join("\n\n")));
        }
        // Nothing ran: the build (or the runner) failed; its output says why.
        if passed + failures.len() + skipped == 0 {
            if let Some(error) = panel.error() {
                report.push_str(&format!("\n\n{error}"));
            }
            if let Some(log) = panel.last_log() {
                report.push_str(&format!("\n\nOutput:\n{}", tail(log, TAIL_LINES)));
            }
        }
        report
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `run_tests` reaches agents (it runs the project's code, so it may ask first).
    #[gpui::test]
    fn registers_run_tests(cx: &mut gpui::TestAppContext) {
        cx.update(init);
        assert_eq!(ide_tools().get("run_tests").map(|t| t.effect), Some(ToolEffect::RunsCode));
    }

    #[test]
    fn selects_tests_by_file_folder_and_name() {
        use crate::discovery::{Kind, TestClass, TestMethod};
        let method = |fqn: &str, file: &str| TestMethod { fqn: fqn.into(), name: fqn.rsplit('.').next().unwrap().into(), file: file.into(), row: 1, cases: vec![] };
        let project = TestProject {
            name: "app".into(),
            path: "/p/Cargo.toml".into(),
            kind: Kind::Rust { package: "app".into() },
            classes: vec![TestClass { fqn: "parser".into(), name: "parser".into(), file: "/p/src/parser.rs".into(), row: 0, tests: vec![method("parser.reads_tabs", "/p/src/parser.rs"), method("parser.reads_spaces", "/p/src/parser.rs"), method("lexer.reads_tabs", "/p/src/lexer.rs")] }],
        };
        let projects = [project];
        assert_eq!(select_tests(&projects, None, None), None, "all of them");
        let by_file = select_tests(&projects, Some(Path::new("/p/src/parser.rs")), None).unwrap();
        assert_eq!(by_file, vec![(PathBuf::from("/p/Cargo.toml"), vec!["parser.reads_tabs".to_string(), "parser.reads_spaces".to_string()])]);
        assert_eq!(select_tests(&projects, Some(Path::new("/p/src")), Some("TABS")).unwrap()[0].1, ["parser.reads_tabs", "lexer.reads_tabs"]);
        assert!(select_tests(&projects, None, Some("nothing")).unwrap().is_empty());
    }
}
