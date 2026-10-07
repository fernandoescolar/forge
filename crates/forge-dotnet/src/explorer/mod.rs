//! The Solution Explorer dock panel: the solution as Visual Studio shows it (solution
//! folders, projects, dependencies and project files with nesting), with context menus
//! for everything vscode-solution-explorer offers, inline naming, keyboard navigation and
//! drag & drop.

mod git_status;
mod menus;
mod ops;
mod tools;

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dotnet_model::explorer::{Node, NodeKind};
use editor::{Editor, EditorEvent};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Action, AnyElement, App, AppContext as _, ClickEvent, IntoElement, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding,
    MouseButton, MouseDownEvent, ParentElement as _, Pixels, Point, Render, SharedString, Styled as _, Subscription, UniformListScrollHandle, WeakEntity,
    Window, actions, anchored, deferred, div, px, uniform_list,
};
use theme::ActiveTheme as _;
use ui::{
    Button, ButtonCommon as _, ButtonSize, ButtonStyle, Clickable as _, Color, ContextMenu, Disableable as _, Icon, IconButton, IconName, IconSize, Label,
    LabelCommon as _, LabelSize, ListItem, ListItemSpacing, Toggleable as _, Tooltip, WithScrollbar as _, h_flex, v_flex,
};
use ui::{InteractiveElement as _, StatefulInteractiveElement as _};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::config;
use crate::model::{DotnetModel, ModelEvent};

actions!(
    solution_explorer,
    [
        /// Shows or hides the Solution Explorer.
        ToggleFocus,
        /// Loads the solution and its projects again.
        Refresh,
        CollapseAll,
        /// Selects the active editor's file in the Solution Explorer.
        RevealActiveFile,
        /// Chooses which solution of the workspace to show.
        SelectSolution,
        NewSolution,
        NewProject,
        AddExistingProject,
        NewFile,
        NewFolder,
        NewSolutionFolder,
        AddSolutionItem,
        Rename,
        Delete,
        RemoveFromSolution,
        Copy,
        Cut,
        Paste,
        Duplicate,
        CopyPath,
        CopyRelativePath,
        RevealInFinder,
        OpenInTerminal,
        Open,
        Build,
        Rebuild,
        Clean,
        Restore,
        Test,
        Run,
        Watch,
        Pack,
        Publish,
        /// Opens the NuGet package manager for the selected project or the solution.
        ManagePackages,
        /// Opens the selected project's user secrets (`secrets.json`), setting them up first.
        ManageUserSecrets,
        /// The `using` directives of every C# file of the selected project become global
        /// usings in its `globalUsingsFile` (dotnet.json).
        MoveUsingsToGlobalUsings,
        /// `dotnet ef migrations add`, for the selected project.
        AddMigration,
        /// `dotnet ef migrations remove`.
        RemoveMigration,
        /// `dotnet ef migrations list`.
        ListMigrations,
        /// `dotnet ef database update`.
        UpdateDatabase,
        AddProjectReference,
        RemoveReference,
        UpdatePackage,
        MoveUp,
        MoveDown,
        /// Moves the solution's package versions into Directory.Packages.props.
        CentralizePackageVersions,
        /// Writes the file templates to .forge/templates so they can be changed.
        CustomizeFileTemplates,
        ExpandSelected,
        CollapseSelected,
        ExtendSelectionUp,
        ExtendSelectionDown,
        SelectAllSiblings,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<SolutionExplorer>(window, cx);
        });
        workspace.register_action(|workspace, _: &RevealActiveFile, window, cx| {
            let file = active_file(workspace, cx);
            if let Some(panel) = workspace.focus_panel::<SolutionExplorer>(window, cx) {
                panel.update(cx, |panel, cx| {
                    if let Some(file) = file {
                        panel.reveal(&file, true, cx);
                    }
                });
            }
        });
        workspace.register_action(|workspace, _: &SelectSolution, window, cx| {
            if let Some(panel) = workspace.panel::<SolutionExplorer>(cx) {
                panel.update(cx, |panel, cx| panel.select_solution(&SelectSolution, window, cx));
            }
        });
        workspace.register_action(|workspace, _: &NewSolution, window, cx| {
            if let Some(panel) = workspace.panel::<SolutionExplorer>(cx) {
                panel.update(cx, |panel, cx| panel.new_solution(&NewSolution, window, cx));
            }
        });
        workspace.register_action(|workspace, _: &NewProject, window, cx| {
            if let Some(panel) = workspace.panel::<SolutionExplorer>(cx) {
                panel.update(cx, |panel, cx| panel.new_project(&NewProject, window, cx));
            }
        });
        workspace.register_action(|workspace, _: &ManagePackages, window, cx| {
            // From the menu: the whole solution. The panel handles it for its selection.
            if let Some(panel) = workspace.panel::<SolutionExplorer>(cx) {
                let model = panel.read(cx).model.clone();
                crate::nuget_view::open(workspace, model, None, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &CentralizePackageVersions, window, cx| {
            if let Some(panel) = workspace.panel::<SolutionExplorer>(cx) {
                panel.update(cx, |panel, cx| panel.centralize_packages(&CentralizePackageVersions, window, cx));
            }
        });
        workspace.register_action(|workspace, _: &CustomizeFileTemplates, window, cx| {
            if let Some(panel) = workspace.panel::<SolutionExplorer>(cx) {
                panel.update(cx, |panel, cx| panel.customize_templates(&CustomizeFileTemplates, window, cx));
            }
        });
    })
    .detach();

    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", menu::SelectPrevious, context),
        KeyBinding::new("down", menu::SelectNext, context),
        KeyBinding::new("left", CollapseSelected, context),
        KeyBinding::new("right", ExpandSelected, context),
        KeyBinding::new("enter", Open, context),
        KeyBinding::new("f2", Rename, context),
        KeyBinding::new("cmd-backspace", Delete, context),
        KeyBinding::new("delete", Delete, context),
        KeyBinding::new("cmd-c", Copy, context),
        KeyBinding::new("cmd-x", Cut, context),
        KeyBinding::new("cmd-v", Paste, context),
        KeyBinding::new("cmd-d", Duplicate, context),
        KeyBinding::new("cmd-alt-c", CopyPath, context),
        KeyBinding::new("cmd-n", NewFile, context),
        KeyBinding::new("cmd-shift-n", NewFolder, context),
        KeyBinding::new("cmd-shift-b", Build, context),
        KeyBinding::new("shift-up", ExtendSelectionUp, context),
        KeyBinding::new("shift-down", ExtendSelectionDown, context),
        KeyBinding::new("cmd-a", SelectAllSiblings, context),
        KeyBinding::new("alt-up", MoveUp, context),
        KeyBinding::new("alt-down", MoveDown, context),
    ]);
}

const KEY_CONTEXT: &str = "SolutionExplorer";
const PANEL_ICON: IconName = IconName::Blocks;

/// A file or folder the user copied or cut in the explorer.
#[derive(Clone, Debug)]
struct Clipboard {
    paths: Vec<PathBuf>,
    cut: bool,
}

