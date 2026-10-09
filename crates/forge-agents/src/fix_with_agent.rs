//! "Fix with agent": a button in the diagnostic's hover popover, and Agents › Fix the
//! Problem at the Cursor (also in the command palette) for the keyboard. Either sends
//! the diagnostic, the code around it and the file to the agent: to the thread on screen
//! if it is free, otherwise to a new thread. Agents › Fix the Problems in This File and
//! … in the Project (also the Problems tab's button) send every error and warning there.

use std::ops::Range;
use std::rc::Rc;

use editor::Editor;
use gpui::{App, Entity, actions};
use language::{Buffer, DiagnosticSeverity, Point};
use workspace::Workspace;

actions!(forge_agent, [
    FixProblemAtCursor,
    /// Asks an agent to fix every error and warning in the active file.
    FixProblemsInFile,
    /// Asks an agent to fix every error and warning the language servers report in the project.
    FixProblemsInProject,
]);

/// Problems listed in a "fix the problems" message at most (the agent can ask for the rest).
const MAX_LISTED: usize = 50;

/// Lines of context on each side of the diagnostic.
const CONTEXT_LINES: u32 = 4;

pub fn init(cx: &mut App) {
    cx.set_global(editor::DiagnosticPopoverAction {
        icon: ui::IconName::ZedAgent,
        tooltip: "Ask the agent to fix this".into(),
        on_click: Rc::new(|editor, range, window, cx| {
            let Some(workspace) = editor.workspace() else { return };
            let multibuffer = editor.buffer().read(cx);
            let Some((buffer, start)) = multibuffer.text_anchor_for_position(range.start, cx) else { return };
            let end = multibuffer.text_anchor_for_position(range.end, cx).map(|(_, end)| end).unwrap_or(start);
            let diagnostics = diagnostics_in(&buffer, start..end, cx);
            if let Some(prompt) = prompt(&buffer, &workspace, &diagnostics, cx) {
                window.dispatch_action(Box::new(forge_ui::AskAgent { prompt }), cx);
            }
        }),
    });
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &FixProblemsInProject, window, cx| fix_project(workspace, window, cx));
    })
    .detach();
    cx.observe_new(|editor: &mut Editor, _, cx| {
        if !editor.mode().is_full() {
            return;
        }
        // Every error and warning in the file.
        let this = cx.entity().downgrade();
        editor
            .register_action(move |_: &FixProblemsInFile, window, cx| {
                this.update(cx, |editor, cx| {
                    let Some(workspace) = editor.workspace() else { return };
                    let Some(buffer) = editor.buffer().read(cx).as_singleton() else { return };
                    let Some(path) = buffer.read(cx).file().and_then(|f| f.as_local()).map(|f| f.abs_path(cx)) else { return };
                    let problems = crate::verify::problems_of(&buffer.read(cx).snapshot(), crate::verify::BASELINE_LIMIT);
                    let root = workspace.read(cx).project().read(cx).visible_worktrees(cx).next().map(|wt| wt.read(cx).abs_path().to_path_buf()).unwrap_or_default();
                    match fix_problems_prompt(&[(path, problems)], &root, "in this file") {
                        Some(prompt) => window.dispatch_action(Box::new(forge_ui::AskAgent { prompt }), cx),
                        None => workspace.update(cx, |ws, cx| ws.show_toast(workspace::Toast::new(workspace::notifications::NotificationId::named("forge-no-problems".into()), "This file has no errors or warnings."), cx)),
                    }
                })
                .ok();
            })
            .detach();
        // The errors and warnings on the cursor's line.
        let this = cx.entity().downgrade();
        editor
            .register_action(move |_: &FixProblemAtCursor, window, cx| {
                this.update(cx, |editor, cx| {
                    let Some(workspace) = editor.workspace() else { return };
                    let head = editor.selections.newest_anchor().head();
                    let multibuffer = editor.buffer().read(cx);
                    let Some((buffer, at)) = multibuffer.text_anchor_for_position(head, cx) else { return };
                    let snapshot = buffer.read(cx).snapshot();
                    let row = text::ToPoint::to_point(&at, &snapshot).row;
                    let line = snapshot.anchor_before(Point::new(row, 0))..snapshot.anchor_after(Point::new(row, snapshot.line_len(row)));
                    let diagnostics = diagnostics_in(&buffer, line, cx);
                    if let Some(prompt) = prompt(&buffer, &workspace, &diagnostics, cx) {
                        window.dispatch_action(Box::new(forge_ui::AskAgent { prompt }), cx);
                    }
                })
                .ok();
            })
            .detach();
    })
    .detach();
}

