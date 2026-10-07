//! Fix all occurrences, as Visual Studio offers them: on a C# diagnostic that has a fix,
//! the code actions menu (⌘.) adds *Fix all … in this file / in the project / in the
//! solution*. OmniSharp does the work (its `o#/getfixall` and `o#/runfixall` endpoints,
//! asked not to apply anything); Forge applies the changes to the buffers, which Zed then
//! shows in one tab to review before saving.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use editor::{CodeActionProvider, Editor};
use gpui::{App, AppContext as _, Entity, Task, WeakEntity, Window};
use language::{Buffer, PointUtf16};
use lsp::LanguageServer;
use project::{CodeAction, LspAction, Project, ProjectTransaction};
use serde::{Deserialize, Serialize};

/// A fix-all across a solution can take a while.
const RUN_TIMEOUT: Duration = Duration::from_secs(180);
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, window, cx| {
        let (Some(window), Some(project)) = (window, editor.project().cloned()) else { return };
        if !editor.mode().is_full() {
            return;
        }
        editor.push_code_action_provider(Rc::new(FixAll { project: project.downgrade() }), window, cx);
    })
    .detach();
}

/// OmniSharp's `FixAllScope`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "u8", try_from = "u8")]
pub enum Scope {
    Document = 0,
    Project = 1,
    Solution = 2,
}

impl From<Scope> for u8 {
    fn from(scope: Scope) -> u8 {
        scope as u8
    }
}

