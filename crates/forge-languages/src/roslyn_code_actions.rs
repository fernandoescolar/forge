//! Roslyn's code actions that its client has to carry out.
//!
//! Roslyn offers some actions as commands for the client rather than edits: a group of
//! actions to choose from (`roslyn.client.nestedCodeAction`, e.g. *Introduce constant*,
//! *Suppress or configure issues*) and *Fix all* (`roslyn.client.fixAllCodeAction`), whose
//! scope the client asks for before asking the server for the edit. It doesn't list these
//! commands as ones it runs, so Zed would drop them.
//!
//! So the editors' code actions go through [`RoslynCodeActions`], which stands in for the
//! project's own source of actions (same id) and passes everything else through: a group
//! becomes one entry per action in it (`Introduce constant › …`), resolved and applied like
//! any action, and *Fix all* one entry per scope (`… in the project`), whose edit comes from
//! `codeAction/resolveFixAll`.

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use editor::{CodeActionProvider, Editor};
use gpui::{App, AppContext as _, Entity, Task, WeakEntity, Window};
use language::Buffer;
use lsp::LanguageServerId;
use project::{CodeAction, LspAction, Project, ProjectTransaction};
use serde_json::Value;

const NESTED: &str = "roslyn.client.nestedCodeAction";
const FIX_ALL: &str = "roslyn.client.fixAllCodeAction";
/// Marks the entries Forge makes for each scope of a *Fix all*.
const FIX_ALL_IN_SCOPE: &str = "forge.roslyn.fixAllInScope";
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(180);

enum ResolveFixAll {}

impl lsp::request::Request for ResolveFixAll {
    type Params = Value;
    type Result = lsp::CodeAction;
    const METHOD: &'static str = "codeAction/resolveFixAll";
}

/// Puts [`RoslynCodeActions`] in every editor of a project.
pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, window, cx| {
        let (Some(window), Some(project)) = (window, editor.project().cloned()) else { return };
        if !editor.mode().is_full() {
            return;
        }
        editor.push_code_action_provider(Rc::new(RoslynCodeActions { project: project.downgrade() }), window, cx);
    })
    .detach();
}

pub struct RoslynCodeActions {
    project: WeakEntity<Project>,
}

impl CodeActionProvider for RoslynCodeActions {
    /// The project's own id, so that this replaces it.
    fn id(&self) -> Arc<str> {
        "project".into()
    }

    fn code_actions(&self, buffer: &Entity<Buffer>, range: Range<text::Anchor>, window: &mut Window, cx: &mut App) -> Task<Result<Vec<CodeAction>>> {
        let Some(project) = self.project.upgrade() else { return Task::ready(Ok(Vec::new())) };
        let roslyn = roslyn_servers(&project, cx);
        let actions = project.code_actions(buffer, range, window, cx);
        if roslyn.is_empty() {
            return actions;
        }
        cx.background_spawn(async move { Ok(expand(actions.await?, &roslyn)) })
    }

    fn apply_code_action(&self, buffer: Entity<Buffer>, action: CodeAction, push_to_history: bool, window: &mut Window, cx: &mut App) -> Task<Result<ProjectTransaction>> {
        let Some(project) = self.project.upgrade() else { return Task::ready(Ok(ProjectTransaction::default())) };
        let Some(command) = action.lsp_action.command().filter(|c| c.command == FIX_ALL_IN_SCOPE).cloned() else {
            return project.apply_code_action(buffer, action, push_to_history, window, cx);
        };
        let server = project.read(cx).lsp_store().read(cx).language_server_for_id(action.server_id);
        let Some(server) = server else { return Task::ready(Err(anyhow::anyhow!("the C# language server is not running"))) };
        let params = command.arguments.clone().and_then(|mut args| args.pop()).unwrap_or(Value::Null);
        let window_handle = window.window_handle();
        cx.spawn(async move |cx| {
            let resolved = server.request::<ResolveFixAll>(params, RESOLVE_TIMEOUT).await.into_response().context("fixing all occurrences")?;
            let action = CodeAction { server_id: action.server_id, range: action.range, lsp_action: LspAction::Action(Box::new(resolved)), resolved: true };
            let apply = cx.update_window(window_handle, |_, window, cx| project.apply_code_action(buffer, action, push_to_history, window, cx))?;
            apply.await
        })
    }
}

/// The Roslyn servers running for `project`.
fn roslyn_servers(project: &Entity<Project>, cx: &App) -> Vec<LanguageServerId> {
    project.read(cx).language_server_statuses(cx).filter(|(_, status)| status.name.0.as_ref() == crate::roslyn::SERVER_NAME.0.as_ref()).map(|(id, _)| id).collect()
}

