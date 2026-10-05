//! The merge editor: a file with conflict markers in three panes. Yours and theirs on top
//! (the file with every conflict resolved to that side, its hunks highlighted), and the
//! result below: the file itself, where Zed's inline conflict buttons still work. Accept a
//! side for the conflict at the cursor, move between conflicts, and mark the file resolved.
//!
//! It reads the markers, not git's index, so it also works for files merged without git's
//! help (a thread's worktree applied over uncommitted changes).

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use editor::{Editor, EditorEvent, RowHighlightOptions};
use forge_ui::OpenMergeEditor;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Task,
    TaskExt as _, Window, actions,
};
use language::{Buffer, BufferEvent, Point};
use multi_buffer::MultiBuffer;
use project::Project;
use theme::ActiveTheme as _;
use ui::{Button, ButtonCommon as _, ButtonSize, ButtonStyle, Clickable as _, Color, Disableable as _, Icon, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip, h_flex, v_flex};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

use crate::conflicts::{self, Choice, Conflict, Side};

actions!(forge_git, [
    /// Moves to the next conflict in the merge editor.
    NextConflict,
    /// Moves to the previous conflict in the merge editor.
    PreviousConflict
]);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, action: &OpenMergeEditor, window, cx| open(workspace, action.path.clone().map(PathBuf::from), window, cx));
    })
    .detach();
}

/// The files of the project with conflicts git knows about.
fn conflicted_files(workspace: &Workspace, cx: &App) -> Vec<PathBuf> {
    let project = workspace.project().read(cx);
    let mut files = Vec::new();
    for repo in project.git_store().read(cx).repositories().values() {
        let snapshot = repo.read(cx).snapshot();
        for (repo_path, _) in snapshot.merge.merge_heads_by_conflicted_path.iter() {
            if !snapshot.status_for_path(repo_path).is_some_and(|entry| entry.status.is_conflicted()) {
                continue;
            }
            if let Some(path) = repo.read(cx).repo_path_to_project_path(repo_path, cx).and_then(|p| project.absolute_path(&p, cx)) {
                files.push(path);
            }
        }
    }
    files
}

/// Opens the merge editor on `path`; without one, on the active file when it has conflict
/// markers, else on a conflicted file the user picks.
pub fn open(workspace: &mut Workspace, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Workspace>) {
    let path = path.or_else(|| {
        let editor = workspace.active_item(cx)?.downcast::<Editor>()?;
        let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
        let has_markers = !conflicts::parse(&buffer.read(cx).text()).is_empty();
        has_markers.then(|| buffer.read(cx).file()?.as_local().map(|f| f.abs_path(cx))).flatten()
    });
    if let Some(path) = path {
        return open_path(workspace, path, window, cx);
    }
    let files = conflicted_files(workspace, cx);
    match files.len() {
        0 => {
            let id = workspace::notifications::NotificationId::named("forge-merge".into());
            workspace.show_toast(workspace::Toast::new(id, "No file has merge conflicts."), cx);
        }
        1 => open_path(workspace, files[0].clone(), window, cx),
        _ => {
            let choices = files.iter().map(|f| forge_ui::pick::Choice::new(f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).detail(f.to_string_lossy().into_owned())).collect();
            let weak = cx.entity().downgrade();
            forge_ui::pick::pick(workspace, "Merge which file?", choices, window, cx, move |ix, window, cx| {
                let Some(file) = files.get(ix).cloned() else { return };
                forge_ui::pick::defer_workspace(weak, window, cx, move |ws, window, cx| open_path(ws, file, window, cx));
            });
        }
    }
}

fn open_path(workspace: &mut Workspace, path: PathBuf, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace.items_of_type::<MergeEditor>(cx).find(|m| m.read(cx).path == path);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let project = workspace.project().clone();
    let open = project.update(cx, |p, cx| p.open_local_buffer(&path, cx));
    cx.spawn_in(window, async move |workspace, cx| {
        let buffer = open.await?;
        workspace.update_in(cx, |workspace, window, cx| {
            let view = cx.new(|cx| MergeEditor::new(path, buffer, project, window, cx));
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        })
    })
    .detach_and_log_err(cx);
}

struct OursRows;
struct TheirsRows;
struct CurrentRows;

pub enum MergeEvent {
    Close,
    Changed,
}

