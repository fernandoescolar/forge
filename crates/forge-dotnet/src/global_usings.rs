//! "Move usings to GlobalUsings.cs": a refactoring in the code actions menu (⌘.) of C#
//! files that turns the `using` directives at the top of the file (or of every file of its
//! project) into `global using` directives in one file at the root of the project. Which
//! file is `globalUsingsFile` in dotnet.json (`GlobalUsings.cs` unless set; `_Imports.cs`,
//! `Properties/Usings.cs`…).
//!
//! Only the usings at the top of a file move: those inside a namespace block, after a file
//! scoped `namespace`, or behind `#if` keep their meaning only where they are, so the
//! header ends there. `global using` lines are left alone.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use editor::{CodeActionProvider, Editor};
use gpui::{App, AppContext as _, Entity, Task, WeakEntity, Window};
use language::Buffer;
use text::ToOffset as _;
use project::{CodeAction, LspAction, Project, ProjectTransaction};

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, window, cx| {
        let (Some(window), Some(project)) = (window, editor.project().cloned()) else { return };
        if !editor.mode().is_full() {
            return;
        }
        editor.push_code_action_provider(Rc::new(GlobalUsings { project: project.downgrade() }), window, cx);
    })
    .detach();
}

/// A `using` directive at the top of a file, ready to become a `global using`.
#[derive(Clone, Debug, PartialEq)]
pub struct Moved {
    /// The bytes to delete: the line (and blank lines after the block, for the last one).
    pub range: Range<usize>,
    /// What follows `using`, without the `;`: `System.Text`, `static System.Math`,
    /// `Json = System.Text.Json`.
    pub directive: String,
}

/// The `using` directives of the file's header that can become global, in order.
pub fn movable_usings(text: &str) -> Vec<Moved> {
    let mut moved = Vec::new();
    let mut offset = 0;
    let mut in_comment = false;
    // Whether only moved usings and blank lines came before: then the blank lines after
    // the block go too, so the file doesn't start with them.
    let mut only_usings = true;
    let mut trailing_blanks: Option<usize> = None;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let trimmed = line.trim_start_matches('\u{feff}').trim();
        if in_comment {
            in_comment = !trimmed.contains("*/");
            only_usings = false;
            continue;
        }
        if trimmed.is_empty() {
            if trailing_blanks.is_none() && !moved.is_empty() {
                trailing_blanks = Some(start);
            }
            continue;
        }
        if let Some(directive) = using_directive(trimmed) {
            moved.push(Moved { range: start..offset, directive });
            trailing_blanks = None;
            continue;
        }
        if trimmed.starts_with("/*") {
            in_comment = !trimmed.contains("*/");
        } else if !(trimmed.starts_with("//") || trimmed.starts_with("global using") || trimmed.starts_with("extern alias") || is_harmless_directive(trimmed)) {
            // The header ends: a namespace, a type, an attribute, `#if`…
            if only_usings && let (Some(blanks), Some(last)) = (trailing_blanks, moved.last_mut()) {
                last.range.end = start.max(blanks);
            }
            return moved;
        }
        only_usings = false;
        trailing_blanks = None;
    }
    moved
}

/// `#nullable`, `#pragma` and `#region` lines don't change what a using means.
fn is_harmless_directive(line: &str) -> bool {
    ["#nullable", "#pragma", "#region", "#endregion"].iter().any(|d| line.starts_with(d))
}

