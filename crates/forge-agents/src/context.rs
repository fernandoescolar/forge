//! Context the special mentions stand for, gathered when the message is sent:
//! `@problems` (the project's errors and warnings), `@diff` (uncommitted changes) and
//! `@terminal` (the latest terminal output). Each becomes a text block of the prompt.

use std::path::PathBuf;

use gpui::{App, AppContext as _, AsyncApp, Entity, Task, WeakEntity, Window};
use language::{DiagnosticSeverity, Point};
use project::Project;
use serde_json::{Value, json};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use workspace::Workspace;

const MAX_PROBLEMS: usize = 200;
const MAX_DIFF_BYTES: usize = 100_000;
const TERMINAL_LINES: usize = 200;

/// The text blocks for `specials` (names from `SPECIAL_MENTIONS`).
pub fn gather(specials: &[String], workspace: WeakEntity<Workspace>, project: WeakEntity<Project>, root: PathBuf, cx: &mut App) -> Task<Vec<Value>> {
    let terminal = specials.iter().any(|s| s == "terminal").then(|| workspace.upgrade().and_then(|w| terminal_output(w.read(cx), cx)));
    let problems = specials.iter().any(|s| s == "problems").then(|| project.upgrade().map(|p| problems(p, cx)));
    let diff = specials.iter().any(|s| s == "diff").then(|| git_diff(root, cx));
    cx.spawn(async move |_| {
        let mut blocks = Vec::new();
        if let Some(problems) = problems {
            let text = match problems {
                Some(task) => task.await,
                None => String::new(),
            };
            let text = if text.is_empty() { "No errors or warnings.".to_string() } else { text };
            blocks.push(json!({ "type": "text", "text": format!("<problems>\n{text}\n</problems>") }));
        }
        if let Some(diff) = diff {
            let text = diff.await;
            let text = if text.trim().is_empty() { "No uncommitted changes.".to_string() } else { text };
            blocks.push(json!({ "type": "text", "text": format!("<diff>\n{text}\n</diff>") }));
        }
        if let Some(terminal) = terminal {
            let text = terminal.unwrap_or_else(|| "No terminal is open.".to_string());
            blocks.push(json!({ "type": "text", "text": format!("<terminal>\n{text}\n</terminal>") }));
        }
        blocks
    })
}

/// The terminal on screen: the active tab if it is one, else the terminal panel's.
fn visible_terminal(workspace: &Workspace, cx: &App) -> Option<Entity<TerminalView>> {
    let active = workspace.active_item(cx).and_then(|i| i.downcast::<TerminalView>());
    active.or_else(|| {
        let panel = workspace.panel::<TerminalPanel>(cx)?;
        panel.read(cx).panes().into_iter().find_map(|pane| pane.read(cx).active_item()?.downcast::<TerminalView>())
    })
}

/// The last lines of the terminal on screen.
pub fn terminal_output(workspace: &Workspace, cx: &App) -> Option<String> {
    let view = visible_terminal(workspace, cx)?;
    let lines = view.read(cx).terminal().read(cx).last_n_non_empty_lines(TERMINAL_LINES);
    Some(lines.join("\n"))
}

gpui::actions!(forge_agent, [
    /// Asks the agent about the terminal's output (the selection, or the last lines).
    AskAboutTerminal
]);

const ASK_LINES: usize = 80;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|ws, _: &AskAboutTerminal, window, cx| {
            let Some(prompt) = terminal_prompt(ws, window, cx) else { return };
            window.dispatch_action(Box::new(forge_ui::AskAgent { prompt }), cx);
        });
    })
    .detach();
}

/// ctrl-enter in a terminal, like "Ask About This Code" in an editor (Zed uses the key for
/// its inline assistant). Bound after the default keymap.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([gpui::KeyBinding::new("ctrl-enter", AskAboutTerminal, Some("Terminal"))]);
}

