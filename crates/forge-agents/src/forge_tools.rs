//! The tools of Forge's MCP server that read the IDE or show something: the language
//! servers' problems, definitions and references, and opening a file for the user. They
//! answer from what Forge already has open (unsaved edits included), instead of the agent
//! compiling or grepping on its own.

use std::path::{Path, PathBuf};

use gpui::{AsyncWindowContext, Entity, WeakEntity};
use language::{Buffer, Point, ToPoint as _};
use project::Project;
use serde_json::Value;
use workspace::Workspace;

use crate::forge_mcp::ToolReply;

/// Results listed at most (references, problems).
const MAX_RESULTS: usize = 100;

/// What a tool works on.
#[derive(Clone)]
pub(crate) struct Ide {
    pub project: WeakEntity<Project>,
    pub workspace: WeakEntity<Workspace>,
    /// The thread's folder: relative paths start there, and paths are shown from it.
    pub root: PathBuf,
}

impl Ide {
    pub fn resolve(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    pub fn show(&self, path: &Path) -> String {
        path.strip_prefix(&self.root).unwrap_or(path).to_string_lossy().into_owned()
    }
}

pub(crate) fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// A 1-based line argument.
pub(crate) fn line_arg(args: &Value, key: &str) -> Option<u32> {
    args.get(key).and_then(Value::as_u64).filter(|l| *l >= 1).map(|l| l as u32)
}

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

/// Lines of a log or a terminal kept when quoting it to the agent: the last ones.
const TAIL_LINES: usize = 80;

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.trim_end().lines().collect();
    match all.len().checked_sub(lines) {
        Some(cut) if cut > 0 => format!("… {cut} earlier lines\n{}", all[cut..].join("\n")),
        _ => all.join("\n"),
    }
}

