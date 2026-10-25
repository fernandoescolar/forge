//! Type hierarchy: the base types and interfaces of the type at the cursor, and the types
//! that derive from it, in a list to jump to (Go › Type Hierarchy).
//!
//! Zed asks language servers for call hierarchies but has nothing for types; this uses
//! LSP's `textDocument/prepareTypeHierarchy`, `typeHierarchy/supertypes` and
//! `typeHierarchy/subtypes` with any server that offers them (Roslyn does).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use editor::Editor;
use gpui::{App, Entity, TaskExt as _, Window, actions};
use language::ToPointUtf16 as _;
use lsp::{LanguageServer, TypeHierarchyItem};
use project::Project;
use workspace::Workspace;

actions!(forge_languages, [
    /// Shows the base types and the derived types of the type at the cursor.
    ShowTypeHierarchy
]);

const TIMEOUT: Duration = Duration::from_secs(30);
/// How far up and down to follow the hierarchy, and how many types to list at most.
const MAX_DEPTH: usize = 8;
const MAX_TYPES: usize = 300;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ShowTypeHierarchy, window, cx| show(workspace, window, cx));
    })
    .detach();
}

/// Where a type is, and how it relates to the one asked about.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub detail: Option<String>,
    pub file: Option<PathBuf>,
    /// Zero-based line and UTF-16 column of its name.
    pub line: u32,
    pub column: u32,
    pub relation: Relation,
    /// Steps from the type asked about.
    pub depth: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    Base,
    Itself,
    Derived,
}

impl Entry {
    fn from_item(item: &TypeHierarchyItem, relation: Relation, depth: usize) -> Self {
        Entry {
            name: item.name.clone(),
            detail: item.detail.clone(),
            file: item.uri.to_file_path().ok(),
            line: item.selection_range.start.line,
            column: item.selection_range.start.character,
            relation,
            depth,
        }
    }

    /// `↑ Shape`, `● Circle`, `  ↓ Ring`, indented by depth.
    pub fn label(&self) -> String {
        let indent = "  ".repeat(self.depth.saturating_sub(1));
        match self.relation {
            Relation::Base => format!("{indent}↑ {}", self.name),
            Relation::Itself => format!("● {}", self.name),
            Relation::Derived => format!("{indent}↓ {}", self.name),
        }
    }
}

fn show(workspace: &mut Workspace, window: &mut Window, cx: &mut gpui::Context<Workspace>) {
    let Some(editor) = workspace.active_item(cx).and_then(|item| item.downcast::<Editor>()) else { return };
    let project = workspace.project().clone();
    let Some((buffer, position)) = cursor(&editor, cx) else { return };
    let Some(file) = buffer.read(cx).file().and_then(|f| f.as_local()).map(|f| f.abs_path(cx)) else { return };
    let servers = servers_with_type_hierarchy(&project, cx);
    let workspace_handle = workspace.weak_handle();
    cx.spawn_in(window, async move |_, cx| {
        let entries = hierarchy(servers, file, position).await?;
        workspace_handle.update_in(cx, |workspace, window, cx| {
            if entries.is_empty() {
                let id = workspace::notifications::NotificationId::unique::<ShowTypeHierarchy>();
                workspace.show_toast(workspace::Toast::new(id, "No type at the cursor (or the language server has no type hierarchy)"), cx);
                return;
            }
            let choices = entries.iter().map(|e| forge_ui::pick::Choice::new(e.label()).detail(e.detail.clone().unwrap_or_default())).collect();
            let workspace_handle = workspace.weak_handle();
            forge_ui::pick::pick(workspace, "Type hierarchy", choices, window, cx, move |index, window, cx| {
                if let (Some(entry), Some(workspace)) = (entries.get(index).cloned(), workspace_handle.upgrade()) {
                    open(&workspace, entry, window, cx);
                }
            });
        })
    })
    .detach_and_log_err(cx);
}

/// The buffer under the cursor and the cursor's position in it.
fn cursor(editor: &Entity<Editor>, cx: &mut App) -> Option<(Entity<language::Buffer>, lsp::Position)> {
    editor.update(cx, |editor, cx| {
        let head = editor.selections.newest_anchor().head();
        let (buffer, anchor) = editor.buffer().read(cx).text_anchor_for_position(head, cx)?;
        let point = anchor.to_point_utf16(&buffer.read(cx).snapshot());
        Some((buffer, lsp::Position::new(point.row, point.column)))
    })
}

