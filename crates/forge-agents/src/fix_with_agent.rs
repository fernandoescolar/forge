//! "Fix with agent": a button in the diagnostic's hover popover, and Agents › Fix the
//! Problem at the Cursor (also in the command palette) for the keyboard. Either sends
//! the diagnostic, the code around it and the file to the agent: to the thread on screen
//! if it is free, otherwise to a new thread.

use std::ops::Range;
use std::rc::Rc;

use editor::Editor;
use gpui::{App, Entity, actions};
use language::{Buffer, DiagnosticSeverity, Point};
use workspace::Workspace;

actions!(forge_agent, [FixProblemAtCursor]);

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
    cx.observe_new(|editor: &mut Editor, _, cx| {
        if !editor.mode().is_full() {
            return;
        }
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