pub struct MergeEditor {
    pub path: PathBuf,
    project: Entity<Project>,
    buffer: Entity<Buffer>,
    result: Entity<Editor>,
    ours: Entity<Editor>,
    theirs: Entity<Editor>,
    pub conflicts: Vec<Conflict>,
    ours_regions: Vec<Range<u32>>,
    theirs_regions: Vec<Range<u32>>,
    /// The conflict at (or after) the result's cursor.
    pub current: Option<usize>,
    refresh: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<MergeEvent> for MergeEditor {}

impl MergeEditor {
    pub fn new(path: PathBuf, buffer: Entity<Buffer>, project: Entity<Project>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let result = cx.new(|cx| Editor::for_buffer(buffer.clone(), Some(project.clone()), window, cx));
        let language = buffer.read(cx).language().cloned();
        let side = |title: &str, window: &mut Window, cx: &mut Context<Self>| {
            let side_buffer = project.update(cx, |p, cx| p.create_local_buffer("", language.clone(), false, cx));
            let multibuffer = cx.new(|cx| MultiBuffer::singleton(side_buffer, cx).with_title(title.to_string()));
            cx.new(|cx| {
                let mut editor = Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx);
                editor.set_read_only(true);
                editor
            })
        };
        let (ours, theirs) = (side("Yours", window, cx), side("Theirs", window, cx));
        let subscriptions = vec![
            cx.subscribe(&buffer, |this, _, event: &BufferEvent, cx| {
                if matches!(event, BufferEvent::Edited { .. } | BufferEvent::Reloaded) {
                    this.schedule_refresh(cx);
                }
            }),
            cx.subscribe_in(&result, window, |this, _, event: &EditorEvent, window, cx| {
                if matches!(event, EditorEvent::SelectionsChanged { local: true }) {
                    this.follow_cursor(window, cx);
                }
            }),
        ];
        let mut this = Self {
            path,
            project,
            buffer,
            result,
            ours,
            theirs,
            conflicts: vec![],
            ours_regions: vec![],
            theirs_regions: vec![],
            current: None,
            refresh: None,
            _subscriptions: subscriptions,
        };
        this.refresh(cx);
        this.go_to(0, window, cx);
        this
    }

    pub fn result(&self) -> &Entity<Editor> {
        &self.result
    }