impl TryFrom<u8> for Scope {
    type Error = String;
    fn try_from(n: u8) -> Result<Self, String> {
        match n {
            0 => Ok(Scope::Document),
            1 => Ok(Scope::Project),
            2 => Ok(Scope::Solution),
            _ => Err(format!("unknown fix-all scope {n}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FixAllItem {
    pub id: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GetFixAllParams {
    file_name: PathBuf,
    scope: Scope,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GetFixAllResponse {
    #[serde(default)]
    items: Vec<FixAllItem>,
}

enum GetFixAll {}

impl lsp::request::Request for GetFixAll {
    type Params = GetFixAllParams;
    type Result = Option<GetFixAllResponse>;
    const METHOD: &'static str = "o#/getfixall";
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RunFixAllParams {
    file_name: PathBuf,
    scope: Scope,
    fix_all_filter: Vec<FixAllItem>,
    timeout: u64,
    wants_text_changes: bool,
    /// Forge applies the changes itself, to the buffers.
    apply_changes: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RunFixAllResponse {
    #[serde(default)]
    changes: Vec<FileChange>,
}

/// A file the fix changes: its edits (zero-based lines and UTF-16 columns, as OmniSharp
/// sends them over LSP), or its whole new text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FileChange {
    pub file_name: PathBuf,
    #[serde(default)]
    pub buffer: Option<String>,
    #[serde(default)]
    pub changes: Option<Vec<TextChange>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct TextChange {
    #[serde(default)]
    pub new_text: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

enum RunFixAll {}

impl lsp::request::Request for RunFixAll {
    type Params = RunFixAllParams;
    type Result = Option<RunFixAllResponse>;
    const METHOD: &'static str = "o#/runfixall";
}

/// The fixable diagnostics among those at the cursor (`here`, their codes), each once.
/// OmniSharp also lists internal ids without a message (`RemoveUnnecessaryImportsFixable`
/// next to `CS8019`); those are left out.
pub fn offered(fixable: &[FixAllItem], here: &[String]) -> Vec<FixAllItem> {
    fixable.iter().filter(|item| !item.message.trim().is_empty() && here.contains(&item.id)).cloned().collect()
}

/// "Fix all “Unnecessary using directive” (CS8019) in the project App".
pub fn title(item: &FixAllItem, scope: Scope, project: Option<&str>) -> String {
    let message = item.message.trim().trim_end_matches('.');
    let message: String = if message.chars().count() > 60 { format!("{}…", message.chars().take(60).collect::<String>()) } else { message.to_string() };
    let place = match (scope, project) {
        (Scope::Document, _) => "in this file".to_string(),
        (Scope::Project, Some(name)) => format!("in the project {name}"),
        (Scope::Project, None) => "in the project".to_string(),
        (Scope::Solution, _) => "in the solution".to_string(),
    };
    format!("Fix all “{message}” ({}) {place}", item.id)
}

/// The edits of `change` in `buffer`'s coordinates, in order.
pub fn edits(buffer: &Buffer, change: &FileChange) -> Vec<(Range<PointUtf16>, String)> {
    if let Some(changes) = &change.changes {
        let point = |line, column| buffer.clip_point_utf16(language::Unclipped(PointUtf16::new(line, column)), text::Bias::Left);
        let mut edits: Vec<_> = changes.iter().map(|c| (point(c.start_line, c.start_column)..point(c.end_line, c.end_column), c.new_text.clone())).collect();
        edits.sort_by_key(|(range, _)| range.start);
        return edits;
    }
    match &change.buffer {
        Some(text) if *text != buffer.text() => vec![(PointUtf16::zero()..buffer.max_point_utf16(), text.clone())],
        _ => vec![],
    }
}

/// The name of the C# project `file` is in: the nearest `.csproj` up.
fn project_name(file: &Path) -> Option<String> {
    file.ancestors().skip(1).find_map(|dir| {
        std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "csproj")).and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
    })
}

struct FixAll {
    project: WeakEntity<Project>,
}

fn omnisharp(project: &Entity<Project>, cx: &App) -> Option<Arc<LanguageServer>> {
    crate::csharp_metadata::omnisharp_servers(project, cx).into_iter().next()
}

impl CodeActionProvider for FixAll {
    fn id(&self) -> Arc<str> {
        "forge-csharp-fix-all".into()
    }

    fn code_actions(&self, buffer: &Entity<Buffer>, range: Range<text::Anchor>, _: &mut Window, cx: &mut App) -> Task<Result<Vec<CodeAction>>> {
        let Some(project) = self.project.upgrade() else { return Task::ready(Ok(vec![])) };
        let snapshot = buffer.read(cx).snapshot();
        let Some(path) = buffer.read(cx).file().and_then(|f| f.as_local()).map(|f| f.abs_path(cx)).filter(|p| p.extension().is_some_and(|x| x == "cs")) else {
            return Task::ready(Ok(vec![]));
        };
        // Only where OmniSharp reported something: asking costs it an analysis of the file.
        let here: Vec<String> = snapshot
            .diagnostics_in_range::<_, usize>(range.clone(), false)
            .filter(|d| d.diagnostic.source.as_deref() == Some("csharp"))
            .filter_map(|d| match d.diagnostic.code.as_ref()? {
                lsp::NumberOrString::String(code) => Some(code.clone()),
                lsp::NumberOrString::Number(n) => Some(n.to_string()),
            })
            .collect();
        if here.is_empty() {
            return Task::ready(Ok(vec![]));
        }
        let Some(server) = omnisharp(&project, cx) else { return Task::ready(Ok(vec![])) };
        cx.background_spawn(async move {
            let response = server.request::<GetFixAll>(GetFixAllParams { file_name: path.clone(), scope: Scope::Document }, LIST_TIMEOUT).await.into_response()?;
            let items = offered(&response.map(|r| r.items).unwrap_or_default(), &here);
            let project = project_name(&path);
            let mut actions = Vec::new();
            for item in items {
                for scope in [Scope::Document, Scope::Project, Scope::Solution] {
                    let data = serde_json::json!({ "file": path, "scope": scope, "item": item });
                    actions.push(CodeAction {
                        server_id: lsp::LanguageServerId(usize::MAX - 1),
                        range: range.clone(),
                        lsp_action: LspAction::Action(Box::new(lsp::CodeAction {
                            title: title(&item, scope, project.as_deref()),
                            kind: Some(lsp::CodeActionKind::QUICKFIX),
                            data: Some(data),
                            ..Default::default()
                        })),
                        resolved: true,
                    });
                }
            }
            Ok(actions)
        })
    }

    fn apply_code_action(&self, _: Entity<Buffer>, action: CodeAction, push_to_history: bool, _: &mut Window, cx: &mut App) -> Task<Result<ProjectTransaction>> {
        let Some(project) = self.project.upgrade() else { return Task::ready(Err(anyhow!("the project is gone"))) };
        let LspAction::Action(lsp_action) = &action.lsp_action else { return Task::ready(Ok(ProjectTransaction::default())) };
        let data = lsp_action.data.clone().unwrap_or_default();
        let parsed = (|| -> Result<(PathBuf, Scope, FixAllItem)> {
            Ok((serde_json::from_value(data["file"].clone())?, serde_json::from_value(data["scope"].clone())?, serde_json::from_value(data["item"].clone())?))
        })();
        let (file, scope, item) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => return Task::ready(Err(error)),
        };
        let Some(server) = omnisharp(&project, cx) else { return Task::ready(Err(anyhow!("OmniSharp is not running"))) };
        cx.spawn(async move |cx| {
            let params = RunFixAllParams {
                file_name: file,
                scope,
                fix_all_filter: vec![item.clone()],
                timeout: RUN_TIMEOUT.as_millis() as u64,
                wants_text_changes: true,
                apply_changes: false,
            };
            let response = server.request::<RunFixAll>(params, RUN_TIMEOUT).await.into_response().with_context(|| format!("OmniSharp could not fix all {}", item.id))?;
            let mut transaction = ProjectTransaction::default();
            for change in response.map(|r| r.changes).unwrap_or_default() {
                let buffer = project.update(cx, |p, cx| p.open_local_buffer(&change.file_name, cx)).await.with_context(|| format!("cannot open {}", change.file_name.display()))?;
                let edited = buffer.update(cx, |buffer, cx| {
                    let edits = edits(buffer, &change);
                    if edits.is_empty() {
                        return None;
                    }
                    buffer.finalize_last_transaction();
                    buffer.start_transaction();
                    buffer.edit(edits, None, cx);
                    let id = buffer.end_transaction(cx)?;
                    let transaction = buffer.get_transaction(id).cloned();
                    if !push_to_history {
                        buffer.forget_transaction(id);
                    }
                    transaction
                });
                if let Some(edited) = edited {
                    transaction.0.insert(buffer, edited);
                }
            }
            Ok(transaction)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn item(id: &str, message: &str) -> FixAllItem {
        FixAllItem { id: id.into(), message: message.into() }
    }

    #[test]
    fn offers_the_fixable_diagnostics_at_the_cursor() {
        // What OmniSharp answered for a file with unused usings (`o#/getfixall`).
        let fixable = [item("CS8019", "Unnecessary using directive."), item("IDE0040", "Accessibility modifiers required"), item("RemoveUnnecessaryImportsFixable", "")];
        let here = ["IDE0005".to_string(), "CS8019".to_string(), "RemoveUnnecessaryImportsFixable".to_string()];
        assert_eq!(offered(&fixable, &here), [item("CS8019", "Unnecessary using directive.")]);
        assert!(offered(&fixable, &["CS0168".into()]).is_empty());
    }

    #[test]
    fn titles_say_what_and_where() {
        let unused = item("CS8019", "Unnecessary using directive.");
        assert_eq!(title(&unused, Scope::Document, Some("App")), "Fix all “Unnecessary using directive” (CS8019) in this file");
        assert_eq!(title(&unused, Scope::Project, Some("App")), "Fix all “Unnecessary using directive” (CS8019) in the project App");
        assert_eq!(title(&unused, Scope::Solution, None), "Fix all “Unnecessary using directive” (CS8019) in the solution");
        assert!(title(&item("X", &"m".repeat(100)), Scope::Document, None).contains(&format!("{}…", "m".repeat(60))));
    }

    #[test]
    fn reads_omnisharps_answer() {
        // `o#/runfixall` for the project, as OmniSharp 2.0 sends it.
        let response: RunFixAllResponse = serde_json::from_value(serde_json::json!({ "Changes": [
            { "Buffer": null, "Changes": [{ "NewText": "", "StartLine": 0, "StartColumn": 0, "EndLine": 3, "EndColumn": 0 }], "FileName": "/p/App/Lib.cs", "ModificationType": 0 },
            { "Buffer": "whole\n", "Changes": null, "FileName": "/p/App/B.cs", "ModificationType": 0 }
        ]}))
        .unwrap();
        assert_eq!(response.changes.len(), 2);
        assert_eq!(response.changes[0].changes.as_ref().unwrap()[0].end_line, 3);
        let params = serde_json::to_value(RunFixAllParams { file_name: "/p/App/Program.cs".into(), scope: Scope::Project, fix_all_filter: vec![item("CS8019", "m")], timeout: 1, wants_text_changes: true, apply_changes: false }).unwrap();
        assert_eq!(params["Scope"], 1);
        assert_eq!(params["FixAllFilter"][0]["Id"], "CS8019");
        assert_eq!(params["ApplyChanges"], false);
    }

    #[gpui::test]
    fn applies_the_changes_to_a_buffer(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("using System.Text;\nusing System.Linq;\n\nclass Lib { /* é */ int x; }\n", cx));
        let change = FileChange {
            file_name: "/p/Lib.cs".into(),
            buffer: None,
            changes: Some(vec![
                // Out of order on purpose; columns count UTF-16 units.
                TextChange { new_text: "long".into(), start_line: 3, start_column: 20, end_line: 3, end_column: 23 },
                TextChange { new_text: String::new(), start_line: 0, start_column: 0, end_line: 3, end_column: 0 },
            ]),
        };
        buffer.update(cx, |buffer, cx| {
            let edits = edits(buffer, &change);
            buffer.edit(edits, None, cx);
            assert_eq!(buffer.text(), "class Lib { /* é */ long x; }\n");
        });
        let whole = FileChange { file_name: "/p/Lib.cs".into(), buffer: Some("new\n".into()), changes: None };
        buffer.update(cx, |buffer, cx| {
            let edits = edits(buffer, &whole);
            buffer.edit(edits, None, cx);
            assert_eq!(buffer.text(), "new\n");
            assert!(super::edits(buffer, &whole).is_empty(), "nothing to do when it is the same");
        });
    }
}