fn terminal_prompt(workspace: &Workspace, window: &Window, cx: &App) -> Option<String> {
    // The terminal panel's when it has the focus.
    let panel_terminal = workspace
        .panel::<TerminalPanel>(cx)
        .filter(|panel| gpui::Focusable::focus_handle(panel.read(cx), cx).contains_focused(window, cx))
        .and_then(|panel| panel.read(cx).panes().into_iter().find_map(|pane| pane.read(cx).active_item()?.downcast::<TerminalView>()));
    let view = panel_terminal.or_else(|| visible_terminal(workspace, cx))?;
    let terminal = view.read(cx).terminal().read(cx);
    let (what, text) = match terminal.last_content.selection_text.clone().filter(|s| !s.trim().is_empty()) {
        Some(selection) => ("this part of my terminal's output", selection),
        None => ("the end of my terminal's output", terminal.last_n_non_empty_lines(ASK_LINES).join("\n")),
    };
    (!text.trim().is_empty()).then(|| {
        format!("Here is {what}. Explain what happened. If something failed, find the cause and fix it, in the project if it is there.\n\n```\n{}\n```", text.trim_end())
    })
}

/// Every error and warning the language servers report, as `path:line: severity: message`.
fn problems(project: Entity<Project>, cx: &mut App) -> Task<String> {
    let paths: Vec<_> = project
        .read(cx)
        .diagnostic_summaries(false, cx)
        .filter(|(_, _, s)| s.error_count + s.warning_count > 0)
        .map(|(path, _, _)| path)
        .collect();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let mut lines = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for path in paths {
            if !seen.insert(path.clone()) || lines.len() >= MAX_PROBLEMS {
                continue;
            }
            let Ok(buffer) = project.update(cx, |p, cx| p.open_buffer(path.clone(), cx)).await else { continue };
            let snapshot = cx.update(|cx| buffer.read(cx).snapshot());
            let name = path.path.as_unix_str().to_string();
            for entry in snapshot.diagnostics_in_range::<_, Point>(0..snapshot.len(), false) {
                let severity = match entry.diagnostic.severity {
                    DiagnosticSeverity::ERROR => "error",
                    DiagnosticSeverity::WARNING => "warning",
                    _ => continue,
                };
                let message = entry.diagnostic.message.as_ref().lines().next().unwrap_or_default();
                lines.push(format!("{name}:{}: {severity}: {message}", entry.range.start.row + 1));
                if lines.len() >= MAX_PROBLEMS {
                    break;
                }
            }
        }
        lines.join("\n")
    })
}

/// `git diff HEAD` in `root` (staged and unstaged changes), cut at a reasonable size.
fn git_diff(root: PathBuf, cx: &mut App) -> Task<String> {
    cx.background_spawn(async move {
        let output = ide_api::std_command("git").args(["diff", "HEAD", "--no-color"]).current_dir(&root).output();
        match output {
            Ok(out) if out.status.success() => {
                // Without the files kept from agents (secrets, `.forge/agentignore`).
                let mut text = crate::agent_ignore::filter_diff(&String::from_utf8_lossy(&out.stdout), &crate::agent_ignore::AgentIgnore::load(&root));
                if text.len() > MAX_DIFF_BYTES {
                    let mut cut = MAX_DIFF_BYTES;
                    while !text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    text.truncate(cut);
                    text.push_str("\n… (cut)");
                }
                text
            }
            Ok(out) => format!("git diff failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
            Err(e) => format!("git diff failed: {e}"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn gathers_the_diff(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| std::process::Command::new("git").args(args).current_dir(dir.path()).output().unwrap();
        git(&["init", "-q"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "start"]);
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        git(&["add", "a.txt"]);
        let blocks = cx
            .update(|cx| gather(&["diff".into(), "terminal".into()], WeakEntity::new_invalid(), WeakEntity::new_invalid(), dir.path().to_path_buf(), cx))
            .await;
        let text: Vec<&str> = blocks.iter().map(|b| b["text"].as_str().unwrap()).collect();
        assert!(text[0].starts_with("<diff>") && text[0].contains("+hello"), "{}", text[0]);
        assert_eq!(text[1], "<terminal>\nNo terminal is open.\n</terminal>");
    }
}