/// What the inline name editor is for.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum EditKind {
    NewFile { dir: PathBuf, project: PathBuf, template: Option<dotnet_model::templates::Template>, anchor: Option<(PathBuf, dotnet_model::msbuild::edit::Position)> },
    NewFolder { dir: PathBuf, project: PathBuf },
    NewSolutionFolder { parent: Option<String> },
    Rename { path: PathBuf, project: Option<PathBuf> },
    RenameProject { path: PathBuf },
    RenameSolutionFolder { id: String },
    RenameSolution,
}

struct EditState {
    kind: EditKind,
    editor: Entity<Editor>,
    /// The row that is renamed, or the row new entries go under.
    row_id: String,
    is_new: bool,
    _subscription: Subscription,
}

/// One line of the tree, flattened.
#[derive(Clone, Debug)]
struct Row {
    id: String,
    depth: usize,
    kind: NodeKind,
    label: SharedString,
    detail: Option<SharedString>,
    expandable: bool,
    expanded: bool,
}

enum Line {
    Row(Row),
    /// Where the name of a new entry is typed.
    NewEntry { depth: usize, is_dir: bool },
}

/// What is dragged within the explorer.
#[derive(Clone, Debug)]
pub(crate) struct DraggedNode {
    /// Every dragged node: the selection when the row was part of it.
    nodes: Vec<(String, NodeKind)>,
    label: SharedString,
}

struct DraggedNodeView {
    label: SharedString,
}

impl Render for DraggedNodeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let colors = cx.theme().colors();
        h_flex()
            .px_2()
            .py_0p5()
            .rounded_sm()
            .bg(colors.elevated_surface_background)
            .border_1()
            .border_color(colors.border_focused)
            .child(Label::new(self.label.clone()).size(LabelSize::Small))
    }
}

pub struct SolutionExplorer {
    workspace: WeakEntity<Workspace>,
    pub(crate) model: Entity<DotnetModel>,
    focus_handle: FocusHandle,
    position: DockPosition,
    expanded: HashSet<String>,
    /// Nodes seen before; new ones open if they are expanded by default.
    seen: HashSet<String>,
    lines: Vec<Line>,
    /// Node id → (kind, parent id), for every node, shown or not. Rebuilt only when the
    /// model changes; expanding and collapsing only lays the visible rows out again.
    index: HashMap<String, (NodeKind, Option<String>)>,
    /// Files, solution items and projects by path, to reveal the active file.
    by_path: HashMap<PathBuf, String>,
    /// Visible row of each shown node.
    row_of: HashMap<String, usize>,
    /// The row the keyboard and single-item commands act on.
    selected: Option<String>,
    /// Every selected row, in the order they were picked (⌘-click, ⇧-click).
    marked: Vec<String>,
    edit: Option<EditState>,
    clipboard: Option<Clipboard>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    drag_target: Option<String>,
    scroll: UniformListScrollHandle,
    /// Project templates from `dotnet new list`, loaded on first use.
    pub(crate) project_templates: Option<Vec<dotnet_model::cli::ProjectTemplate>>,
    pub(crate) busy: Option<SharedString>,
    /// Changed files and the folders above them (see `git_status`).
    git: HashMap<PathBuf, git::status::GitSummary>,
    _subscriptions: Vec<Subscription>,
}

impl SolutionExplorer {
    pub fn new(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let model = cx.new(|cx| DotnetModel::new(project.clone(), cx));
        let model_subscription = cx.subscribe_in(&model, window, |this, _, event, _, cx| match event {
            ModelEvent::Changed => {
                this.reindex(cx);
                if config::get(cx).track_active_file
                    && let Some(file) = this.workspace.upgrade().and_then(|ws| active_file(ws.read(cx), cx)) {
                        this.reveal(&file, false, cx);
                    }
            }
        });
        let workspace_entity = workspace.weak_handle().upgrade().expect("the workspace is alive while it adds panels");
        let workspace_subscription = cx.subscribe_in(&workspace_entity, window, |this, workspace, event, _, cx| {
            if let workspace::Event::ActiveItemChanged = event
                && config::get(cx).track_active_file
                    && let Some(file) = active_file(workspace.read(cx), cx) {
                        this.reveal(&file, false, cx);
                    }
        });
        let git_store = project.read(cx).git_store().clone();
        let git_subscription = cx.subscribe(&git_store, |this, _, _: &project::git_store::GitStoreEvent, cx| {
            if let Some(project) = this.workspace.upgrade().map(|ws| ws.read(cx).project().clone()) {
                this.git = git_status::collect(&project, cx);
                cx.notify();
            }
        });
        let git = git_status::collect(&project, cx);
        Self {
            workspace: workspace.weak_handle(),
            model,
            focus_handle: cx.focus_handle(),
            position: forge_ui::dock_position::load(<Self as Panel>::panel_key(), cx).unwrap_or(DockPosition::Left),
            expanded: HashSet::new(),
            seen: HashSet::new(),
            lines: Vec::new(),
            index: HashMap::new(),
            by_path: HashMap::new(),
            row_of: HashMap::new(),
            selected: None,
            marked: Vec::new(),
            edit: None,
            clipboard: None,
            context_menu: None,
            drag_target: None,
            scroll: UniformListScrollHandle::new(),
            project_templates: None,
            busy: None,
            git,
            _subscriptions: vec![model_subscription, workspace_subscription, git_subscription],
        }
    }

    pub fn model(&self) -> &Entity<DotnetModel> {
        &self.model
    }

    /// Indexes the model's whole tree, then lays out the visible rows.
    fn reindex(&mut self, cx: &mut Context<Self>) {
        let tree = self.model.read(cx).tree.clone();
        self.index.clear();
        self.by_path.clear();
        if let Some(tree) = &tree {
            let mut stack: Vec<(&Node, Option<&str>)> = vec![(tree, None)];
            while let Some((node, parent)) = stack.pop() {
                if node.expanded_by_default && self.seen.insert(node.id.clone()) {
                    self.expanded.insert(node.id.clone());
                }
                if let NodeKind::File { path, .. } | NodeKind::SolutionItem { path, .. } | NodeKind::Project { path } = &node.kind {
                    self.by_path.entry(path.clone()).or_insert_with(|| node.id.clone());
                }
                self.index.insert(node.id.clone(), (node.kind.clone(), parent.map(str::to_string)));
                stack.extend(node.children.iter().map(|child| (child, Some(node.id.as_str()))));
            }
        }
        if self.selected.as_ref().is_some_and(|id| !self.index.contains_key(id)) {
            self.selected = None;
        }
        self.marked.retain(|id| self.index.contains_key(id));
        self.relayout(cx);
    }

