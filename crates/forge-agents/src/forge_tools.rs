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

pub(crate) use forge_ui::ide_tools::{in_temp, line_arg, normalize, str_arg, tail, within};

/// Where `symbol` starts in `line` (bytes): its first occurrence as a whole word.
pub(crate) fn column_of(line: &str, symbol: &str) -> Option<usize> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    line.match_indices(symbol).map(|(i, _)| i).find(|&i| {
        let before = line[..i].chars().next_back();
        let after = line[i + symbol.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

pub(crate) async fn open(ide: &Ide, path: &Path, cx: &mut AsyncWindowContext) -> Result<Entity<Buffer>, String> {
    let task = ide.project.update(cx, |p, cx| p.open_local_buffer(path, cx)).map_err(|_| "The project is closed.".to_string())?;
    task.await.map_err(|e| format!("Can't open {}: {e:#}", ide.show(path)))
}

/// The buffer and position of `symbol` on `line` (1-based) of `path`.
pub(crate) async fn symbol_at(ide: &Ide, args: &Value, cx: &mut AsyncWindowContext) -> Result<(Entity<Buffer>, Point, String), String> {
    let path = str_arg(args, "path").ok_or("`path` is missing.")?;
    let line = line_arg(args, "line").ok_or("`line` (1-based) is missing.")?;
    let symbol = str_arg(args, "symbol").ok_or("`symbol` is missing.")?;
    let buffer = open(ide, &ide.resolve(&path)?, cx).await?;
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

/// The language servers' errors and warnings: in a file or a folder (`path`), in the files
/// this conversation changed (`changed`), or in every file that has some; only errors, or
/// only warnings, with `severity`.
pub(crate) async fn diagnostics(ide: Ide, args: Value, changed: &[PathBuf], cx: &mut AsyncWindowContext) -> ToolReply {
    let only_changed = args.get("changed").and_then(Value::as_bool).unwrap_or(false);
    let severity = str_arg(&args, "severity");
    let want = |error: bool| match severity.as_deref() {
        Some("error") => error,
        Some("warning") => !error,
        _ => true,
    };
    let within = str_arg(&args, "path").map(|p| ide.resolve(&p)).transpose()?;
    let mut files: Vec<PathBuf> = if only_changed {
        changed.to_vec()
    } else if within.as_ref().is_some_and(|p| p.is_file()) {
        within.iter().cloned().collect()
    } else {
        let project = ide.project.upgrade().ok_or("The project is closed.")?;
        let counts = cx.update(|_, cx| crate::verify::diagnostic_counts(&project, cx)).map_err(|_| "The project is closed.".to_string())?;
        counts.into_iter().filter(|(_, (e, w))| e + w > 0).map(|(p, _)| p).collect()
    };
    if let Some(within) = &within {
        files.retain(|f| f.starts_with(within));
    }
    files.sort();
    if files.is_empty() {
        return Ok(match (only_changed, &within) {
            (true, _) if changed.is_empty() => "You haven't changed any file in this conversation.".into(),
            (true, Some(_)) => "You haven't changed any file there in this conversation.".into(),
            _ => "The language servers report no errors or warnings there.".into(),
        });
    }
    let project = ide.project.clone();
    let checks = cx.update(|_, cx| crate::verify::check(project, files, Default::default(), cx)).map_err(|_| "The project is closed.".to_string())?.await;
    let mut lines = Vec::new();
    let (mut errors, mut warnings, mut with_problems) = (0, 0, 0);
    for check in &checks {
        let shown: Vec<_> = check.problems.iter().filter(|p| want(p.error)).collect();
        if !shown.is_empty() {
            with_problems += 1;
        }
        for p in shown {
            if p.error { errors += 1 } else { warnings += 1 }
            lines.push(format!("{}:{} {}: {}", ide.show(&check.path), p.line + 1, if p.error { "error" } else { "warning" }, p.message));
        }
    }
    if lines.is_empty() {
        return Ok("The language servers report no such problems there.".into());
    }
    let total = lines.len();
    lines.truncate(MAX_RESULTS);
    let more = if total > MAX_RESULTS { format!("\n… and {} more (narrow it with `path` or `severity`)", total - MAX_RESULTS) } else { String::new() };
    let plural = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
    Ok(format!("{}{more}\n({}, {} in {})", lines.join("\n"), plural(errors, "error"), plural(warnings, "warning"), plural(with_problems, "file")))
}

/// The problems your changes brought: in `path`, or in every file this conversation changed,
/// once the language servers have looked at them, compared with what each file had before
/// the agent first changed it (by message, as lines move).
pub(crate) async fn check_file(ide: Ide, args: Value, changed: &[PathBuf], baseline: &std::collections::HashMap<PathBuf, Vec<crate::verify::Problem>>, cx: &mut AsyncWindowContext) -> ToolReply {
    let files: Vec<PathBuf> = match str_arg(&args, "path") {
        Some(path) => vec![ide.resolve(&path)?],
        None => changed.to_vec(),
    };
    if files.is_empty() {
        return Ok("You haven't changed any file in this conversation: there is nothing to check.".into());
    }
    let project = ide.project.clone();
    crate::verify::settle(&project, cx).await;
    let mut parts = Vec::new();
    let mut clean = 0;
    for path in files {
        let shown = ide.show(&path);
        let Some(now) = crate::verify::problems_in(&project, &path, crate::verify::BASELINE_LIMIT, cx).await else {
            parts.push(format!("{shown}: it doesn't exist."));
            continue;
        };
        let Some(before) = baseline.get(&path) else {
            // Not changed by this conversation: all of its problems are as they were.
            let list: Vec<String> = now.iter().take(MAX_RESULTS).map(|p| format!("- line {} {}: {}", p.line + 1, if p.error { "error" } else { "warning" }, p.message)).collect();
            parts.push(if now.is_empty() {
                format!("{shown}: no problems (you haven't changed it in this conversation).")
            } else {
                format!("{shown}: you haven't changed it in this conversation; its problems:\n{}", list.join("\n"))
            });
            continue;
        };
        let (new, kept) = crate::verify::new_problems(&now, before);
        let had = if kept > 0 { format!(" ({kept} it already had before your changes remain)") } else { String::new() };
        if new.is_empty() {
            clean += 1;
            parts.push(format!("{shown}: no new problems{had}."));
        } else {
            let list: Vec<String> = new.iter().take(MAX_RESULTS).map(|p| format!("- line {} {}: {}", p.line + 1, if p.error { "error" } else { "warning" }, p.message)).collect();
            parts.push(format!("{shown}: {} new problem{}{had}:\n{}", new.len(), if new.len() == 1 { "" } else { "s" }, list.join("\n")));
        }
    }
    if clean == parts.len() && parts.len() > 1 {
        return Ok(format!("No new problems in the {} files you changed.\n{}", parts.len(), parts.join("\n")));
    }
    Ok(parts.join("\n"))
}

/// Opens `path` in the editor at `line` (to `end_line`), selected, for the user to look at.
pub(crate) async fn show_file(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let path = str_arg(&args, "path").ok_or("`path` is missing.")?;
    let abs = ide.resolve(&path)?;
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
    let buffer = open(ide, &ide.resolve(&path)?, cx).await?;
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
            let abs = ide.resolve(&path)?;
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
        records.push(crate::project_fs::WriteRecord { path, old_text, new_text, problems_before: None });
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

/// Where the implementations of `symbol` are (of an interface, a trait, an abstract method).
pub(crate) async fn implementations(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let (buffer, point, symbol) = symbol_at(&ide, &args, cx).await?;
    let task = ide.project.update(cx, |p, cx| p.implementations(&buffer, point, cx)).map_err(|_| closed())?;
    let links = task.await.map_err(|e| format!("The language server couldn't find them: {e:#}"))?.unwrap_or_default();
    if links.is_empty() {
        return Ok(format!("The language server finds no implementations of `{symbol}`."));
    }
    let mut lines = cx
        .update(|_, cx| links.iter().take(MAX_RESULTS).map(|l| describe(&ide, &l.target.buffer, l.target.range.start.to_point(&l.target.buffer.read(cx).snapshot()), cx)).collect::<Vec<_>>())
        .map_err(|_| closed())?;
    lines.sort();
    lines.dedup();
    Ok(format!("{} implementation{} of `{symbol}`:\n{}", links.len(), if links.len() == 1 { "" } else { "s" }, lines.join("\n")))
}

/// Where the type of `symbol` (a variable, a parameter, an expression) is defined.
pub(crate) async fn type_definition(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let (buffer, point, symbol) = symbol_at(&ide, &args, cx).await?;
    let task = ide.project.update(cx, |p, cx| p.type_definitions(&buffer, point, cx)).map_err(|_| closed())?;
    let links = task.await.map_err(|e| format!("The language server couldn't find it: {e:#}"))?.unwrap_or_default();
    if links.is_empty() {
        return Err(format!("The language server knows no type definition for `{symbol}` there."));
    }
    let lines = cx
        .update(|_, cx| links.iter().map(|l| describe(&ide, &l.target.buffer, l.target.range.start.to_point(&l.target.buffer.read(cx).snapshot()), cx)).collect::<Vec<_>>())
        .map_err(|_| closed())?;
    Ok(format!("The type of `{symbol}` is defined at:\n{}", lines.join("\n")))
}

/// A file's outline: its classes, functions, methods… nested, with their lines.
pub(crate) async fn document_symbols(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let path = str_arg(&args, "path").ok_or("`path` is missing.")?;
    let buffer = open(&ide, &ide.resolve(&path)?, cx).await?;
    let task = ide.project.update(cx, |p, cx| p.document_symbols(&buffer, cx)).map_err(|_| closed())?;
    let symbols = task.await.map_err(|e| format!("The language server couldn't list them: {e:#}"))?;
    if symbols.is_empty() {
        return Ok(format!("The language server lists no symbols in {path} (is one running for this file?)."));
    }
    fn walk(symbols: &[project::DocumentSymbol], depth: usize, out: &mut Vec<String>) {
        for s in symbols {
            let (start, end) = (s.range.start.0.row + 1, s.range.end.0.row + 1);
            let lines = if start == end { format!("line {start}") } else { format!("lines {start}-{end}") };
            out.push(format!("{}{} {} ({lines})", "  ".repeat(depth), symbol_kind(s.kind), s.name));
            walk(&s.children, depth + 1, out);
        }
    }
    let mut lines = Vec::new();
    walk(&symbols, 0, &mut lines);
    let total = lines.len();
    lines.truncate(MAX_RESULTS * 3);
    let more = if total > lines.len() { format!("\n… and {} more", total - lines.len()) } else { String::new() };
    Ok(format!("{}{more}", lines.join("\n")))
}

/// `function`, `class`, `method`…: a symbol kind as words.
fn symbol_kind(kind: impl std::fmt::Debug) -> String {
    let name = format!("{kind:?}");
    // `SymbolKind(12)`-style output for kinds the protocol doesn't name.
    if name.starts_with("SymbolKind") { "symbol".into() } else { name.to_lowercase().replace('_', " ") }
}

/// Who calls `symbol` (`incoming`, the default) or what it calls (`outgoing`), per the
/// language server's call hierarchy: each caller or callee with where.
pub(crate) async fn call_hierarchy(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let (buffer, point, symbol) = symbol_at(&ide, &args, cx).await?;
    let outgoing = str_arg(&args, "direction").as_deref() == Some("outgoing");
    let task = ide.project.update(cx, |p, cx| p.prepare_call_hierarchy(&buffer, point, cx)).map_err(|_| closed())?;
    let items = task.await.map_err(|e| format!("The language server has no call hierarchy there: {e:#}"))?.unwrap_or_default();
    let Some(item) = items.into_iter().next() else {
        return Err(format!("The language server has no call hierarchy for `{symbol}` (is it a function or method, and does the server support call hierarchies?)."));
    };
    let item_place = |item: &project::CallHierarchyItem, cx: &gpui::App| {
        let snapshot = item.buffer.read(cx).snapshot();
        describe(&ide, &item.buffer, item.selection_range.start.to_point(&snapshot), cx)
    };
    let lines: Vec<String> = if outgoing {
        let task = ide.project.update(cx, |p, cx| p.outgoing_calls(item, cx)).map_err(|_| closed())?;
        let calls = task.await.map_err(|e| format!("The language server couldn't list them: {e:#}"))?.unwrap_or_default();
        cx.update(|_, cx| calls.iter().take(MAX_RESULTS).map(|c| format!("- {} {}: {}", symbol_kind(c.to.kind), c.to.name, item_place(&c.to, cx))).collect()).map_err(|_| closed())?
    } else {
        let task = ide.project.update(cx, |p, cx| p.incoming_calls(item, cx)).map_err(|_| closed())?;
        let calls = task.await.map_err(|e| format!("The language server couldn't list them: {e:#}"))?.unwrap_or_default();
        cx.update(|_, cx| {
            calls
                .iter()
                .take(MAX_RESULTS)
                .map(|c| {
                    let sites: Vec<String> = c.from_ranges.iter().map(|l| (l.range.start.to_point(&l.buffer.read(cx).snapshot()).row + 1).to_string()).collect();
                    let times = if sites.len() > 1 { format!(" ({} calls, lines {})", sites.len(), sites.join(", ")) } else { String::new() };
                    format!("- {} {}{times}: {}", symbol_kind(c.from.kind), c.from.name, item_place(&c.from, cx))
                })
                .collect()
        })
        .map_err(|_| closed())?
    };
    if lines.is_empty() {
        return Ok(if outgoing { format!("`{symbol}` calls nothing the language server knows of.") } else { format!("Nothing calls `{symbol}`, as far as the language server knows.") });
    }
    Ok(format!("{} `{symbol}`:\n{}", if outgoing { "Calls made by" } else { "Callers of" }, lines.join("\n")))
}

/// The signatures (overloads) of the function called as `symbol(` on `line`, with their
/// parameters' documentation.
pub(crate) async fn signature_help(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let (buffer, point, symbol) = symbol_at(&ide, &args, cx).await?;
    // Inside the call's parentheses, where an editor asks for it.
    let inside = cx
        .update(|_, cx| {
            let snapshot = buffer.read(cx).snapshot();
            let after = Point::new(point.row, point.column + symbol.len() as u32);
            let rest: String = snapshot.text_for_range(after..Point::new(point.row, snapshot.line_len(point.row))).collect();
            let skip = rest.find('(').map(|i| i + 1).unwrap_or(0) as u32;
            Point::new(point.row, after.column + skip)
        })
        .map_err(|_| closed())?;
    let task = ide.project.update(cx, |p, cx| p.lsp_store().update(cx, |s, cx| s.signature_help(&buffer, inside, cx))).map_err(|_| closed())?;
    let helps = task.await.unwrap_or_default();
    let text = cx
        .update(|_, cx| {
            helps
                .iter()
                .flat_map(|h| h.signatures.iter())
                .map(|sig| {
                    let docs = sig.documentation.as_ref().map(|d| d.read(cx).source().trim().to_string()).filter(|d| !d.is_empty()).map(|d| format!("\n  {}", d.replace('\n', "\n  "))).unwrap_or_default();
                    format!("- {}{docs}", sig.label)
                })
                .collect::<Vec<_>>()
        })
        .map_err(|_| closed())?;
    if text.is_empty() {
        return Ok(format!("The language server gives no signature for `{symbol}(` there."));
    }
    Ok(format!("`{symbol}`:\n{}", text.join("\n")))
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
