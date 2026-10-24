//! The tools of Forge's MCP server that read the IDE or show something: the language
//! servers' problems, definitions and references, and opening a file for the user. They
//! answer from what Forge already has open (unsaved edits included), instead of the agent
//! compiling or grepping on its own.

use std::path::{Path, PathBuf};

use gpui::{AsyncWindowContext, Entity};
use language::{Buffer, Point, ToPoint as _};
use serde_json::Value;
use workspace::Workspace;

use crate::forge_mcp::ToolReply;

/// Results listed at most (references, problems).
const MAX_RESULTS: usize = 100;

/// What a tool works on.
pub(crate) type Ide = forge_ui::ide_tools::ToolContext;

pub(crate) use forge_ui::ide_tools::{line_arg, str_arg, tail};

/// Where `symbol` starts in `line` (bytes): its first occurrence as a whole word.
pub(crate) fn column_of(line: &str, symbol: &str) -> Option<usize> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    line.match_indices(symbol).map(|(i, _)| i).find(|&i| {
        let before = line[..i].chars().next_back();
        let after = line[i + symbol.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

async fn open(ide: &Ide, path: &Path, cx: &mut AsyncWindowContext) -> Result<Entity<Buffer>, String> {
    let task = ide.project.update(cx, |p, cx| p.open_local_buffer(path, cx)).map_err(|_| "The project is closed.".to_string())?;
    task.await.map_err(|e| format!("Can't open {}: {e:#}", ide.show(path)))
}

/// The buffer and position of `symbol` on `line` (1-based) of `path`.
pub(crate) async fn symbol_at(ide: &Ide, args: &Value, cx: &mut AsyncWindowContext) -> Result<(Entity<Buffer>, Point, String), String> {
    let path = str_arg(args, "path").ok_or("`path` is missing.")?;
    let line = line_arg(args, "line").ok_or("`line` (1-based) is missing.")?;
    let symbol = str_arg(args, "symbol").ok_or("`symbol` is missing.")?;
    let buffer = open(ide, &ide.resolve(&path), cx).await?;
    let text = cx.update(|_, cx| {
        let snapshot = buffer.read(cx).snapshot();
        let row = line - 1;
        (row <= snapshot.max_point().row).then(|| snapshot.text_for_range(Point::new(row, 0)..Point::new(row, snapshot.line_len(row))).collect::<String>())
    });
    let text = text.map_err(|_| "The project is closed.".to_string())?.ok_or_else(|| format!("{path} has no line {line}."))?;
    let column = column_of(&text, &symbol).ok_or_else(|| format!("`{symbol}` is not on line {line} of {path}, which reads: {}", text.trim()))?;
    Ok((buffer, Point::new(line - 1, column as u32), symbol))
}

/// `path:line:column  code` for a location.
fn describe(ide: &Ide, buffer: &Entity<Buffer>, start: Point, cx: &gpui::App) -> String {
    let buffer = buffer.read(cx);
    let path = buffer.file().and_then(|f| f.as_local()).map(|f| ide.show(&f.abs_path(cx))).unwrap_or_else(|| "(unsaved buffer)".into());
    let snapshot = buffer.snapshot();
    let code: String = snapshot.text_for_range(Point::new(start.row, 0)..Point::new(start.row, snapshot.line_len(start.row))).collect();
    format!("{path}:{}:{}  {}", start.row + 1, start.column + 1, code.trim())
}

pub(crate) async fn definition(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let (buffer, point, symbol) = symbol_at(&ide, &args, cx).await?;
    let task = ide.project.update(cx, |p, cx| p.definitions(&buffer, point, cx)).map_err(|_| "The project is closed.".to_string())?;
    let links = task.await.map_err(|e| format!("The language server couldn't find it: {e:#}"))?.unwrap_or_default();
    if links.is_empty() {
        return Err(format!("The language server knows no definition of `{symbol}` there (is a language server running for this file?)."));
    }
    let lines = cx
        .update(|_, cx| links.iter().map(|l| describe(&ide, &l.target.buffer, l.target.range.start.to_point(&l.target.buffer.read(cx).snapshot()), cx)).collect::<Vec<_>>())
        .map_err(|_| "The project is closed.".to_string())?;
    Ok(format!("`{symbol}` is defined at:\n{}", lines.join("\n")))
}

pub(crate) async fn references(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let (buffer, point, symbol) = symbol_at(&ide, &args, cx).await?;
    let task = ide.project.update(cx, |p, cx| p.references(&buffer, point, cx)).map_err(|_| "The project is closed.".to_string())?;
    let locations = task.await.map_err(|e| format!("The language server couldn't find them: {e:#}"))?.unwrap_or_default();
    if locations.is_empty() {
        return Ok(format!("The language server finds no references to `{symbol}`."));
    }
    let total = locations.len();
    let mut lines = cx
        .update(|_, cx| locations.iter().take(MAX_RESULTS).map(|l| describe(&ide, &l.buffer, l.range.start.to_point(&l.buffer.read(cx).snapshot()), cx)).collect::<Vec<_>>())
        .map_err(|_| "The project is closed.".to_string())?;
    lines.sort();
    lines.dedup();
    let more = if total > MAX_RESULTS { format!("\n… and {} more", total - MAX_RESULTS) } else { String::new() };
    Ok(format!("{total} references to `{symbol}`:\n{}{more}", lines.join("\n")))
}

/// The language servers' errors and warnings: in `path`, or in every file that has some.
pub(crate) async fn diagnostics(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let files: Vec<PathBuf> = match str_arg(&args, "path") {
        Some(path) => vec![ide.resolve(&path)],
        None => {
            let project = ide.project.upgrade().ok_or("The project is closed.")?;
            let counts = cx.update(|_, cx| crate::verify::diagnostic_counts(&project, cx)).map_err(|_| "The project is closed.".to_string())?;
            let mut files: Vec<PathBuf> = counts.into_iter().filter(|(_, (e, w))| e + w > 0).map(|(p, _)| p).collect();
            files.sort();
            files
        }
    };
    if files.is_empty() {
        return Ok("The language servers report no errors or warnings.".into());
    }
    let project = ide.project.clone();
    let checks = cx.update(|_, cx| crate::verify::check(project, files, Default::default(), cx)).map_err(|_| "The project is closed.".to_string())?.await;
    let mut lines = Vec::new();
    for check in &checks {
        for p in &check.problems {
            lines.push(format!("{}:{} {}: {}", ide.show(&check.path), p.line + 1, if p.error { "error" } else { "warning" }, p.message));
        }
    }
    if lines.is_empty() {
        return Ok("The language servers report no errors or warnings there.".into());
    }
    let total = lines.len();
    lines.truncate(MAX_RESULTS);
    let more = if total > MAX_RESULTS { format!("\n… and {} more", total - MAX_RESULTS) } else { String::new() };
    Ok(format!("{}{more}", lines.join("\n")))
}

/// Opens `path` in the editor at `line` (to `end_line`), selected, for the user to look at.
pub(crate) async fn show_file(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let path = str_arg(&args, "path").ok_or("`path` is missing.")?;
    let abs = ide.resolve(&path);
    let start = line_arg(&args, "line").unwrap_or(1);
    let end = line_arg(&args, "end_line").unwrap_or(start).max(start);
    let open = ide
        .workspace
        .update_in(cx, |ws, window, cx| ws.open_abs_path(abs, workspace::OpenOptions::default(), window, cx))
        .map_err(|_| "The window is closed.".to_string())?;
    let item = open.await.map_err(|e| format!("Can't open {path}: {e:#}"))?;
    if let Some(editor) = item.downcast::<editor::Editor>() {
        editor
            .update_in(cx, |editor, window, cx| {
                let range = Point::new(start - 1, 0)..Point::new(end, 0);
                editor.change_selections(editor::SelectionEffects::scroll(editor::scroll::Autoscroll::center()), window, cx, |s| s.select_ranges([range]));
            })
            .ok();
    }
    Ok(format!("Opened {path} at line {start} for the user."))
}

/// What a language-server edit does.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum EditKind {
    Rename { symbol: String, new_name: String },
    CodeAction { title: String },
    Format,
}

/// An edit the agent asked the language server for (`rename_symbol`, `apply_code_action`,
/// `format_file`), with what it would touch.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EditPlan {
    pub kind: EditKind,
    pub args: Value,
    /// Places it changes, when known (a rename's references).
    pub places: Option<usize>,
    pub files: Vec<PathBuf>,
}

impl EditPlan {
    /// What it does, in a few words.
    pub fn summary(&self) -> String {
        match &self.kind {
            EditKind::Rename { symbol, new_name } => format!("rename `{symbol}` to `{new_name}`"),
            EditKind::CodeAction { title } => format!("apply \"{title}\""),
            EditKind::Format => "format the file".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum EditState {
    Waiting,
    Applying,
    Applied,
    Declined,
    Failed(String),
}

impl EditState {
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Waiting | Self::Failed(_))
    }

    pub fn outcome(&self) -> String {
        match self {
            Self::Applied => "applied".into(),
            Self::Declined => "declined".into(),
            Self::Failed(e) => format!("failed: {e}"),
            _ => "not decided".into(),
        }
    }
}

fn closed() -> String {
    "The project is closed.".to_string()
}

/// The lines `line`..=`end_line` (1-based) of `path`, as buffer points.
async fn lines_at(ide: &Ide, args: &Value, cx: &mut AsyncWindowContext) -> Result<(Entity<Buffer>, std::ops::Range<Point>), String> {
    let path = str_arg(args, "path").ok_or("`path` is missing.")?;
    let line = line_arg(args, "line").ok_or("`line` (1-based) is missing.")?;
    let end = line_arg(args, "end_line").unwrap_or(line).max(line);
    let buffer = open(ide, &ide.resolve(&path), cx).await?;
    let range = cx
        .update(|_, cx| {
            let snapshot = buffer.read(cx).snapshot();
            let last = snapshot.max_point().row;
            (line - 1 <= last).then(|| Point::new(line - 1, 0)..Point::new((end - 1).min(last), snapshot.line_len((end - 1).min(last))))
        })
        .map_err(|_| closed())?
        .ok_or_else(|| format!("{path} has no line {line}."))?;
    Ok((buffer, range))
}

async fn code_actions_at(ide: &Ide, args: &Value, cx: &mut AsyncWindowContext) -> Result<(Entity<Buffer>, Vec<project::CodeAction>), String> {
    let (buffer, range) = lines_at(ide, args, cx).await?;
    let task = ide.project.update(cx, |p, cx| p.code_actions(&buffer, range, None, cx)).map_err(|_| closed())?;
    let actions = task.await.map_err(|e| format!("The language server couldn't list them: {e:#}"))?.unwrap_or_default();
    Ok((buffer, actions))
}

/// The language server's code actions (quick fixes, refactors) on some lines.
pub(crate) async fn code_actions(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let (_, actions) = code_actions_at(&ide, &args, cx).await?;
    if actions.is_empty() {
        return Ok("The language server offers no code actions there.".into());
    }
    let lines: Vec<String> = actions
        .iter()
        .map(|a| match a.lsp_action.action_kind() {
            Some(kind) => format!("- {} ({})", a.lsp_action.title(), kind.as_str()),
            None => format!("- {}", a.lsp_action.title()),
        })
        .collect();
    Ok(format!("Code actions there (apply one with `apply_code_action` and its exact title):\n{}", lines.join("\n")))
}

/// Checks an edit call and finds what it would touch.
pub(crate) async fn edit_plan(name: &str, ide: &Ide, args: &Value, cx: &mut AsyncWindowContext) -> Result<EditPlan, String> {
    match name {
        "rename_symbol" => {
            let new_name = str_arg(args, "new_name").ok_or("`new_name` is missing.")?;
            let (buffer, point, symbol) = symbol_at(ide, args, cx).await?;
            if new_name == symbol {
                return Err(format!("`{symbol}` already has that name."));
            }
            let task = ide.project.update(cx, |p, cx| p.references(&buffer, point, cx)).map_err(|_| closed())?;
            let locations = task.await.map_err(|e| format!("The language server can't rename it: {e:#}"))?.unwrap_or_default();
            let mut files: Vec<PathBuf> = cx.update(|_, cx| locations.iter().filter_map(|l| buffer_path(&l.buffer, cx)).collect()).map_err(|_| closed())?;
            files.sort();
            files.dedup();
            Ok(EditPlan { kind: EditKind::Rename { symbol, new_name }, args: args.clone(), places: Some(locations.len()), files })
        }
        "apply_code_action" => {
            let title = str_arg(args, "title").ok_or("`title` is missing.")?;
            let (buffer, actions) = code_actions_at(ide, args, cx).await?;
            if !actions.iter().any(|a| a.lsp_action.title() == title) {
                let offered: Vec<&str> = actions.iter().map(|a| a.lsp_action.title()).collect();
                return Err(format!("No code action there is called \"{title}\". Offered: {}", if offered.is_empty() { "none".to_string() } else { offered.join(" | ") }));
            }
            let files = cx.update(|_, cx| buffer_path(&buffer, cx)).map_err(|_| closed())?.into_iter().collect();
            Ok(EditPlan { kind: EditKind::CodeAction { title }, args: args.clone(), places: None, files })
        }
        "format_file" => {
            let path = str_arg(args, "path").ok_or("`path` is missing.")?;
            let abs = ide.resolve(&path);
            open(ide, &abs, cx).await?;
            Ok(EditPlan { kind: EditKind::Format, args: args.clone(), places: None, files: vec![abs] })
        }
        other => Err(format!("Unknown edit: {other}")),
    }
}

fn buffer_path(buffer: &Entity<Buffer>, cx: &gpui::App) -> Option<PathBuf> {
    buffer.read(cx).file().and_then(|f| f.as_local()).map(|f| f.abs_path(cx))
}

/// Makes the edit with the language server, through the open buffers, and saves the files:
/// what each was on disk before and is now.
pub(crate) async fn apply_edit(ide: &Ide, plan: &EditPlan, cx: &mut AsyncWindowContext) -> Result<Vec<crate::project_fs::WriteRecord>, String> {
    let transaction = match &plan.kind {
        EditKind::Rename { new_name, .. } => {
            let (buffer, point, _) = symbol_at(ide, &plan.args, cx).await?;
            let new_name = new_name.clone();
            let task = ide.project.update(cx, |p, cx| p.perform_rename(buffer, point, new_name, None, cx)).map_err(|_| closed())?;
            task.await.map_err(|e| format!("The language server couldn't rename it: {e:#}"))?
        }
        EditKind::CodeAction { title } => {
            let (buffer, actions) = code_actions_at(ide, &plan.args, cx).await?;
            let action = actions.into_iter().find(|a| a.lsp_action.title() == title).ok_or("The code action is no longer offered there.")?;
            let task = ide.project.update(cx, |p, cx| p.apply_code_action(buffer, action, true, cx)).map_err(|_| closed())?;
            task.await.map_err(|e| format!("The language server couldn't apply it: {e:#}"))?
        }
        EditKind::Format => {
            let path = plan.files.first().ok_or("No file.")?.clone();
            let buffer = open(ide, &path, cx).await?;
            let task = ide
                .project
                .update(cx, |p, cx| p.format([buffer].into_iter().collect(), project::lsp_store::LspFormatTarget::Buffers, true, project::lsp_store::FormatTrigger::Manual, cx))
                .map_err(|_| closed())?;
            task.await.map_err(|e| format!("Couldn't format it: {e:#}"))?
        }
    };
    if transaction.0.is_empty() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for buffer in transaction.0.into_keys() {
        let (path, new_text) = cx.update(|_, cx| (buffer_path(&buffer, cx), buffer.read(cx).text())).map_err(|_| closed())?;
        let Some(path) = path else { continue };
        // Not saved yet: the file on disk is still the old text.
        let old_text = std::fs::read_to_string(&path).ok();
        let save = ide.project.update(cx, |p, cx| p.save_buffer(buffer.clone(), cx)).map_err(|_| closed())?;
        save.await.map_err(|e| format!("Couldn't save {}: {e:#}", ide.show(&path)))?;
        records.push(crate::project_fs::WriteRecord { path, old_text, new_text });
    }
    records.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(records)
}

/// What the language server says about a symbol (its type, signature and docs).
pub(crate) async fn hover(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let (buffer, point, symbol) = symbol_at(&ide, &args, cx).await?;
    let task = ide.project.update(cx, |p, cx| p.hover(&buffer, point, cx)).map_err(|_| closed())?;
    let text: Vec<String> = task.await.unwrap_or_default().into_iter().flat_map(|h| h.contents).map(|b| b.text.trim().to_string()).filter(|t| !t.is_empty()).collect();
    if text.is_empty() {
        return Ok(format!("The language server says nothing about `{symbol}` there."));
    }
    Ok(text.join("\n\n"))
}

/// Symbols (types, functions…) whose name matches `query`, across the project.
pub(crate) async fn workspace_symbols(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let query = str_arg(&args, "query").ok_or("`query` is missing.")?;
    let task = ide.project.update(cx, |p, cx| p.symbols(&query, cx)).map_err(|_| closed())?;
    let symbols = task.await.map_err(|e| format!("The language servers couldn't search: {e:#}"))?;
    if symbols.is_empty() {
        return Ok(format!("No symbol matches `{query}` (is a language server running for these files?)."));
    }
    let total = symbols.len();
    let lines = cx
        .update(|_, cx| {
            let project = ide.project.upgrade()?;
            Some(
                symbols
                    .iter()
                    .take(MAX_RESULTS)
                    .map(|s| {
                        let path = match &s.path {
                            project::lsp_store::SymbolLocation::InProject(pp) => project.read(cx).absolute_path(pp, cx).map(|p| ide.show(&p)).unwrap_or_else(|| pp.path.as_unix_str().to_string()),
                            project::lsp_store::SymbolLocation::OutsideProject { abs_path, .. } => abs_path.to_string_lossy().into_owned(),
                        };
                        let container = s.container_name.as_deref().map(|c| format!(" in {c}")).unwrap_or_default();
                        format!("{} ({:?}{container})  {path}:{}", s.name, s.kind, s.range.start.0.row + 1)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .map_err(|_| closed())?
        .ok_or_else(closed)?;
    let more = if total > MAX_RESULTS { format!("\n… and {} more", total - MAX_RESULTS) } else { String::new() };
    Ok(format!("{}{more}", lines.join("\n")))
}

/// The file editor the user activated last, in any pane (when the agent asks, the active
/// item is usually its own thread, often in the same pane).
fn last_editor(workspace: &Workspace, cx: &gpui::App) -> Option<Entity<editor::Editor>> {
    let recency = workspace.recently_activated_items(cx);
    workspace
        .items_of_type::<editor::Editor>(cx)
        .filter(|e| e.read(cx).buffer().read(cx).as_singleton().is_some())
        .max_by_key(|e| recency.get(&e.entity_id()).copied().unwrap_or(0))
}

/// What the user is looking at and working on: the file and line they are in (and what
/// they selected), the files they have open, the problems near their cursor, the end of the
/// terminal they last used, and the tests that failed in their last run.
pub(crate) async fn user_context(ide: Ide, cx: &mut AsyncWindowContext) -> ToolReply {
    use multi_buffer::ToPoint as _;
    let gathered = ide
        .workspace
        .update_in(cx, |ws, _, cx| {
            let mut text = String::new();
            let mut at: Option<(PathBuf, u32)> = None;
            match last_editor(ws, cx) {
                Some(editor) => {
                    let editor = editor.read(cx);
                    let buffer = editor.buffer().read(cx);
                    let path = buffer.as_singleton().and_then(|b| b.read(cx).file().and_then(|f| f.as_local()).map(|f| f.abs_path(cx)));
                    let snapshot = buffer.snapshot(cx);
                    let selection = editor.selections.newest_anchor();
                    let (start, end) = (selection.start.to_point(&snapshot), selection.end.to_point(&snapshot));
                    if let Some(path) = path {
                        text.push_str(&format!("The user is in {} at line {}.\n", ide.show(&path), start.row + 1));
                        at = Some((path, start.row));
                    }
                    if start != end {
                        let selected: String = snapshot.text_for_range(start..end).collect();
                        let selected = if selected.len() > 4000 { format!("{}…", &selected[..selected.floor_char_boundary(4000)]) } else { selected };
                        text.push_str(&format!("They selected lines {}-{}:\n```\n{selected}\n```\n", start.row + 1, end.row + 1));
                    }
                }
                None => text.push_str("The user has no file open in an editor.\n"),
            }
            let mut open: Vec<String> = ws
                .items_of_type::<editor::Editor>(cx)
                .filter_map(|e| e.read(cx).buffer().read(cx).as_singleton()?.read(cx).file()?.as_local().map(|f| ide.show(&f.abs_path(cx))))
                .collect();
            open.sort();
            open.dedup();
            if !open.is_empty() {
                text.push_str(&format!("\nOpen files: {}\n", open.join(", ")));
            }
            if let Some(output) = crate::context::terminal_output(ws, cx) {
                text.push_str(&format!("\nThe end of the terminal they last used:\n```\n{}\n```\n", tail(&output, 30)));
            }
            // What other parts of Forge add (the tests that failed in the last run).
            for section in forge_ui::ide_tools::ide_tools().context(&ide, ws, cx) {
                text.push_str(&format!("\n{section}\n"));
            }
            (text, at)
        })
        .map_err(|_| "The window is closed.".to_string())?;
    let (mut text, at) = gathered;
    // The problems the language servers report near the cursor.
    if let Some((path, row)) = at {
        let project = ide.project.clone();
        let checks = crate::verify::read_problems(project, vec![path.clone()], Default::default(), cx).await;
        let near: Vec<String> = checks
            .iter()
            .flat_map(|c| &c.problems)
            .filter(|p| p.line + 5 >= row && p.line <= row + 5)
            .map(|p| format!("line {} {}: {}", p.line + 1, if p.error { "error" } else { "warning" }, p.message))
            .collect();
        if !near.is_empty() {
            text.push_str(&format!("\nProblems near their cursor in {}:\n{}\n", ide.show(&path), near.join("\n")));
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_symbols_as_whole_words() {
        assert_eq!(column_of("let total = subtotal + total_tax;", "total"), Some(4));
        assert_eq!(column_of("fn parse_args() {}", "parse_args"), Some(3));
        assert_eq!(column_of("subtotal", "total"), None);
        assert_eq!(column_of("    self.total()", "total"), Some(9));
    }
}