    /// Lays out the visible rows from the tree and what is expanded.
    fn relayout(&mut self, cx: &mut Context<Self>) {
        let tree = self.model.read(cx).tree.clone();
        self.lines.clear();
        if let Some(tree) = tree {
            self.flatten(&tree, 0);
        }
        if let Some(edit) = &self.edit
            && edit.is_new
            && let Some(at) = self.lines.iter().position(|l| matches!(l, Line::Row(r) if r.id == edit.row_id))
        {
            let depth = match &self.lines[at] {
                Line::Row(r) => r.depth + 1,
                _ => 0,
            };
            let is_dir = matches!(edit.kind, EditKind::NewFolder { .. } | EditKind::NewSolutionFolder { .. });
            self.lines.insert(at + 1, Line::NewEntry { depth, is_dir });
        }
        self.row_of = self.lines.iter().enumerate().filter_map(|(ix, l)| if let Line::Row(r) = l { Some((r.id.clone(), ix)) } else { None }).collect();
        cx.notify();
    }

    fn flatten(&mut self, node: &Node, depth: usize) {
        let expanded = self.expanded.contains(&node.id);
        self.lines.push(Line::Row(Row {
            id: node.id.clone(),
            depth,
            kind: node.kind.clone(),
            label: node.label.clone().into(),
            detail: node.detail.clone().map(Into::into),
            expandable: !node.children.is_empty(),
            expanded,
        }));
        if expanded {
            for child in &node.children {
                self.flatten(child, depth + 1);
            }
        }
    }

    /// The selected rows' kinds: every marked row when several are, else the selected one.
    pub(crate) fn selected_kinds(&self) -> Vec<NodeKind> {
        if self.marked.len() > 1 && self.selected.as_ref().is_some_and(|s| self.marked.contains(s)) {
            self.marked.iter().filter_map(|id| self.index.get(id)).map(|(kind, _)| kind.clone()).collect()
        } else {
            self.selected_kind().into_iter().collect()
        }
    }

    pub(crate) fn is_multi_selection(&self) -> bool {
        self.selected_kinds().len() > 1
    }

    /// A click on a row: ⌘ toggles it in the selection, ⇧ extends to it.
    fn click_select(&mut self, id: String, modifiers: gpui::Modifiers) {
        if modifiers.secondary() {
            if let Some(at) = self.marked.iter().position(|m| *m == id) {
                self.marked.remove(at);
            } else {
                if self.marked.is_empty()
                    && let Some(current) = self.selected.clone()
                {
                    self.marked.push(current);
                }
                self.marked.push(id.clone());
            }
        } else if modifiers.shift
            && let Some(anchor) = self.selected.as_ref().and_then(|s| self.row_of.get(s)).copied()
            && let Some(target) = self.row_of.get(&id).copied()
        {
            let range = anchor.min(target)..=anchor.max(target);
            self.marked = self.lines[range].iter().filter_map(|l| if let Line::Row(r) = l { Some(r.id.clone()) } else { None }).collect();
            // Keep the anchor where it was, so the next ⇧-click extends from it.
            return;
        } else {
            self.marked.clear();
        }
        self.selected = Some(id);
    }

    pub(crate) fn selected_kind(&self) -> Option<NodeKind> {
        self.selected.as_ref().and_then(|id| self.index.get(id)).map(|(kind, _)| kind.clone())
    }

    fn row_index(&self, id: &str) -> Option<usize> {
        self.row_of.get(id).copied()
    }

    fn select(&mut self, id: String, cx: &mut Context<Self>) {
        if let Some(ix) = self.row_index(&id) {
            self.scroll.scroll_to_item(ix, gpui::ScrollStrategy::Center);
        }
        self.marked.clear();
        self.selected = Some(id);
        cx.notify();
    }