/// The problems the language servers report in the project, as one message for an agent.
fn fix_project(workspace: &mut Workspace, window: &mut gpui::Window, cx: &mut gpui::Context<Workspace>) {
    let project = workspace.project().clone();
    let root = project.read(cx).visible_worktrees(cx).next().map(|wt| wt.read(cx).abs_path().to_path_buf()).unwrap_or_default();
    let mut files: Vec<_> = crate::verify::diagnostic_counts(&project, cx).into_iter().filter(|(_, (e, w))| e + w > 0).map(|(p, _)| p).collect();
    files.sort();
    let weak = project.downgrade();
    cx.spawn_in(window, async move |workspace, cx| {
        let checks = crate::verify::read_problems(weak, files, Default::default(), cx).await;
        let found: Vec<_> = checks.into_iter().map(|c| (c.path, c.problems)).collect();
        workspace
            .update_in(cx, |ws, window, cx| match fix_problems_prompt(&found, &root, "in the project") {
                Some(prompt) => window.dispatch_action(Box::new(forge_ui::AskAgent { prompt }), cx),
                None => ws.show_toast(workspace::Toast::new(workspace::notifications::NotificationId::named("forge-no-problems".into()), "The language servers report no errors or warnings."), cx),
            })
            .ok();
    })
    .detach();
}

/// "Fix these problems …": errors first, as mentions (`@path:line`) the agent can open, at
/// most [`MAX_LISTED`]; `None` when there are none.
pub(crate) fn fix_problems_prompt(files: &[(std::path::PathBuf, Vec<crate::verify::Problem>)], root: &std::path::Path, scope: &str) -> Option<String> {
    let mut all: Vec<(String, &crate::verify::Problem)> = files
        .iter()
        .flat_map(|(path, problems)| {
            let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned();
            problems.iter().map(move |p| (rel.clone(), p))
        })
        .collect();
    if all.is_empty() {
        return None;
    }
    all.sort_by(|(a_path, a), (b_path, b)| (!a.error, a_path, a.line).cmp(&(!b.error, b_path, b.line)));
    let errors = all.iter().filter(|(_, p)| p.error).count();
    let mut text = format!(
        "Fix the problems the language servers report {scope}: {errors} error{} and {} warning{}. Errors first; check your changes with `check_file` as you go.\n",
        if errors == 1 { "" } else { "s" },
        all.len() - errors,
        if all.len() - errors == 1 { "" } else { "s" },
    );
    for (path, p) in all.iter().take(MAX_LISTED) {
        text.push_str(&format!("- @{path}:{} {}: {}\n", p.line + 1, if p.error { "error" } else { "warning" }, p.message));
    }
    if all.len() > MAX_LISTED {
        text.push_str(&format!("… and {} more: list them with `diagnostics`.\n", all.len() - MAX_LISTED));
    }
    Some(text)
}