    pub fn side_text(&self, side: Side, cx: &App) -> String {
        let editor = if side == Side::Ours { &self.ours } else { &self.theirs };
        editor.read(cx).buffer().read(cx).as_singleton().map(|b| b.read(cx).text()).unwrap_or_default()
    }

    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(150)).await;
            this.update(cx, |this, cx| this.refresh(cx)).ok();
        }));
    }

    /// Reads the conflicts from the result again and rebuilds both sides.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let text = self.buffer.read(cx).text();
        self.conflicts = conflicts::parse(&text);
        let (ours_text, ours_regions) = conflicts::side_text(&text, Side::Ours);
        let (theirs_text, theirs_regions) = conflicts::side_text(&text, Side::Theirs);
        set_side(&self.ours, ours_text, &ours_regions, cx);
        set_side(&self.theirs, theirs_text, &theirs_regions, cx);
        self.ours_regions = ours_regions;
        self.theirs_regions = theirs_regions;
        if self.current.is_some_and(|c| c >= self.conflicts.len()) || self.current.is_none() {
            self.current = (!self.conflicts.is_empty()).then_some(0);
        }
        cx.emit(MergeEvent::Changed);
        cx.notify();
    }

    fn cursor_row(&self, cx: &mut Context<Self>) -> u32 {
        self.result.update(cx, |e, cx| e.selections.newest::<Point>(&e.display_snapshot(cx)).head().row)
    }

    fn follow_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = self.cursor_row(cx);
        let current = conflicts::at_or_after(&self.conflicts, row);
        if current != self.current {
            self.current = current;
            if let Some(ix) = current {
                self.reveal_sides(ix, window, cx);
            }
            cx.notify();
        }
    }

    /// Scrolls both sides to conflict `ix` and marks it.
    fn reveal_sides(&self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        for (editor, regions) in [(&self.ours, &self.ours_regions), (&self.theirs, &self.theirs_regions)] {
            let Some(region) = regions.get(ix) else { continue };
            editor.update(cx, |editor, cx| {
                let point = Point::new(region.start, 0);
                editor.change_selections(editor::SelectionEffects::scroll(editor::scroll::Autoscroll::center()), window, cx, |s| s.select_ranges([point..point]));
                editor.clear_row_highlights::<CurrentRows>();
                if region.end > region.start {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let range = snapshot.anchor_before(Point::new(region.start, 0))..snapshot.anchor_before(Point::new(region.end - 1, 0));
                    editor.highlight_rows::<CurrentRows>(range, |cx| cx.theme().colors().editor_active_line_background, RowHighlightOptions { autoscroll: false, include_gutter: true }, cx);
                }
            });
        }
    }

    /// Moves the result's cursor to conflict `ix` and the sides along.
    pub fn go_to(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conflict) = self.conflicts.get(ix) else { return };
        let point = Point::new(conflict.rows.start, 0);
        self.result.update(cx, |editor, cx| {
            editor.change_selections(editor::SelectionEffects::scroll(editor::scroll::Autoscroll::center()), window, cx, |s| s.select_ranges([point..point]));
        });
        self.current = Some(ix);
        self.reveal_sides(ix, window, cx);
        cx.notify();
    }

    fn next(&mut self, _: &NextConflict, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.current.map(|c| (c + 1).min(self.conflicts.len().saturating_sub(1))).unwrap_or(0);
        self.go_to(ix, window, cx);
    }

    fn previous(&mut self, _: &PreviousConflict, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.current.map(|c| c.saturating_sub(1)).unwrap_or(0);
        self.go_to(ix, window, cx);
    }

    /// Resolves the current conflict with `choice`, then moves to the next one.
    pub fn accept(&mut self, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.current else { return };
        let Some(conflict) = self.conflicts.get(ix).cloned() else { return };
        let mut text: String = conflict.lines(choice).iter().map(|l| format!("{l}\n")).collect();
        self.buffer.update(cx, |buffer, cx| {
            let max = buffer.max_point();
            let end = if conflict.rows.end > max.row {
                // The conflict ends the file: keep its last line ending as it was.
                if !buffer.text().ends_with('\n') {
                    text.pop();
                }
                max
            } else {
                Point::new(conflict.rows.end, 0)
            };
            buffer.edit([(Point::new(conflict.rows.start, 0)..end, text)], None, cx);
        });
        self.refresh(cx);
        if !self.conflicts.is_empty() {
            self.go_to(ix.min(self.conflicts.len() - 1), window, cx);
        }
    }

    /// Saves the result and, when git has the file as conflicted, stages it; then closes.
    pub fn mark_resolved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.conflicts.is_empty() {
            return;
        }
        let save = self.project.update(cx, |p, cx| p.save_buffer(self.buffer.clone(), cx));
        let path = self.path.clone();
        cx.spawn_in(window, async move |this, cx| {
            save.await?;
            cx.background_spawn(async move { stage_if_unmerged(&path) }).await?;
            this.update(cx, |_, cx| cx.emit(MergeEvent::Close))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }
}

/// `git add` for a file git has as unmerged; files with markers git doesn't know about
/// (merged by Forge) stay as they are.
fn stage_if_unmerged(path: &Path) -> anyhow::Result<()> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { return Ok(()) };
    let unmerged = std::process::Command::new("git").arg("-C").arg(dir).args(["ls-files", "-u", "--"]).arg(name).output();
    if unmerged.is_ok_and(|o| o.status.success() && !o.stdout.is_empty()) {
        let added = std::process::Command::new("git").arg("-C").arg(dir).args(["add", "--"]).arg(name).output()?;
        anyhow::ensure!(added.status.success(), "git add failed: {}", String::from_utf8_lossy(&added.stderr).trim());
    }
    Ok(())
}

fn set_side(editor: &Entity<Editor>, text: String, regions: &[Range<u32>], cx: &mut App) {
    let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else { return };
    if buffer.read(cx).text() != text {
        buffer.update(cx, |buffer, cx| buffer.set_text(text, cx));
    }
    let ours = editor.read(cx).buffer().read(cx).title(cx).starts_with("Yours");
    editor.update(cx, |editor, cx| {
        editor.clear_row_highlights::<OursRows>();
        editor.clear_row_highlights::<TheirsRows>();
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        for region in regions.iter().filter(|r| r.end > r.start) {
            let range = snapshot.anchor_before(Point::new(region.start, 0))..snapshot.anchor_before(Point::new(region.end - 1, 0));
            let options = RowHighlightOptions { autoscroll: false, include_gutter: true };
            if ours {
                editor.highlight_rows::<OursRows>(range, |cx| cx.theme().status().created_background, options, cx);
            } else {
                editor.highlight_rows::<TheirsRows>(range, |cx| cx.theme().status().info_background, options, cx);
            }
        }
    });
}