    fn toggle(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_string());
        }
        self.relayout(cx);
    }

    /// Expands the way to a file and selects it.
    pub fn reveal(&mut self, file: &Path, scroll: bool, cx: &mut Context<Self>) {
        let Some(target) = self.by_path.get(file).cloned() else { return };
        if self.selected.as_ref() == Some(&target) {
            return;
        }
        let mut parent = self.index.get(&target).and_then(|(_, p)| p.clone());
        let mut opened = false;
        while let Some(id) = parent {
            opened |= self.expanded.insert(id.clone());
            parent = self.index.get(&id).and_then(|(_, p)| p.clone());
        }
        if opened {
            self.relayout(cx);
        }
        self.marked.clear();
        self.selected = Some(target);
        if let Some(ix) = self.selected.as_ref().and_then(|id| self.row_index(id)) {
            self.scroll.scroll_to_item(ix, if scroll { gpui::ScrollStrategy::Center } else { gpui::ScrollStrategy::Top });
        }
        cx.notify();
    }

    /// Grows the selection by one row up or down (⇧↑, ⇧↓).
    fn extend_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(current) = self.selected.clone() else { return self.move_selection(delta, cx) };
        let Some(ix) = self.row_of.get(&current).copied() else { return };
        let target = (ix as isize + delta).clamp(0, self.lines.len() as isize - 1) as usize;
        let Some(Line::Row(row)) = self.lines.get(target) else { return };
        let id = row.id.clone();
        if self.marked.is_empty() {
            self.marked.push(current);
        }
        if let Some(at) = self.marked.iter().position(|m| *m == id) {
            // Going back over a marked row unmarks the one being left.
            if let Some(left) = self.marked.iter().position(|m| *m == *self.selected.as_ref().unwrap()) {
                if at + 1 == left || left + 1 == at {
                    self.marked.remove(left);
                }
            }
        } else {
            self.marked.push(id.clone());
        }
        self.selected = Some(id);
        self.scroll.scroll_to_item(target, gpui::ScrollStrategy::Center);
        cx.notify();
    }

    fn extend_down(&mut self, _: &ExtendSelectionDown, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_selection(1, cx);
    }

    fn extend_up(&mut self, _: &ExtendSelectionUp, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_selection(-1, cx);
    }

    fn select_all(&mut self, _: &SelectAllSiblings, _: &mut Window, cx: &mut Context<Self>) {
        // Every row under the selected row's parent.
        let Some(parent) = self.selected.as_ref().and_then(|s| self.index.get(s)).and_then(|(_, p)| p.clone()) else { return };
        self.marked = self
            .lines
            .iter()
            .filter_map(|l| if let Line::Row(r) = l { Some(&r.id) } else { None })
            .filter(|id| self.index.get(*id).and_then(|(_, p)| p.as_ref()) == Some(&parent))
            .cloned()
            .collect();
        cx.notify();
    }

    /// Moves the selection by `delta` visible rows.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows: Vec<&Row> = self.lines.iter().filter_map(|l| if let Line::Row(r) = l { Some(r) } else { None }).collect();
        if rows.is_empty() {
            return;
        }
        let target = match self.selected.as_ref().and_then(|id| rows.iter().position(|r| &r.id == id)) {
            Some(ix) => (ix as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
            None => 0,
        };
        let id = rows[target].id.clone();
        self.select(id, cx);
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn select_previous(&mut self, _: &menu::SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn expand_selected(&mut self, _: &ExpandSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else { return };
        let expandable = self.row_of.get(&id).and_then(|ix| self.lines.get(*ix)).is_some_and(|l| matches!(l, Line::Row(r) if r.expandable));
        if expandable && !self.expanded.contains(&id) {
            self.toggle(&id, cx);
        } else {
            self.move_selection(1, cx);
        }
    }

    fn collapse_selected(&mut self, _: &CollapseSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else { return };
        if self.expanded.contains(&id) {
            self.toggle(&id, cx);
        } else if let Some(parent) = self.index.get(&id).and_then(|(_, p)| p.clone()) {
            self.select(parent, cx);
        }
    }

    fn collapse_all(&mut self, _: &CollapseAll, _: &mut Window, cx: &mut Context<Self>) {
        let root = self.lines.iter().find_map(|l| if let Line::Row(r) = l { Some(r.id.clone()) } else { None });
        self.expanded.clear();
        if let Some(root) = root {
            self.expanded.insert(root);
        }
        self.relayout(cx);
    }

    fn refresh(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.reload(true, cx));
    }

    fn deploy_context_menu(&mut self, position: Point<Pixels>, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.marked.contains(&id) {
            self.marked.clear();
        }
        self.selected = Some(id.clone());
        let Some(kind) = self.selected_kind() else { return };
        let menu = if self.is_multi_selection() { self.build_multi_menu(window, cx) } else { self.build_context_menu(&kind, window, cx) };
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    /// Starts typing a name inline, for a new entry under `row_id` or a rename of it.
    pub(crate) fn start_edit(&mut self, kind: EditKind, row_id: String, initial: &str, is_new: bool, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(initial, window, cx);
            // Select the name without its extension, like the project panel.
            let stem_len = if is_new { initial.len() } else { initial.rfind('.').filter(|&i| i > 0).unwrap_or(initial.len()) };
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges([editor::MultiBufferOffset(0)..editor::MultiBufferOffset(stem_len)])
            });
            editor
        });
        let subscription = cx.subscribe_in(&editor, window, |this, _, event, window, cx| {
            if let EditorEvent::Blurred = event {
                this.confirm_edit(window, cx);
            }
        });
        if is_new {
            self.expanded.insert(row_id.clone());
        }
        self.edit = Some(EditState { kind, editor: editor.clone(), row_id: row_id.clone(), is_new, _subscription: subscription });
        self.relayout(cx);
        if let Some(ix) = self.row_index(&row_id) {
            self.scroll.scroll_to_item(ix + usize::from(is_new), gpui::ScrollStrategy::Center);
        }
        window.focus(&editor.focus_handle(cx), cx);
    }

    fn confirm_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.edit.take() else { return };
        let name = edit.editor.read(cx).text(cx).trim().to_string();
        self.relayout(cx);
        window.focus(&self.focus_handle, cx);
        if name.is_empty() {
            return;
        }
        self.apply_edit(edit.kind, name, window, cx);
    }

    fn cancel_edit(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit.take().is_some() {
            self.relayout(cx);
            window.focus(&self.focus_handle, cx);
        }
    }

    fn render_line(&self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match &self.lines[ix] {
            Line::Row(row) => self.render_row(row, window, cx),
            Line::NewEntry { depth, is_dir } => {
                let editor = self.edit.as_ref().map(|e| e.editor.clone());
                ListItem::new("new-entry")
                    .indent_level(*depth)
                    .indent_step_size(px(12.))
                    .spacing(ListItemSpacing::ExtraDense)
                    .start_slot(Icon::new(if *is_dir { IconName::Folder } else { IconName::File }).size(IconSize::Small).color(Color::Muted))
                    .child(div().w_full().h(px(20.)).children(editor))
                    .into_any_element()
            }
        }
    }

    fn row_icon(&self, row: &Row, cx: &App) -> Icon {
        let path_icon = |path: &Path| file_icons::FileIcons::get_icon(path, cx).map(Icon::from_path);
        match &row.kind {
            NodeKind::Solution => Icon::from_path("icons/forge_solution.svg").color(Color::Accent),
            NodeKind::SolutionFolder { .. } => Icon::new(if row.expanded { IconName::FolderOpen } else { IconName::Folder }).color(Color::Muted),
            NodeKind::SolutionItem { path, .. } => path_icon(path).unwrap_or_else(|| Icon::new(IconName::File)),
            NodeKind::Project { .. } => Icon::from_path("icons/forge_project.svg").color(Color::Accent),
            NodeKind::ProjectError { .. } => Icon::new(IconName::Warning).color(Color::Warning),
            NodeKind::Dependencies { .. } => Icon::new(IconName::Book).color(Color::Muted),
            NodeKind::Frameworks { .. } | NodeKind::Framework { .. } => Icon::new(IconName::Box).color(Color::Muted),
            NodeKind::Packages { .. } | NodeKind::Package { .. } | NodeKind::PackageDependency { .. } => Icon::from_path("icons/forge_nuget.svg").color(Color::Muted),
            NodeKind::ProjectReferences { .. } | NodeKind::ProjectReference { .. } => Icon::from_path("icons/forge_project.svg").color(Color::Muted),
            NodeKind::Assemblies { .. } | NodeKind::Assembly { .. } => Icon::new(IconName::Binary).color(Color::Muted),
            NodeKind::Folder { path, is_link, .. } => {
                let icon = file_icons::FileIcons::get_folder_icon(row.expanded, path, cx).map(Icon::from_path).unwrap_or_else(|| Icon::new(IconName::Folder));
                if *is_link { icon.color(Color::Muted) } else { icon }
            }
            NodeKind::File { path, .. } => path_icon(path).unwrap_or_else(|| Icon::new(IconName::File)),
        }
    }

    fn render_row(&self, row: &Row, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let selected = self.selected.as_ref() == Some(&row.id) || self.marked.contains(&row.id);
        let renaming = self.edit.as_ref().filter(|e| !e.is_new && e.row_id == row.id).map(|e| e.editor.clone());
        let id = row.id.clone();
        let click_id = row.id.clone();
        let toggle_id = row.id.clone();
        let menu_id = row.id.clone();
        let drop_id = row.id.clone();
        let drag_target = self.drag_target.as_ref() == Some(&row.id);
        let dimmed = matches!(&row.kind, NodeKind::File { missing: true, .. } | NodeKind::ProjectError { .. })
            || matches!(&row.kind, NodeKind::File { is_link: true, .. } | NodeKind::Folder { is_link: true, .. });
        let is_cut = self.clipboard.as_ref().is_some_and(|c| c.cut && row.kind.path().is_some_and(|p| c.paths.iter().any(|x| x == p)));
        let draggable = matches!(row.kind, NodeKind::File { .. } | NodeKind::Folder { .. } | NodeKind::SolutionFolder { .. } | NodeKind::Project { .. } | NodeKind::SolutionItem { .. });
        let dragged = if self.marked.len() > 1 && self.marked.contains(&row.id) {
            let nodes: Vec<(String, NodeKind)> = self.marked.iter().filter_map(|id| self.index.get(id).map(|(k, _)| (id.clone(), k.clone()))).collect();
            DraggedNode { label: format!("{} items", nodes.len()).into(), nodes }
        } else {
            DraggedNode { nodes: vec![(row.id.clone(), row.kind.clone())], label: row.label.clone() }
        };

        let git_path = match &row.kind {
            NodeKind::Solution => self.model.read(cx).solution.as_ref().map(|s| s.dir().to_path_buf()),
            NodeKind::File { .. } | NodeKind::Folder { .. } | NodeKind::SolutionItem { .. } | NodeKind::Project { .. } => {
                row.kind.path().map(|p| git_status::row_path(p, matches!(row.kind, NodeKind::Project { .. })).to_path_buf())
            }
            _ => None,
        };
        let git = git_path.and_then(|p| self.git.get(&p)).copied();
        let git_color = git.as_ref().and_then(git_status::color);
        let is_file = matches!(row.kind, NodeKind::File { .. } | NodeKind::SolutionItem { .. });
        let git_mark: Option<SharedString> = git.as_ref().and_then(|g| if is_file { git_status::letter(g).map(Into::into) } else { git_color.map(|_| "•".into()) });

        let label: AnyElement = match renaming {
            Some(editor) => div().flex_1().h(px(20.)).child(editor).into_any_element(),
            None => h_flex()
                .gap_1p5()
                .min_w_0()
                .child(
                    // The project panel's size, so both explorers read the same.
                    Label::new(row.label.clone())
                        .single_line()
                        .when(matches!(row.kind, NodeKind::Solution | NodeKind::Project { .. }), |l| l.weight(FontWeight::SEMIBOLD))
                        .when_some(git_color, |l, c| l.color(c))
                        .when(dimmed || is_cut, |l| l.color(Color::Muted)),
                )
                .children(row.detail.clone().map(|d| Label::new(d).size(LabelSize::XSmall).color(Color::Muted).single_line()))
                .into_any_element(),
        };

        div()
            .id(SharedString::from(format!("row-{id}")))
            .w_full()
            .when(drag_target, |el| el.bg(colors.drop_target_background))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.is_right_click() {
                    return;
                }
                cx.stop_propagation();
                window.focus(&this.focus_handle, cx);
                let modifiers = event.modifiers();
                this.click_select(click_id.clone(), modifiers);
                if !modifiers.secondary() && !modifiers.shift {
                    this.activate(&click_id, event.click_count(), window, cx);
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.deploy_context_menu(event.position, menu_id.clone(), window, cx);
                }),
            )
            .when(draggable, |el| {
                el.on_drag(dragged, |dragged, _, _, cx| cx.new(|_| DraggedNodeView { label: dragged.label.clone() }))
            })
            .drag_over::<DraggedNode>(move |style, _, _, cx| style.bg(cx.theme().colors().drop_target_background))
            .on_drop(cx.listener(move |this, dragged: &DraggedNode, window, cx| {
                this.drag_target = None;
                this.drop_on(dragged.clone(), &drop_id, window, cx);
            }))
            .child(
                ListItem::new(SharedString::from(id.clone()))
                    .indent_level(row.depth)
                    .indent_step_size(px(12.))
                    .spacing(ListItemSpacing::ExtraDense)
                    .toggle_state(selected)
                    .when(row.expandable, |item| {
                        // Zed shows an open node's chevron only on hover; always, here.
                        item.toggle(Some(row.expanded)).always_show_disclosure_icon(true).on_toggle(cx.listener(move |this, _, _, cx| {
                            this.toggle(&toggle_id, cx);
                        }))
                    })
                    .start_slot(self.row_icon(row, cx).size(IconSize::Small))
                    .child(label)
                    .end_slot::<AnyElement>(git_mark.map(|mark| {
                        div().pr_2().child(Label::new(mark).size(LabelSize::Small).color(git_color.unwrap_or(Color::Muted))).into_any_element()
                    })),
            )
            .into_any_element()
    }

    /// Clicking a row: folders and nodes with children toggle; files open.
    fn activate(&mut self, id: &str, click_count: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, _)) = self.index.get(id).cloned() else { return };
        match &kind {
            NodeKind::File { path, missing: false, .. } | NodeKind::SolutionItem { path, .. } => self.open_file(path.clone(), click_count > 1, window, cx),
            NodeKind::Project { path } if config::get(cx).open_project_on_click || click_count > 1 => self.open_file(path.clone(), click_count > 1, window, cx),
            NodeKind::ProjectReference { target, .. } if click_count > 1 => self.open_file(target.clone(), true, window, cx),
            NodeKind::ProjectError { message, .. } if click_count > 1 => self.show_error(message.clone(), cx),
            _ => {
                let expandable = self.row_of.get(id).and_then(|ix| self.lines.get(*ix)).is_some_and(|l| matches!(l, Line::Row(r) if r.expandable));
                if expandable {
                    self.toggle(id, cx);
                }
            }
        }
    }

    pub(crate) fn open_file(&mut self, path: PathBuf, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        forge_ui::pick::defer_workspace(self.workspace.clone(), window, cx, move |workspace, window, cx| {
            let options = workspace::OpenOptions { focus: Some(focus), visible: Some(workspace::OpenVisible::None), ..Default::default() };
            let open = workspace.open_abs_path(path, options, window, cx);
            cx.spawn(async move |_, cx| {
                if let Err(error) = open.await {
                    this.update(cx, |this, cx| this.show_error(format!("{error:#}"), cx)).ok();
                }
            })
            .detach();
        });
    }

    fn open(&mut self, _: &Open, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.selected.clone() {
            self.activate(&id, 2, window, cx);
        }
    }

    pub(crate) fn show_error(&self, message: String, cx: &mut Context<Self>) {
        struct SolutionExplorerError;
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(workspace::Toast::new(workspace::notifications::NotificationId::unique::<SolutionExplorerError>(), message), cx);
            });
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let colors = cx.theme().colors();
        let model = self.model.read(cx);
        let loading = model.loading;
        h_flex()
            .justify_between()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(forge_ui::panel_grip("solution-explorer-grip", Arc::new(cx.entity()), "Solution", Some(PANEL_ICON)))
                    .child(Label::new("Solution Explorer").weight(FontWeight::BOLD).single_line()),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("manage-packages", IconName::Book)
                            .disabled(model.solution.is_none())
                            .tooltip(Tooltip::text("Manage NuGet packages"))
                            .on_click(cx.listener(|this, _, window, cx| this.manage_packages(&ManagePackages, window, cx))),
                    )
                    .child(
                        IconButton::new("reveal", IconName::Crosshair)
                            .tooltip(Tooltip::text("Select the active file"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(file) = this.workspace.upgrade().and_then(|ws| active_file(ws.read(cx), cx)) {
                                    this.reveal(&file, true, cx);
                                }
                            })),
                    )
                    .child(
                        IconButton::new("collapse-all", IconName::ListCollapse)
                            .tooltip(Tooltip::text("Collapse all"))
                            .on_click(cx.listener(|this, _, window, cx| this.collapse_all(&CollapseAll, window, cx))),
                    )
                    .child(
                        IconButton::new("refresh", IconName::RotateCw)
                            .disabled(loading)
                            .tooltip(Tooltip::text("Reload the solution"))
                            .on_click(cx.listener(|this, _, window, cx| this.refresh(&Refresh, window, cx))),
                    ),
            )
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let model = self.model.read(cx);
        let message = if model.loading {
            "Looking for solutions…".to_string()
        } else if let Some(error) = &model.error {
            format!("The solution could not be read: {error}")
        } else {
            "No .NET solution or project in this workspace.".to_string()
        };
        let loading = model.loading;
        v_flex()
            .p_4()
            .gap_2()
            .child(Label::new(message).color(Color::Muted))
            .when(!loading, |col| {
                col.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("new-solution", "New Solution…")
                                .style(ButtonStyle::Filled)
                                .size(ButtonSize::Compact)
                                .on_click(cx.listener(|this, _, window, cx| this.new_solution(&NewSolution, window, cx))),
                        )
                        .child(
                            Button::new("new-project", "New Project…")
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .on_click(cx.listener(|this, _, window, cx| this.new_project(&NewProject, window, cx))),
                        ),
                )
            })
            .into_any_element()
    }
}