/// Errors first, then warnings; hints and infos are not worth an agent round-trip.
fn diagnostics_in(buffer: &Entity<Buffer>, range: Range<text::Anchor>, cx: &App) -> Vec<(Range<Point>, String, DiagnosticSeverity)> {
    let snapshot = buffer.read(cx).snapshot();
    let mut found: Vec<_> = snapshot
        .diagnostics_in_range::<_, Point>(range, false)
        .filter(|entry| entry.diagnostic.is_primary && entry.diagnostic.severity <= DiagnosticSeverity::WARNING)
        .map(|entry| {
            let code = entry.diagnostic.code.as_ref().map(|code| match code {
                lsp::NumberOrString::Number(n) => n.to_string(),
                lsp::NumberOrString::String(s) => s.clone(),
            });
            let message = match code {
                Some(code) => format!("{} ({code})", entry.diagnostic.message.as_str()),
                None => entry.diagnostic.message.as_str().to_string(),
            };
            (entry.range, message, entry.diagnostic.severity)
        })
        .collect();
    found.sort_by_key(|(range, _, severity)| (*severity, range.start));
    found
}

fn prompt(buffer: &Entity<Buffer>, workspace: &Entity<Workspace>, diagnostics: &[(Range<Point>, String, DiagnosticSeverity)], cx: &App) -> Option<String> {
    let (range, _, _) = diagnostics.first()?;
    let buffer = buffer.read(cx);
    let file = buffer.file()?;
    let abs_path = file.as_local()?.abs_path(cx);
    let root = workspace.read(cx).project().read(cx).visible_worktrees(cx).next()?.read(cx).abs_path().to_path_buf();
    let path = abs_path.strip_prefix(&root).unwrap_or(&abs_path).to_string_lossy().into_owned();
    let line = range.start.row + 1;

    let snapshot = buffer.snapshot();
    let first = range.start.row.saturating_sub(CONTEXT_LINES);
    let last = (range.end.row + CONTEXT_LINES).min(snapshot.max_point().row);
    let excerpt: String = snapshot.text_for_range(Point::new(first, 0)..Point::new(last, snapshot.line_len(last))).collect();
    let language = buffer.language().map(|l| l.name().as_ref().to_lowercase()).unwrap_or_default();

    let problems: Vec<String> = diagnostics
        .iter()
        .map(|(range, message, severity)| {
            let kind = if *severity == DiagnosticSeverity::ERROR { "error" } else { "warning" };
            format!("- line {}: {kind}: {message}", range.start.row + 1)
        })
        .collect();
    Some(format!(
        "Fix this in @{path} around line {line}:\n{}\n\nCode (lines {}-{}):\n```{language}\n{excerpt}\n```",
        problems.join("\n"),
        first + 1,
        last + 1,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::Problem;

    #[test]
    fn lists_errors_first_and_caps_the_rest() {
        let p = |line: u32, error: bool, message: &str| Problem { line, error, message: message.into() };
        let files = vec![("/p/src/b.rs".into(), vec![p(9, false, "unused import")]), ("/p/src/a.rs".into(), vec![p(2, false, "unused variable"), p(4, true, "mismatched types")])];
        assert_eq!(
            fix_problems_prompt(&files, std::path::Path::new("/p"), "in the project").unwrap(),
            "Fix the problems the language servers report in the project: 1 error and 2 warnings. Errors first; check your changes with `check_file` as you go.\n\
             - @src/a.rs:5 error: mismatched types\n- @src/a.rs:3 warning: unused variable\n- @src/b.rs:10 warning: unused import\n"
        );
        assert_eq!(fix_problems_prompt(&[("/p/a.rs".into(), vec![])], std::path::Path::new("/p"), "in this file"), None);
        let many: Vec<_> = (0..60).map(|i| p(i, true, "e")).collect();
        let text = fix_problems_prompt(&[("/p/a.rs".into(), many)], std::path::Path::new("/p"), "in this file").unwrap();
        assert_eq!(text.lines().filter(|l| l.starts_with("- @")).count(), MAX_LISTED);
        assert!(text.ends_with("… and 10 more: list them with `diagnostics`.\n"));
    }
}