/// Which tests of `projects` a `run_tests` call means: `None` for all of them, else the
/// method FQNs per project manifest. `path` is a test file or a folder of them; `name` a part
/// of the test's name (or of its class and namespace).
pub(crate) fn select_tests(projects: &[forge_tests::discovery::TestProject], path: Option<&Path>, name: Option<&str>) -> Option<Vec<(PathBuf, Vec<String>)>> {
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

/// Runs tests in the Tests panel (shown, with its progress and the gutter results) and
/// reports how they went: counts, and each failure with its place and message.
pub(crate) async fn run_tests(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    use forge_tests::{TestPanel, runner::Scope, trx::Outcome};
    let path = str_arg(&args, "path").map(|p| ide.resolve(&p));
    let name = str_arg(&args, "name");
    let panel = ide
        .workspace
        .update_in(cx, |ws, window, cx| {
            ws.open_panel::<TestPanel>(window, cx);
            ws.panel::<TestPanel>(cx)
        })
        .map_err(|_| "The window is closed.".to_string())?
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
        .map_err(|_| "The window is closed.".to_string())??;
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
                        let place = if test.row == forge_tests::discovery::NO_ROW { ide.show(&test.file) } else { format!("{}:{}", ide.show(&test.file), test.row + 1) };
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

/// Response bodies quoted to the agent, at most (bytes).
const BODY_BYTES: usize = 8_000;

/// Sends an HTTP request through Forge's `.http` support (the response tab shows it): the
/// request at `line` of a `.http` file, or an ad-hoc one resolved with the project's
/// environments. Returns the status, headers and body.
pub(crate) async fn http_request(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let receiver = match str_arg(&args, "path") {
        Some(path) => {
            let file = ide.resolve(&path);
            let line = line_arg(&args, "line").ok_or("`line` (1-based) of the request in the .http file is missing.")?;
            ide.workspace.update_in(cx, |ws, window, cx| forge_http::send_request_at(ws, &file, line - 1, window, cx)).map_err(|_| "The window is closed.".to_string())?
        }
        None => {
            let url = str_arg(&args, "url").ok_or("Give `url` (and `method`), or `path` and `line` of a request in a .http file.")?;
            let method = str_arg(&args, "method").unwrap_or_else(|| "GET".into());
            let headers: Vec<(String, String)> = args.get("headers").and_then(Value::as_object).map(|h| h.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect()).unwrap_or_default();
            let body = args.get("body").map(|b| b.as_str().map(str::to_string).unwrap_or_else(|| b.to_string()));
            // Resolved as if in a .http file at the root: the project's environments apply.
            let file = ide.root.join("agent.http");
            ide.workspace.update_in(cx, |ws, window, cx| forge_http::send_adhoc(ws, &file, &method, &url, headers, body, window, cx)).map_err(|_| "The window is closed.".to_string())?
        }
    }
    .map_err(|e| format!("Can't send it: {e:#}"))?;
    let outcome = receiver.await.map_err(|_| "The request was cancelled.".to_string())?;
    let request = &outcome.request;
    let environment = outcome.environment.as_deref().map(|e| format!(" (environment {e})")).unwrap_or_default();
    let exchange = outcome.response.map_err(|e| format!("{} {}{environment} failed: {e}", request.method, request.url))?;
    let headers: Vec<String> = exchange.headers.iter().map(|(k, v)| format!("{k}: {v}")).collect();
    let body = if exchange.is_binary() {
        format!("({} bytes of binary content)", exchange.body.len())
    } else {
        let text = String::from_utf8_lossy(&exchange.body);
        if text.len() > BODY_BYTES {
            let mut cut = BODY_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}\n… ({} more bytes)", &text[..cut], text.len() - cut)
        } else {
            text.into_owned()
        }
    };
    Ok(format!(
        "{} {}{environment} → {} {} in {} ms (the user sees it in the response tab)\n\n{}\n\n{body}",
        request.method,
        request.url,
        exchange.status,
        exchange.reason,
        exchange.elapsed.as_millis(),
        headers.join("\n")
    ))
}

fn run_controller(ide: &Ide, cx: &mut AsyncWindowContext) -> Result<Entity<forge_run::RunController>, String> {
    let workspace = ide.workspace.upgrade().ok_or("The window is closed.")?;
    cx.update(|_, cx| forge_run::RunController::for_workspace(&workspace, cx)).ok().flatten().ok_or_else(|| "Run isn't set up for this window.".to_string())
}

/// The run target's output so far (its terminal), the last `lines`.
fn app_output_text(controller: &Entity<forge_run::RunController>, lines: usize, cx: &gpui::App) -> Option<String> {
    let content = controller.read(cx).terminal()?.read(cx).get_content();
    Some(tail(&content, lines))
}

/// Starts a run target (the title bar's Run) and reports its first output.
pub(crate) async fn run_app(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    use forge_run::State;
    let controller = run_controller(&ide, cx)?;
    let wanted = str_arg(&args, "target").map(|t| t.to_lowercase());
    let picked = cx
        .update(|_, cx| {
            let c = controller.read(cx);
            let names = || c.targets().iter().map(|t| format!("{} ({})", t.name, t.kind.label())).collect::<Vec<_>>().join(", ");
            if c.targets().is_empty() {
                return Err("Forge found nothing to run in this project.".to_string());
            }
            if c.state(cx) != State::Idle {
                return Err("Something is already running (title bar Run); stop it first with `stop_app`.".to_string());
            }
            let target = match &wanted {
                Some(w) => c.targets().iter().find(|t| t.name.to_lowercase() == *w || t.id().to_lowercase() == *w).ok_or_else(|| format!("No run target is called that. The targets are: {}", names()))?,
                None => c.selected().or(c.targets().first()).unwrap(),
            };
            Ok((target.id(), target.name.clone()))
        })
        .map_err(|_| "The window is closed.".to_string())??;
    let (id, name) = picked;
    cx.update(|window, cx| {
        controller.update(cx, |c, cx| c.select(id, cx));
        let controller = controller.clone();
        // As the Run action does: starting a task updates the workspace.
        window.defer(cx, move |window, cx| controller.update(cx, |c, cx| c.run(window, cx)));
    })
    .map_err(|_| "The window is closed.".to_string())?;
    // Its first output: startup errors, the address it listens on…
    cx.background_executor().timer(std::time::Duration::from_secs(5)).await;
    let (state, output) = cx.update(|_, cx| (controller.read(cx).state(cx), app_output_text(&controller, 40, cx))).map_err(|_| "The window is closed.".to_string())?;
    let status = if state == State::Idle { "It already ended." } else { "It is running; `app_output` reads more of its output, `stop_app` stops it." };
    Ok(format!("Started {name} in a terminal the user can see. {status}\n\nOutput so far:\n{}", output.unwrap_or_default()))
}

/// The output of the run target, while it runs or after.
pub(crate) async fn app_output(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let controller = run_controller(&ide, cx)?;
    let lines = args.get("lines").and_then(Value::as_u64).map(|l| l.clamp(1, 1000) as usize).unwrap_or(TAIL_LINES);
    cx.update(|_, cx| {
        let running = controller.read(cx).state(cx) != forge_run::State::Idle;
        let output = app_output_text(&controller, lines, cx).ok_or("Nothing has run yet.")?;
        Ok(format!("{}\n\n{output}", if running { "Running." } else { "Not running anymore." }))
    })
    .map_err(|_| "The window is closed.".to_string())?
}

pub(crate) async fn stop_app(ide: Ide, cx: &mut AsyncWindowContext) -> ToolReply {
    let controller = run_controller(&ide, cx)?;
    cx.update(|_, cx| {
        if controller.read(cx).state(cx) == forge_run::State::Idle {
            return Ok("Nothing is running.".to_string());
        }
        controller.update(cx, |c, cx| c.stop(cx));
        Ok("Stopping it (Ctrl+C).".to_string())
    })
    .map_err(|_| "The window is closed.".to_string())?
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

    #[test]
    fn selects_tests_by_file_folder_and_name() {
        use forge_tests::discovery::{Kind, TestClass, TestMethod, TestProject};
        let method = |fqn: &str, file: &str| TestMethod { fqn: fqn.into(), name: fqn.rsplit('.').next().unwrap().into(), file: file.into(), row: 1 };
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
        assert_eq!(tail("1\n2\n3\n4", 2), "… 2 earlier lines\n3\n4");
    }
}