/// The file of the active editor, if any.
pub(crate) fn active_file(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let item = workspace.active_item(cx)?;
    let project_path = item.project_path(cx)?;
    workspace.project().read(cx).absolute_path(&project_path, cx)
}

impl Render for SolutionExplorer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let colors = cx.theme().colors().clone();
        let count = self.lines.len();
        let busy = self.busy.clone();
        let body: AnyElement = if count == 0 {
            self.render_empty(cx)
        } else {
            v_flex()
                .flex_shrink_1()
                .size_full()
                .child(
                    uniform_list(
                        "solution-explorer-rows",
                        count,
                        cx.processor(|this, range: Range<usize>, window, cx| range.map(|ix| this.render_line(ix, window, cx)).collect()),
                    )
                    .flex_shrink_1()
                    .size_full()
                    .track_scroll(&self.scroll),
                )
                .vertical_scrollbar_for(&self.scroll, window, cx)
                .into_any_element()
        };
        v_flex()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::expand_selected))
            .on_action(cx.listener(Self::extend_up))
            .on_action(cx.listener(Self::extend_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::collapse_selected))
            .on_action(cx.listener(Self::collapse_all))
            .on_action(cx.listener(Self::refresh))
            .on_action(cx.listener(Self::open))
            .on_action(cx.listener(Self::cancel_edit))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.confirm_edit(window, cx)))
            .on_action(cx.listener(Self::new_file))
            .on_action(cx.listener(Self::new_folder))
            .on_action(cx.listener(Self::new_solution_folder))
            .on_action(cx.listener(Self::add_solution_item))
            .on_action(cx.listener(Self::rename))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::remove_from_solution))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::duplicate))
            .on_action(cx.listener(Self::copy_path))
            .on_action(cx.listener(Self::copy_relative_path))
            .on_action(cx.listener(Self::reveal_in_finder))
            .on_action(cx.listener(Self::open_in_terminal))
            .on_action(cx.listener(Self::build))
            .on_action(cx.listener(Self::rebuild_project))
            .on_action(cx.listener(Self::clean))
            .on_action(cx.listener(Self::restore))
            .on_action(cx.listener(Self::test))
            .on_action(cx.listener(Self::run))
            .on_action(cx.listener(Self::watch))
            .on_action(cx.listener(Self::pack))
            .on_action(cx.listener(Self::publish))
            .on_action(cx.listener(Self::manage_packages))
            .on_action(cx.listener(Self::manage_user_secrets))
            .on_action(cx.listener(Self::move_usings_to_global_usings))
            .on_action(cx.listener(Self::add_migration))
            .on_action(cx.listener(Self::remove_migration))
            .on_action(cx.listener(Self::list_migrations))
            .on_action(cx.listener(Self::update_database))
            .on_action(cx.listener(Self::add_project_reference))
            .on_action(cx.listener(Self::remove_reference))
            .on_action(cx.listener(Self::update_package))
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::add_existing_project))
            .on_action(cx.listener(Self::new_project))
            .on_action(cx.listener(Self::new_solution))
            .on_action(cx.listener(Self::select_solution))
            .on_action(cx.listener(Self::centralize_packages))
            .on_action(cx.listener(Self::customize_templates))
            .size_full()
            .bg(colors.panel_background)
            .child(self.render_header(cx))
            .children(busy.map(|message| {
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(Icon::new(IconName::ArrowCircle).size(IconSize::Small).color(Color::Accent))
                    .child(Label::new(message).size(LabelSize::Small).color(Color::Muted))
            }))
            .child(
                v_flex()
                    .id("solution-explorer-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            // Right-click on empty space: the solution's menu.
                            let root = this.lines.iter().find_map(|l| if let Line::Row(r) = l { Some(r.id.clone()) } else { None });
                            if let Some(root) = root {
                                this.deploy_context_menu(event.position, root, window, cx);
                            }
                        }),
                    )
                    .child(body),
            )
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(anchored().position(*position).anchor(gpui::Anchor::TopLeft).child(menu.clone())).with_priority(3)
            }))
    }
}