impl Render for MergeEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let left = self.conflicts.len();
        let first = self.conflicts.first();
        let ours_label = first.map(|c| c.ours_label.clone()).filter(|l| !l.is_empty()).unwrap_or_else(|| "yours".into());
        let theirs_label = first.map(|c| c.theirs_label.clone()).filter(|l| !l.is_empty()).unwrap_or_else(|| "theirs".into());
        let status: SharedString = match (left, self.current) {
            (0, _) => "All conflicts resolved".into(),
            (n, Some(c)) => format!("Conflict {} of {n}", c + 1).into(),
            (n, None) => format!("{n} conflicts").into(),
        };
        let has_current = self.current.is_some() && left > 0;
        let accept = |id: &'static str, label: &'static str, choice: Choice, cx: &mut Context<Self>| {
            Button::new(id, label).size(ButtonSize::Compact).disabled(!has_current).on_click(cx.listener(move |this, _, window, cx| this.accept(choice, window, cx)))
        };
        let toolbar = h_flex()
            .justify_between()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .gap_1()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::Small).color(Color::Muted))
                    .child(Label::new(status).size(LabelSize::Small).color(if left == 0 { Color::Success } else { Color::Default }))
                    .child(IconButton::new("merge-previous", IconName::ChevronUp).icon_size(IconSize::Small).disabled(left == 0).tooltip(Tooltip::text("Previous conflict")).on_click(cx.listener(|this, _, window, cx| this.previous(&PreviousConflict, window, cx))))
                    .child(IconButton::new("merge-next", IconName::ChevronDown).icon_size(IconSize::Small).disabled(left == 0).tooltip(Tooltip::text("Next conflict")).on_click(cx.listener(|this, _, window, cx| this.next(&NextConflict, window, cx)))),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(accept("merge-ours", "Accept Yours", Choice::Ours, cx))
                    .child(accept("merge-theirs", "Accept Theirs", Choice::Theirs, cx))
                    .child(accept("merge-both", "Accept Both", Choice::Both, cx))
                    .child(
                        Button::new("merge-done", "Mark as Resolved")
                            .style(ButtonStyle::Filled)
                            .size(ButtonSize::Compact)
                            .disabled(left > 0)
                            .on_click(cx.listener(|this, _, window, cx| this.mark_resolved(window, cx))),
                    ),
            );
        let pane = |title: String, color: gpui::Hsla, editor: &Entity<Editor>| {
            v_flex()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .child(h_flex().px_2().py_0p5().gap_1().border_b_1().border_color(colors.border).child(div_dot(color)).child(Label::new(title).size(LabelSize::Small).weight(FontWeight::SEMIBOLD)))
                .child(gpui::div().flex_1().min_h_0().child(editor.clone()))
        };
        let status_colors = cx.theme().status().clone();
        v_flex()
            .key_context("ForgeMergeEditor")
            .on_action(cx.listener(Self::next))
            .on_action(cx.listener(Self::previous))
            .size_full()
            .bg(colors.editor_background)
            .child(toolbar)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(pane(format!("Yours · {ours_label}"), status_colors.created, &self.ours))
                    .child(gpui::div().w_px().h_full().bg(colors.border))
                    .child(pane(format!("Theirs · {theirs_label}"), status_colors.info, &self.theirs)),
            )
            .child(gpui::div().h_px().w_full().bg(colors.border))
            .child(pane("Result".into(), colors.text_accent, &self.result))
            .when(left == 0, |el| el.child(h_flex().px_2().py_1().child(Label::new("No conflicts left: save the result and mark it resolved.").size(LabelSize::Small).color(Color::Muted))))
    }
}

fn div_dot(color: gpui::Hsla) -> gpui::Div {
    gpui::div().size_2().rounded_full().bg(color)
}

impl Focusable for MergeEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.result.focus_handle(cx)
    }
}