/// `using X;` → `X`; `None` for anything else (`using var`, `using (…)`, `global using`).
fn using_directive(line: &str) -> Option<String> {
    let rest = line.strip_prefix("using")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let code = rest.split("//").next().unwrap_or(rest).trim();
    let body = code.strip_suffix(';')?.trim();
    let first = body.split_whitespace().next()?;
    if body.is_empty() || body.contains(['(', '{', ';']) || matches!(first, "var" | "await") {
        return None;
    }
    Some(body.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Where the `global using` lines for `add` go in the global usings file (`existing`,
/// `None` when it doesn't exist yet): `(offset, text)` to insert, or `None` when it has
/// them all already. New ones go after its last `global using`, `System` first.
pub fn merge_global_usings(existing: Option<&str>, add: &[String]) -> Option<(usize, String)> {
    let text = existing.unwrap_or_default();
    let mut present: Vec<String> = Vec::new();
    let mut insert_at = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        offset += line.len();
        if let Some(directive) = line.trim().strip_prefix("global").and_then(|rest| using_directive(rest.trim_start())) {
            present.push(directive);
            insert_at = Some(offset);
        }
    }
    let mut new: Vec<String> = Vec::new();
    for directive in add {
        if !present.contains(directive) && !new.contains(directive) {
            new.push(directive.clone());
        }
    }
    if new.is_empty() {
        return None;
    }
    new.sort_by_key(|d| sort_key(d));
    let lines: String = new.iter().map(|d| format!("global using {d};\n")).collect();
    if text.trim().is_empty() {
        return Some((0, lines));
    }
    Some(match insert_at {
        // After the last global using (which may lack its newline).
        Some(at) if at == text.len() && !text.ends_with('\n') => (at, format!("\n{lines}")),
        Some(at) => (at, lines),
        // After the rest of the file, a blank line between.
        None if text.ends_with('\n') => (text.len(), format!("\n{lines}")),
        None => (text.len(), format!("\n\n{lines}")),
    })
}

/// `System…` first, then the rest alphabetically; `static` ones and aliases after.
fn sort_key(directive: &str) -> (u8, bool, String) {
    let group = if directive.starts_with("static ") {
        2
    } else if directive.contains(" = ") {
        3
    } else {
        1
    };
    let name = directive.trim_start_matches("static ");
    let system = name == "System" || name.starts_with("System.");
    (group, !system, name.to_lowercase())
}

/// The folder of the C# project `file` belongs to: the nearest one up with a `.csproj`.
pub fn project_dir(file: &Path) -> Option<PathBuf> {
    file.ancestors().skip(1).find(|dir| std::fs::read_dir(dir).is_ok_and(|mut entries| entries.any(|e| e.is_ok_and(|e| e.path().extension().is_some_and(|x| x == "csproj"))))).map(Path::to_path_buf)
}

/// The project's `.csproj` name, for labels.
fn project_name(dir: &Path) -> String {
    std::fs::read_dir(dir)
        .ok()
        .and_then(|entries| entries.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "csproj")))
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "the project".into())
}