/// The running language servers; those without a type hierarchy refuse the request and
/// are skipped (the capability isn't in the LSP types Zed uses).
fn servers_with_type_hierarchy(project: &Entity<Project>, cx: &App) -> Vec<Arc<LanguageServer>> {
    let lsp_store = project.read(cx).lsp_store();
    let lsp_store = lsp_store.read(cx);
    lsp_store.language_server_statuses().filter_map(|(id, _)| lsp_store.language_server_for_id(id)).collect()
}

/// The hierarchy of the type at `position` in `file`: its bases (nearest last, so the
/// list reads top-down), itself, then the types derived from it.
async fn hierarchy(servers: Vec<Arc<LanguageServer>>, file: PathBuf, position: lsp::Position) -> Result<Vec<Entry>> {
    let uri = lsp::Uri::from_file_path(&file).map_err(|()| anyhow::anyhow!("{} is not a file path", file.display()))?;
    for server in servers {
        let params = lsp::TypeHierarchyPrepareParams {
            text_document_position_params: lsp::TextDocumentPositionParams { text_document: lsp::TextDocumentIdentifier { uri: uri.clone() }, position },
            work_done_progress_params: Default::default(),
        };
        let Ok(Some(items)) = server.request::<lsp::request::TypeHierarchyPrepare>(params, TIMEOUT).await.into_response() else { continue };
        let Some(item) = items.into_iter().next() else { continue };
        let mut bases = Vec::new();
        walk(&server, &item, Relation::Base, 1, &mut bases).await;
        let mut derived = Vec::new();
        walk(&server, &item, Relation::Derived, 1, &mut derived).await;
        bases.reverse();
        let mut entries = bases;
        entries.push(Entry::from_item(&item, Relation::Itself, 0));
        entries.extend(derived);
        return Ok(entries);
    }
    Ok(Vec::new())
}

/// Follows supertypes or subtypes depth-first, each type once.
async fn walk(server: &LanguageServer, item: &TypeHierarchyItem, relation: Relation, depth: usize, out: &mut Vec<Entry>) {
    if depth > MAX_DEPTH || out.len() >= MAX_TYPES {
        return;
    }
    let params_super = lsp::TypeHierarchySupertypesParams { item: item.clone(), work_done_progress_params: Default::default(), partial_result_params: Default::default() };
    let next = match relation {
        Relation::Base => server.request::<lsp::request::TypeHierarchySupertypes>(params_super, TIMEOUT).await.into_response(),
        _ => {
            let params = lsp::TypeHierarchySubtypesParams { item: item.clone(), work_done_progress_params: Default::default(), partial_result_params: Default::default() };
            server.request::<lsp::request::TypeHierarchySubtypes>(params, TIMEOUT).await.into_response()
        }
    };
    let Ok(Some(next)) = next else { return };
    for child in next {
        let entry = Entry::from_item(&child, relation, depth);
        if out.iter().any(|e| e.name == entry.name && e.file == entry.file && e.line == entry.line) {
            continue;
        }
        out.push(entry);
        Box::pin(walk(server, &child, relation, depth + 1, out)).await;
    }
}

fn open(workspace: &Entity<Workspace>, entry: Entry, window: &mut Window, cx: &mut App) {
    let Some(file) = entry.file.clone() else { return };
    let task = workspace.update(cx, |workspace, cx| workspace.open_abs_path(file, workspace::OpenOptions::default(), window, cx));
    window
        .spawn(cx, async move |cx| {
            let item = task.await?;
            let editor = item.downcast::<Editor>().context("not an editor")?;
            editor.update_in(cx, |editor, window, cx| {
                let point = language::Point::new(entry.line, entry.column);
                editor.change_selections(Default::default(), window, cx, |s| s.select_ranges([point..point]));
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_show_direction_and_depth() {
        let entry = |name: &str, relation, depth| Entry { name: name.into(), detail: None, file: None, line: 0, column: 0, relation, depth };
        assert_eq!(entry("Shape", Relation::Base, 1).label(), "↑ Shape");
        assert_eq!(entry("Circle", Relation::Itself, 0).label(), "● Circle");
        assert_eq!(entry("Ring", Relation::Derived, 1).label(), "↓ Ring");
        assert_eq!(entry("Donut", Relation::Derived, 2).label(), "  ↓ Donut");
    }
}