impl Item for MergeEditor {
    type Event = MergeEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Merge: {}", self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.path.to_string_lossy().into_owned().into())
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            MergeEvent::Close => f(ItemEvent::CloseItem),
            MergeEvent::Changed => f(ItemEvent::UpdateTab),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    #[test]
    fn stages_only_what_git_has_as_unmerged() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git").arg("-C").arg(&root).args(args).output().unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "T"]);
        git(&["config", "user.email", "t@t"]);
        std::fs::write(root.join("a.txt"), "base\n").unwrap();
        std::fs::write(root.join("b.txt"), "b\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        git(&["checkout", "-qb", "other"]);
        std::fs::write(root.join("a.txt"), "theirs\n").unwrap();
        git(&["commit", "-qam", "theirs"]);
        git(&["checkout", "-q", "main"]);
        std::fs::write(root.join("a.txt"), "ours\n").unwrap();
        git(&["commit", "-qam", "ours"]);
        git(&["merge", "-q", "other"]);
        assert!(!git(&["ls-files", "-u"]).is_empty(), "a real conflict");

        std::fs::write(root.join("a.txt"), "ours\ntheirs\n").unwrap();
        stage_if_unmerged(&root.join("a.txt")).unwrap();
        assert!(git(&["ls-files", "-u"]).is_empty(), "resolved and staged");
        // A file merged outside git (markers, but no conflict in the index) isn't staged.
        std::fs::write(root.join("b.txt"), "merged by Forge\n").unwrap();
        stage_if_unmerged(&root.join("b.txt")).unwrap();
        assert_eq!(git(&["diff", "--name-only"]).trim(), "b.txt", "still an unstaged change");
    }

    #[gpui::test]
    async fn merges_in_three_panes(cx: &mut TestAppContext) {
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            init(cx);
        });
        let text = "fn main() {\n<<<<<<< HEAD\n    a();\n=======\n    b();\n>>>>>>> feature\n    keep();\n<<<<<<< HEAD\n    c();\n=======\n    d();\n>>>>>>> feature\n}\n";
        params.fs.as_fake().insert_tree("/root", serde_json::json!({ "main.rs": text })).await;
        let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, |ws, window, cx| open(ws, Some(PathBuf::from("/root/main.rs")), window, cx));
        cx.run_until_parked();
        let merge = workspace.read_with(cx, |ws, cx| ws.items_of_type::<MergeEditor>(cx).next()).expect("the merge editor opened");
        let read = |cx: &mut VisualTestContext| merge.read_with(cx, |m, cx| (m.conflicts.len(), m.current, m.side_text(Side::Ours, cx), m.side_text(Side::Theirs, cx)));
        let (left, current, ours, theirs) = read(cx);
        assert_eq!((left, current), (2, Some(0)));
        assert_eq!(ours, "fn main() {\n    a();\n    keep();\n    c();\n}\n");
        assert_eq!(theirs, "fn main() {\n    b();\n    keep();\n    d();\n}\n");

        merge.update_in(cx, |m, window, cx| m.accept(Choice::Theirs, window, cx));
        let (left, current, ours, _) = read(cx);
        assert_eq!((left, current), (1, Some(0)), "one left, and it is the current one");
        assert_eq!(ours, "fn main() {\n    b();\n    keep();\n    c();\n}\n", "the sides show the resolved conflict as resolved");

        merge.update_in(cx, |m, window, cx| m.accept(Choice::Both, window, cx));
        let result = merge.read_with(cx, |m, cx| m.result().read(cx).buffer().read(cx).as_singleton().unwrap().read(cx).text());
        assert_eq!(result, "fn main() {\n    b();\n    keep();\n    c();\n    d();\n}\n");
        assert_eq!(read(cx).0, 0);

        // Typing in the result counts too: markers typed back are a conflict again.
        merge.update(cx, |m, cx| {
            m.buffer.update(cx, |b, cx| b.edit([(Point::new(1, 0)..Point::new(1, 0), "<<<<<<< x\n=======\n>>>>>>> y\n")], None, cx));
        });
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        assert_eq!(read(cx).0, 1);

        // Mark as Resolved needs every conflict gone; then it saves and closes.
        merge.update_in(cx, |m, window, cx| m.accept(Choice::Ours, window, cx));
        merge.update_in(cx, |m, window, cx| m.mark_resolved(window, cx));
        cx.run_until_parked();
        assert!(workspace.read_with(cx, |ws, cx| ws.items_of_type::<MergeEditor>(cx).next()).is_none(), "closed");
        assert_eq!(params.fs.load(Path::new("/root/main.rs")).await.unwrap(), "fn main() {\n    b();\n    keep();\n    c();\n    d();\n}\n");
    }
}