/// Roslyn's client-side actions turned into ones Zed can apply; the rest as they are.
fn expand(actions: Vec<CodeAction>, roslyn: &[LanguageServerId]) -> Vec<CodeAction> {
    let mut expanded = Vec::new();
    for action in actions {
        if !roslyn.contains(&action.server_id) {
            expanded.push(action);
            continue;
        }
        let command = action.lsp_action.command().cloned();
        let argument = command.as_ref().and_then(|c| c.arguments.as_ref()?.first().cloned());
        match (command.as_ref().map(|c| c.command.as_str()), argument) {
            (Some(NESTED), Some(argument)) => expanded.extend(nested(&action, action.lsp_action.title(), &argument, roslyn)),
            (Some(FIX_ALL), Some(argument)) => expanded.extend(fix_all(&action, command.as_ref().map_or(action.lsp_action.title(), |c| c.title.as_str()), &argument)),
            _ => expanded.push(action),
        }
    }
    expanded
}

/// The actions of a group, titled `Group › Action`, to be resolved when chosen.
fn nested(parent: &CodeAction, title: &str, argument: &Value, roslyn: &[LanguageServerId]) -> Vec<CodeAction> {
    let children = argument.get("NestedCodeActions").and_then(Value::as_array).cloned().unwrap_or_default();
    let children: Vec<CodeAction> = children
        .into_iter()
        .filter_map(|child| serde_json::from_value::<lsp::CodeAction>(child).ok())
        .map(|mut child| {
            child.title = format!("{title} › {}", child.title);
            CodeAction { server_id: parent.server_id, range: parent.range.clone(), lsp_action: LspAction::Action(Box::new(child)), resolved: false }
        })
        .collect();
    // A group inside the group opens up the same way.
    expand(children, roslyn)
}

/// One entry per scope Roslyn can fix all occurrences in.
fn fix_all(parent: &CodeAction, title: &str, argument: &Value) -> Vec<CodeAction> {
    let flavors: Vec<String> = argument.get("FixAllFlavors").and_then(Value::as_array).map(|f| f.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    flavors
        .into_iter()
        .map(|scope| {
            let label = format!("{title} {}", scope_label(&scope));
            let params = serde_json::json!({ "title": title, "data": argument, "scope": scope });
            let command = lsp::Command { title: label.clone(), command: FIX_ALL_IN_SCOPE.into(), arguments: Some(vec![params]) };
            CodeAction {
                server_id: parent.server_id,
                range: parent.range.clone(),
                lsp_action: LspAction::Action(Box::new(lsp::CodeAction { title: label, kind: Some(lsp::CodeActionKind::QUICKFIX), command: Some(command), ..Default::default() })),
                resolved: true,
            }
        })
        .collect()
}

fn scope_label(scope: &str) -> String {
    match scope {
        "Document" => "in this file".into(),
        "Project" => "in the project".into(),
        "Solution" => "in the solution".into(),
        "ContainingMember" => "in this member".into(),
        "ContainingType" => "in this type".into(),
        other => format!("in {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn action(server: usize, title: &str, command: Option<(&str, Value)>) -> CodeAction {
        let range = text::Anchor::min_for_buffer(text::BufferId::new(1).unwrap())..text::Anchor::max_for_buffer(text::BufferId::new(1).unwrap());
        CodeAction {
            server_id: LanguageServerId(server),
            range,
            lsp_action: LspAction::Action(Box::new(lsp::CodeAction {
                title: title.into(),
                command: command.map(|(name, argument)| lsp::Command { title: title.into(), command: name.into(), arguments: Some(vec![argument]) }),
                data: Some(json!({})),
                ..Default::default()
            })),
            resolved: false,
        }
    }

    #[test]
    fn opens_groups_and_fix_all_scopes() {
        let group = json!({ "NestedCodeActions": [
            { "title": "Introduce local constant for '1'", "data": { "UniqueIdentifier": "a" } },
            { "title": "Introduce local constant for all occurrences of '1'", "data": { "UniqueIdentifier": "b" } },
        ] });
        let fix_all = json!({ "FixAllFlavors": ["Document", "Project", "Solution"], "UniqueIdentifier": "Fix All: Remove unused variable" });
        let actions = vec![
            action(1, "Remove unused variable", None),
            action(1, "Fix All: Remove unused variable", Some((FIX_ALL, fix_all.clone()))),
            action(1, "Introduce constant", Some((NESTED, group))),
            action(2, "Other server's group", Some((NESTED, json!({})))),
        ];
        let expanded = expand(actions, &[LanguageServerId(1)]);
        let titles: Vec<&str> = expanded.iter().map(|a| a.lsp_action.title()).collect();
        assert_eq!(
            titles,
            [
                "Remove unused variable",
                "Fix All: Remove unused variable in this file",
                "Fix All: Remove unused variable in the project",
                "Fix All: Remove unused variable in the solution",
                "Introduce constant › Introduce local constant for '1'",
                "Introduce constant › Introduce local constant for all occurrences of '1'",
                "Other server's group",
            ]
        );
        assert!(!expanded[4].resolved, "the group's actions resolve when chosen");
        let command = expanded[2].lsp_action.command().unwrap();
        assert_eq!(command.command, FIX_ALL_IN_SCOPE);
        assert_eq!(command.arguments.as_ref().unwrap()[0], json!({ "title": "Fix All: Remove unused variable", "data": fix_all, "scope": "Project" }));
    }
}