/// The project's C# files (not under bin, obj or an ignored folder, nor another project's).
fn project_files(dir: &Path, ignored: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(folder) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else { continue };
        let entries: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        if folder != dir && entries.iter().any(|p| p.extension().is_some_and(|x| x == "csproj")) {
            continue;
        }
        for path in entries {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if path.is_dir() {
                let skip = name.starts_with('.') || ["bin", "obj", "node_modules"].contains(&name.as_str()) || ignored.iter().any(|i| i.eq_ignore_ascii_case(&name));
                if !skip {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|x| x == "cs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

struct GlobalUsings {
    project: WeakEntity<Project>,
}

const ID: &str = "forge-global-usings";

fn code_action(title: String, range: Range<text::Anchor>, data: serde_json::Value) -> CodeAction {
    CodeAction {
        server_id: lsp::LanguageServerId(usize::MAX),
        range,
        lsp_action: LspAction::Action(Box::new(lsp::CodeAction {
            title,
            kind: Some(lsp::CodeActionKind::REFACTOR_REWRITE),
            data: Some(data),
            ..Default::default()
        })),
        resolved: true,
    }
}

impl CodeActionProvider for GlobalUsings {
    fn id(&self) -> Arc<str> {
        ID.into()
    }

    fn code_actions(&self, buffer: &Entity<Buffer>, range: Range<text::Anchor>, _: &mut Window, cx: &mut App) -> Task<Result<Vec<CodeAction>>> {
        let buffer = buffer.read(cx);
        let Some(path) = buffer.file().and_then(|f| f.as_local()).map(|f| f.abs_path(cx)).filter(|p| p.extension().is_some_and(|x| x == "cs")) else {
            return Task::ready(Ok(vec![]));
        };
        let snapshot = buffer.snapshot();
        // This runs whenever the cursor stops: the header is enough, not the whole file.
        let head: String = snapshot.text_for_range(0..snapshot.len().min(64 * 1024)).collect();
        let moved = movable_usings(&head);
        let (Some(first), Some(last)) = (moved.first(), moved.last()) else { return Task::ready(Ok(vec![])) };
        // Offered on the usings, not everywhere in the file.
        let (asked_start, asked_end) = (range.start.to_offset(&snapshot), range.end.to_offset(&snapshot));
        if asked_start > last.range.end || asked_end < first.range.start {
            return Task::ready(Ok(vec![]));
        }
        let file_name = crate::config::get(cx).global_usings_file;
        cx.background_spawn(async move {
            let Some(dir) = project_dir(&path) else { return Ok(vec![]) };
            let target = dir.join(&file_name);
            if target == path {
                return Ok(vec![]);
            }
            let shown = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(file_name);
            let data = |scope: &str| serde_json::json!({ "scope": scope, "project": dir, "target": target });
            Ok(vec![
                code_action(format!("Move usings to {shown}"), range.clone(), data("file")),
                code_action(format!("Move usings to {shown} in every file of {}", project_name(&dir)), range, data("project")),
            ])
        })
    }

    fn apply_code_action(&self, buffer: Entity<Buffer>, action: CodeAction, push_to_history: bool, _: &mut Window, cx: &mut App) -> Task<Result<ProjectTransaction>> {
        let Some(project) = self.project.upgrade() else { return Task::ready(Err(anyhow::anyhow!("the project is gone"))) };
        let LspAction::Action(lsp_action) = &action.lsp_action else { return Task::ready(Ok(ProjectTransaction::default())) };
        let data = lsp_action.data.clone().unwrap_or_default();
        let whole_project = data["scope"] == "project";
        let dir = PathBuf::from(data["project"].as_str().unwrap_or_default());
        let target = PathBuf::from(data["target"].as_str().unwrap_or_default());
        move_usings(project, Some(buffer), whole_project.then_some(dir), target, push_to_history, cx)
    }
}

/// Moves the usings of `buffer` and, with `project_dir`, of every C# file of that project,
/// into `target` as global usings. The buffers are edited, not saved.
pub fn move_usings(project: Entity<Project>, buffer: Option<Entity<Buffer>>, project_dir: Option<PathBuf>, target: PathBuf, push_to_history: bool, cx: &mut App) -> Task<Result<ProjectTransaction>> {
    let ignored = crate::config::get(cx).ignored_folders;
    cx.spawn(async move |cx| {
        // The files to change: this one, or every file of the project that has usings to move.
        let this = buffer.as_ref().and_then(|b| b.read_with(cx, |b, cx| b.file().and_then(|f| f.as_local()).map(|f| f.abs_path(cx))));
        let mut buffers: Vec<Entity<Buffer>> = buffer.into_iter().collect();
        if let Some(dir) = project_dir {
            let target = target.clone();
            let others = cx
                .background_spawn(async move {
                    project_files(&dir, &ignored)
                        .into_iter()
                        .filter(|p| Some(p) != this.as_ref() && *p != target)
                        .filter(|p| std::fs::read_to_string(p).is_ok_and(|text| !movable_usings(&text).is_empty()))
                        .collect::<Vec<_>>()
                })
                .await;
            for path in others {
                let open = project.update(cx, |p, cx| p.open_local_buffer(&path, cx));
                buffers.push(open.await.with_context(|| format!("cannot open {}", path.display()))?);
            }
        }
        let mut transaction = ProjectTransaction::default();
        let mut directives = Vec::new();
        for buffer in buffers {
            let edited = buffer.update(cx, |buffer, cx| {
                let moved = movable_usings(&buffer.text());
                directives.extend(moved.iter().map(|m| m.directive.clone()));
                edit(buffer, moved.into_iter().map(|m| (m.range, String::new())).collect(), push_to_history, cx)
            });
            if let Some(edited) = edited {
                transaction.0.insert(buffer, edited);
            }
        }
        if directives.is_empty() {
            return Ok(transaction);
        }
        let global = project.update(cx, |p, cx| p.open_local_buffer(&target, cx)).await.with_context(|| format!("cannot open {}", target.display()))?;
        let added = global.update(cx, |buffer, cx| {
            // A file that doesn't exist yet opens empty; saving creates it.
            let (at, text) = merge_global_usings(Some(&buffer.text()), &directives)?;
            edit(buffer, vec![(at..at, text)], push_to_history, cx)
        });
        if let Some(added) = added {
            transaction.0.insert(global, added);
        }
        Ok(transaction)
    })
}

/// A tab with what `transaction` changed, to review before saving (as Zed shows a code
/// action that changed several files).
pub fn open_review(workspace: &mut workspace::Workspace, transaction: ProjectTransaction, title: String, window: &mut Window, cx: &mut gpui::Context<workspace::Workspace>) {
    let mut entries: Vec<_> = transaction.0.into_iter().collect();
    entries.sort_by_key(|(buffer, _)| buffer.read(cx).file().map(|f| f.path().clone()));
    let multibuffer = cx.new(|cx| {
        let mut multibuffer = editor::MultiBuffer::new(language::Capability::ReadWrite).with_title(title);
        for (buffer, transaction) in &entries {
            let ranges: Vec<_> = buffer.read(cx).edited_ranges_for_transaction::<language::Point>(transaction).collect();
            multibuffer.set_excerpts_for_path(editor::PathKey::for_buffer(buffer, cx), buffer.clone(), ranges, editor::multibuffer_context_lines(cx), cx);
        }
        multibuffer.push_transaction(entries.iter().map(|(b, t)| (b, t)), cx);
        multibuffer
    });
    let project = workspace.project().clone();
    let editor = cx.new(|cx| Editor::for_multibuffer(multibuffer, Some(project), window, cx));
    workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
}

/// Applies `edits` to `buffer` as one transaction (kept in its undo history if asked).
fn edit(buffer: &mut Buffer, edits: Vec<(Range<usize>, String)>, push_to_history: bool, cx: &mut gpui::Context<Buffer>) -> Option<language::Transaction> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moved(text: &str) -> (Vec<String>, String) {
        let moved = movable_usings(text);
        let mut rest = text.to_string();
        for m in moved.iter().rev() {
            rest.replace_range(m.range.clone(), "");
        }
        (moved.into_iter().map(|m| m.directive).collect(), rest)
    }

    #[test]
    fn moves_the_usings_at_the_top() {
        let (directives, rest) = moved("using System;\nusing static System.Math;\nusing Json = System.Text.Json;\n\nnamespace App;\n\npublic class A { }\n");
        assert_eq!(directives, ["System", "static System.Math", "Json = System.Text.Json"]);
        assert_eq!(rest, "namespace App;\n\npublic class A { }\n", "and the blank line after them");
    }

    #[test]
    fn keeps_what_isnt_a_movable_using() {
        let text = "// Copyright\n\n#nullable enable\nusing System; // why\nglobal using Old;\n#if DEBUG\nusing Debug.Only;\n#endif\nnamespace App\n{\n    using Inner;\n}\n";
        let (directives, rest) = moved(text);
        assert_eq!(directives, ["System"], "not after #if, not inside the namespace, not the global one");
        assert_eq!(rest, "// Copyright\n\n#nullable enable\nglobal using Old;\n#if DEBUG\nusing Debug.Only;\n#endif\nnamespace App\n{\n    using Inner;\n}\n");
        assert!(movable_usings("namespace App;\nusing System;\n").is_empty(), "after a file-scoped namespace");
        assert!(movable_usings("using var x = Open();\nusing (var y = Open()) { }\n").is_empty(), "using statements");
        assert_eq!(moved("\u{feff}using System;\r\nclass A {}\r\n").0, ["System"], "BOM and CRLF");
    }

    #[test]
    fn merges_into_the_global_usings_file() {
        let add = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(merge_global_usings(None, &add(&["Zeta", "System.Text", "static System.Math", "A = B.C", "Alpha"])), Some((0, "global using System.Text;\nglobal using Alpha;\nglobal using Zeta;\nglobal using static System.Math;\nglobal using A = B.C;\n".into())), "a new file, sorted");

        let existing = "// Shared\nglobal using System;\nglobal using App.Models;\n";
        assert_eq!(merge_global_usings(Some(existing), &add(&["System", "App.Services"])), Some((existing.len(), "global using App.Services;\n".into())), "only what is missing, after the others");
        assert_eq!(merge_global_usings(Some(existing), &add(&["System", "App.Models"])), None, "nothing to add");
        assert_eq!(merge_global_usings(Some("global using System;"), &add(&["Linq"])), Some((20, "\nglobal using Linq;\n".into())), "no newline at the end");
        assert_eq!(merge_global_usings(Some("// Usings\n"), &add(&["Linq"])), Some((10, "\nglobal using Linq;\n".into())), "after the rest, a blank line between");
    }

    /// Both actions on a project with an `_Imports.cs`: the file's usings, then every file's,
    /// end up there as global usings (each once), in buffers left to review and save.
    #[gpui::test]
    async fn moves_a_files_or_a_projects_usings(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let files = [
            ("App/App.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\" />\n"),
            ("App/Program.cs", "using System;\nusing App.Models;\n\nConsole.WriteLine(new User());\n"),
            ("App/Models/User.cs", "using System;\nusing System.Text;\n\nnamespace App.Models;\n\npublic class User { }\n"),
            ("App/Models/Plain.cs", "namespace App.Models;\n"),
            ("App/_Imports.cs", "global using System.Linq;\n"),
        ];
        // The project lookup reads the disk; buffers come from the project's (fake) fs.
        let fs = fs::FakeFs::new(cx.executor());
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            fs::Fs::create_dir(&*fs, path.parent().unwrap()).await.unwrap();
            fs.insert_file(&path, text.as_bytes().to_vec()).await;
        }
        cx.executor().allow_parking();
        cx.update(|cx| {
            settings::init(cx);
            cx.set_global(crate::config::DotnetConfig { global_usings_file: "_Imports.cs".into(), ..Default::default() });
        });
        let project = Project::test(fs, [root.as_path()], cx).await;
        let provider = GlobalUsings { project: project.downgrade() };
        let program = project.update(cx, |p, cx| p.open_local_buffer(root.join("App/Program.cs"), cx)).await.unwrap();
        let window = cx.add_empty_window();

        let at = |buffer: &Entity<Buffer>, offset: usize, cx: &mut gpui::VisualTestContext| buffer.read_with(cx, |b, _| b.anchor_before(offset)..b.anchor_before(offset));
        let range = at(&program, 3, window);
        let actions = window.update(|window, cx| provider.code_actions(&program, range, window, cx)).await.unwrap();
        let titles: Vec<_> = actions.iter().map(|a| a.lsp_action.title().to_string()).collect();
        assert_eq!(titles, ["Move usings to _Imports.cs", "Move usings to _Imports.cs in every file of App"]);
        let range = at(&program, 40, window);
        assert!(window.update(|window, cx| provider.code_actions(&program, range, window, cx)).await.unwrap().is_empty(), "not offered away from the usings");

        // This file.
        let transaction = window.update(|window, cx| provider.apply_code_action(program.clone(), actions[0].clone(), true, window, cx)).await.unwrap();
        assert_eq!(transaction.0.len(), 2, "the file and _Imports.cs");
        assert_eq!(program.read_with(window, |b, _| b.text()), "Console.WriteLine(new User());\n");
        let imports = project.update(window, |p, cx| p.open_local_buffer(root.join("App/_Imports.cs"), cx)).await.unwrap();
        assert_eq!(imports.read_with(window, |b, _| b.text()), "global using System.Linq;\nglobal using System;\nglobal using App.Models;\n");

        // Every file of the project.
        let transaction = window.update(|window, cx| provider.apply_code_action(program.clone(), actions[1].clone(), true, window, cx)).await.unwrap();
        assert_eq!(transaction.0.len(), 2, "User.cs and _Imports.cs; Program.cs has nothing left, Plain.cs never had");
        let user = project.update(window, |p, cx| p.open_local_buffer(root.join("App/Models/User.cs"), cx)).await.unwrap();
        assert_eq!(user.read_with(window, |b, _| b.text()), "namespace App.Models;\n\npublic class User { }\n");
        assert_eq!(imports.read_with(window, |b, _| b.text()), "global using System.Linq;\nglobal using System;\nglobal using App.Models;\nglobal using System.Text;\n");
        assert!(imports.read_with(window, |b, _| b.is_dirty()), "left to review and save");
    }

    #[test]
    fn finds_the_project_and_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let write = |path: &str| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        };
        for path in ["App/App.csproj", "App/Program.cs", "App/Models/User.cs", "App/obj/Gen.cs", "App/Tests/Tests.csproj", "App/Tests/T.cs", "App/Ignored/X.cs"] {
            write(path);
        }
        assert_eq!(project_dir(&root.join("App/Models/User.cs")), Some(root.join("App")));
        assert_eq!(project_dir(&root.join("App/Tests/T.cs")), Some(root.join("App/Tests")));
        assert_eq!(project_name(&root.join("App")), "App");
        assert_eq!(project_files(&root.join("App"), &["Ignored".into()]), vec![root.join("App/Models/User.cs"), root.join("App/Program.cs")], "not obj, an ignored folder or another project");
    }
}