impl Focusable for SolutionExplorer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for SolutionExplorer {}

impl Panel for SolutionExplorer {
    fn persistent_name() -> &'static str {
        "ForgeSolutionExplorer"
    }
    fn panel_key() -> &'static str {
        "ForgeSolutionExplorer"
    }
    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }
    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }
    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        forge_ui::dock_position::save(Self::panel_key(), position, cx);
        gpui::BorrowAppContext::update_global::<settings::SettingsStore, _>(cx, |_, _| {});
        cx.notify();
    }
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(300.)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(PANEL_ICON)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Solution Explorer")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }
    fn activation_priority(&self) -> u32 {
        // Forge panels use 10–19; Zed's use 1–7 and extension slots 8 and 20+.
        14
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dotnet_model::solution::Solution;
    use project::PathChange;
    use gpui::{TestAppContext, VisualTestContext, point, size};
    use std::time::{Duration, Instant};

    async fn panel_for(root: &Path, cx: &mut TestAppContext) -> (Entity<SolutionExplorer>, VisualTestContext) {
        cx.executor().allow_parking();
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(root, serde_json::json!({})).await;
        cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
        let project = project::Project::test(fs, [root], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let panel = workspace.update_in(&mut cx, |ws, window, cx| {
            let panel = cx.new(|cx| SolutionExplorer::new(ws, window, cx));
            ws.add_panel(panel.clone(), window, cx);
            panel
        });
        (panel, cx)
    }

    async fn wait_for(cx: &mut VisualTestContext, panel: &Entity<SolutionExplorer>, what: &str, pred: impl Fn(&SolutionExplorer, &App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            cx.executor().advance_clock(Duration::from_millis(500));
            cx.run_until_parked();
            if panel.read_with(cx, |p, cx| pred(p, cx)) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn labels(panel: &SolutionExplorer) -> Vec<String> {
        panel.lines.iter().filter_map(|l| if let Line::Row(r) = l { Some(format!("{}{}", "  ".repeat(r.depth), r.label)) } else { None }).collect()
    }

    fn draw(panel: &Entity<SolutionExplorer>, cx: &mut VisualTestContext) {
        let panel = panel.clone();
        cx.draw(point(px(0.), px(0.)), size(px(400.), px(800.)), move |_, _| div().size_full().child(panel));
    }

    fn write(root: &Path, name: &str, text: &str) {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Commands that reach other panels run after the explorer's own update.
    #[gpui::test]
    async fn test_command_reaches_the_tests_panel(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write(&root, "All.sln", include_str!("../../../dotnet-model/tests/fixtures/sample.sln"));
        write(&root, "src/App/App.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\" />\n");
        write(&root, "tests/Tests/Tests.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\"><ItemGroup><PackageReference Include=\"Microsoft.NET.Test.Sdk\" /></ItemGroup></Project>\n");
        let (panel, mut cx) = panel_for(&root, cx).await;
        let workspace = panel.read_with(&cx, |p, _| p.workspace.upgrade().unwrap());
        workspace.update_in(&mut cx, |ws, window, cx| {
            let tests = cx.new(|cx| forge_tests::TestPanel::new(ws, window, cx));
            ws.add_panel(tests, window, cx);
        });
        wait_for(&mut cx, &panel, "the solution", |p, _| !p.lines.is_empty()).await;
        let tests = root.join("tests/Tests/Tests.csproj");
        panel.update_in(&mut cx, |p, window, cx| {
            p.selected = Some(format!("project:{}", tests.display()));
            p.test(&Test, window, cx);
        });
        cx.run_until_parked();
        panel.update_in(&mut cx, |p, window, cx| {
            p.selected = None;
            p.test(&Test, window, cx);
        });
        cx.run_until_parked();
    }

    /// A C# project's menu moves the usings of all its files to its global usings file,
    /// and shows the changes in a tab to review.
    #[gpui::test]
    async fn moves_a_projects_usings_to_its_global_usings(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let files = [
            ("App.sln", include_str!("../../../dotnet-model/tests/fixtures/sample.sln")),
            ("src/App/App.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\" />\n"),
            ("src/App/Program.cs", "using System;\n\nConsole.WriteLine();\n"),
            ("src/App/Models/User.cs", "using System.Text;\nnamespace App.Models;\n"),
        ];
        for (name, text) in files {
            write(&root, name, text);
        }
        let (panel, mut cx) = panel_for(&root, cx).await;
        let workspace = panel.read_with(&cx, |p, _| p.workspace.upgrade().unwrap());
        // The buffers come from the project's fs.
        let fs = workspace.read_with(&cx, |ws, cx| ws.project().read(cx).fs().clone());
        for (name, text) in files {
            let path = root.join(name);
            fs.create_dir(path.parent().unwrap()).await.unwrap();
            fs.write(&path, text.as_bytes()).await.unwrap();
        }
        cx.update(|_, cx| cx.set_global(crate::config::DotnetConfig { global_usings_file: "_Imports.cs".into(), ..Default::default() }));
        wait_for(&mut cx, &panel, "the solution", |p, _| !p.lines.is_empty()).await;

        let app = root.join("src/App/App.csproj");
        panel.update_in(&mut cx, |p, window, cx| {
            p.selected = Some(format!("project:{}", app.display()));
            p.move_usings_to_global_usings(&MoveUsingsToGlobalUsings, window, cx);
        });
        cx.run_until_parked();
        let review = workspace.read_with(&cx, |ws, cx| ws.active_item(cx).and_then(|i| i.downcast::<Editor>())).expect("a review tab");
        assert_eq!(review.read_with(&cx, |e, cx| e.buffer().read(cx).title(cx).to_string()), "Usings of App → _Imports.cs");
        let text = |path: &str, cx: &mut VisualTestContext| {
            let project = workspace.read_with(cx, |ws, _| ws.project().clone());
            let open = project.update(cx, |p, cx| p.open_local_buffer(root.join(path), cx));
            async move { open.await.unwrap() }
        };
        let imports = text("src/App/_Imports.cs", &mut cx).await;
        assert_eq!(imports.read_with(&cx, |b, _| b.text()), "global using System;\nglobal using System.Text;\n");
        let user = text("src/App/Models/User.cs", &mut cx).await;
        assert_eq!(user.read_with(&cx, |b, _| b.text()), "namespace App.Models;\n");
    }

    #[gpui::test]
    async fn multiple_selection_and_targeted_reloads(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write(&root, "All.sln", include_str!("../../../dotnet-model/tests/fixtures/sample.sln"));
        write(&root, "src/App/App.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\" />\n");
        write(&root, "src/App/A.cs", "");
        write(&root, "src/App/B.cs", "");
        write(&root, "src/App/C.cs", "");
        write(&root, "src/App/Target/.keep", "");
        write(&root, "tests/Tests/Tests.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\" />\n");
        let (panel, mut cx) = panel_for(&root, cx).await;
        wait_for(&mut cx, &panel, "the solution", |p, _| !p.lines.is_empty()).await;
        let app = root.join("src/App/App.csproj");
        panel.update(&mut cx, |p, cx| p.toggle(&format!("project:{}", app.display()), cx));

        let file_id = |name: &str| format!("file:{}:{name}", app.display());
        panel.update(&mut cx, |p, _| {
            p.click_select(file_id("A.cs"), gpui::Modifiers::default());
            p.click_select(file_id("C.cs"), gpui::Modifiers::secondary_key());
            assert_eq!(p.selected_kinds().len(), 2, "⌘-click adds to the selection");
            p.click_select(file_id("A.cs"), gpui::Modifiers::default());
            p.click_select(file_id("C.cs"), gpui::Modifiers::shift());
            assert_eq!(p.selected_kinds().len(), 3, "⇧-click selects the range A..C");
            assert_eq!(p.selected_paths().len(), 3);
        });

        // Dragging the selection moves all of it, in one job.
        panel.update_in(&mut cx, |p, window, cx| {
            let nodes: Vec<(String, NodeKind)> = p.marked.iter().map(|id| (id.clone(), p.index[id].0.clone())).collect();
            p.drop_on(DraggedNode { nodes, label: "3 items".into() }, &format!("dir:{}:Target", app.display()), window, cx)
        });
        wait_for(&mut cx, &panel, "the move", |_, _| ["A.cs", "B.cs", "C.cs"].iter().all(|f| root.join("src/App/Target").join(f).exists())).await;

        // File events reload only what they touch; the initial scan reloads nothing.
        let model = panel.read_with(&cx, |p, _| p.model.clone());
        model.update(&mut cx, |m, _| {
            assert!(!m.note_changes(&[(root.join("src/App/Program.cs"), PathChange::Loaded)]));
            assert!(!m.note_changes(&[(root.join("src/App/bin/Debug/App.dll"), PathChange::Added)]));
            assert!(m.note_changes(&[(root.join("src/App/New.cs"), PathChange::Added)]));
            assert!(m.note_changes(&[(root.join("tests/Tests/Tests.csproj"), PathChange::Updated)]));
            assert!(!m.note_changes(&[(root.join("src/App/Target/A.cs"), PathChange::Updated)]), "edits to code files change nothing");
            assert_eq!(m.pending(), (false, vec![root.join("tests/Tests/Tests.csproj")], vec![app.clone()]));
            assert!(m.note_changes(&[(root.join("All.sln"), PathChange::Updated)]));
            assert!(m.pending().0);
        });
    }

    /// `FORGE_DOTNET_BIG_SOLUTION=/path/Big.sln cargo test -p forge-dotnet big_solution -- --ignored --nocapture`
    #[gpui::test]
    #[ignore]
    async fn big_solution_stays_responsive(cx: &mut TestAppContext) {
        let Ok(solution) = std::env::var("FORGE_DOTNET_BIG_SOLUTION") else { return };
        let root = PathBuf::from(solution).parent().unwrap().to_path_buf();
        let started = Instant::now();
        let (panel, mut cx) = panel_for(&root, cx).await;
        wait_for(&mut cx, &panel, "the solution", |p, _| p.index.len() > 1000).await;
        println!("loaded {} nodes in {:?}", panel.read_with(&cx, |p, _| p.index.len()), started.elapsed());
        let projects: Vec<String> = panel.read_with(&cx, |p, _| p.index.iter().filter(|(_, (k, _))| matches!(k, NodeKind::Project { .. })).map(|(id, _)| id.clone()).collect());
        let started = Instant::now();
        for id in &projects {
            panel.update(&mut cx, |p, cx| p.toggle(id, cx));
        }
        println!("expanded {} projects in {:?} ({} rows)", projects.len(), started.elapsed(), panel.read_with(&cx, |p, _| p.lines.len()));
        let started = Instant::now();
        panel.update(&mut cx, |p, cx| p.reindex(cx));
        println!("reindex {:?}", started.elapsed());
        let started = Instant::now();
        for _ in 0..10 {
            draw(&panel, &mut cx);
        }
        println!("10 frames {:?}", started.elapsed());
    }

    #[gpui::test]
    async fn shows_and_edits_a_solution(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write(&root, "All.sln", include_str!("../../../dotnet-model/tests/fixtures/sample.sln"));
        write(&root, "README.md", "");
        write(&root, "src/App/App.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup>\n    <TargetFramework>net8.0</TargetFramework>\n  </PropertyGroup>\n</Project>\n");
        write(&root, "src/App/Program.cs", "");
        write(&root, "src/App/Models/User.cs", "");
        write(&root, "tests/Tests/Tests.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\" />\n");

        let (panel, mut cx) = panel_for(&root, cx).await;
        wait_for(&mut cx, &panel, "the solution", |p, _| !p.lines.is_empty()).await;
        assert_eq!(panel.read_with(&cx, |p, _| labels(p)), vec!["All", "  src", "    App", "    README.md", "  Tests"]);
        draw(&panel, &mut cx);

        // Expand the project: dependencies first, then folders, then files.
        let app_id = format!("project:{}", root.join("src/App/App.csproj").display());
        panel.update(&mut cx, |p, cx| p.toggle(&app_id, cx));
        assert_eq!(
            panel.read_with(&cx, |p, _| labels(p)),
            vec!["All", "  src", "    App", "      Dependencies", "      Models", "      Program.cs", "    README.md", "  Tests"]
        );
        draw(&panel, &mut cx);

        // New folder and a class from a template in it, both through the inline editor's result.
        let app = root.join("src/App/App.csproj");
        panel.update_in(&mut cx, |p, window, cx| {
            p.apply_edit(EditKind::NewFolder { dir: root.join("src/App"), project: app.clone() }, "Services".into(), window, cx)
        });
        wait_for(&mut cx, &panel, "the new folder", |p, _| labels(p).iter().any(|l| l.trim() == "Services")).await;
        let class = dotnet_model::templates::built_in().into_iter().find(|t| t.name == "Class" && t.extension == "cs").unwrap();
        panel.update_in(&mut cx, |p, window, cx| {
            p.apply_edit(EditKind::NewFile { dir: root.join("src/App/Services"), project: app.clone(), template: Some(class), anchor: None }, "Greeter".into(), window, cx)
        });
        wait_for(&mut cx, &panel, "the new class", |_, _| root.join("src/App/Services/Greeter.cs").exists()).await;
        assert_eq!(std::fs::read_to_string(root.join("src/App/Services/Greeter.cs")).unwrap(), "namespace App.Services;\n\npublic class Greeter\n{\n}\n");

        panel.update_in(&mut cx, |p, window, cx| {
            p.apply_edit(EditKind::Rename { path: root.join("src/App/Services/Greeter.cs"), project: Some(app.clone()) }, "Hello.cs".into(), window, cx)
        });
        wait_for(&mut cx, &panel, "the rename", |_, _| root.join("src/App/Services/Hello.cs").exists()).await;

        // A solution folder, then the test project dragged into it.
        panel.update_in(&mut cx, |p, window, cx| p.apply_edit(EditKind::NewSolutionFolder { parent: None }, "tests".into(), window, cx));
        wait_for(&mut cx, &panel, "the solution folder", |p, _| labels(p).iter().any(|l| l == "  tests")).await;
        let solution = Solution::load(&root.join("All.sln")).unwrap();
        let folder = solution.folders.iter().find(|f| f.name == "tests").unwrap().id.clone();
        let tests = root.join("tests/Tests/Tests.csproj");
        panel.update_in(&mut cx, |p, window, cx| {
            let dragged = DraggedNode { nodes: vec![(format!("project:{}", tests.display()), NodeKind::Project { path: tests.clone() })], label: "Tests".into() };
            p.drop_on(dragged, &format!("folder:{folder}"), window, cx)
        });
        wait_for(&mut cx, &panel, "the move", |p, _| labels(p).iter().any(|l| l == "    Tests")).await;
        let solution = Solution::load(&root.join("All.sln")).unwrap();
        assert!(solution.projects.is_empty(), "the project left the root");
        assert_eq!(solution.folders.iter().find(|f| f.name == "tests").unwrap().projects[0].path, tests);
        draw(&panel, &mut cx);

        // The NuGet manager opens for the project and renders without a network.
        let workspace = panel.read_with(&cx, |p, _| p.workspace.upgrade().unwrap());
        let model = panel.read_with(&cx, |p, _| p.model.clone());
        workspace.update_in(&mut cx, |ws, window, cx| crate::nuget_view::open(ws, model, Some(app.clone()), window, cx));
        cx.run_until_parked();
        let manager = workspace.read_with(&cx, |ws, cx| ws.items_of_type::<crate::nuget_view::NuGetManager>(cx).count());
        assert_eq!(manager, 1);
        cx.draw(point(px(0.), px(0.)), size(px(1000.), px(700.)), move |_, _| div().size_full().child(workspace.clone()));
    }
}
