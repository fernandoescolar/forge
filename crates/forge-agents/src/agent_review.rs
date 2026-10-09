//! The agent's pending changes, wherever the file is open.
//!
//! A file a thread changed (and the user hasn't kept or undone yet) shows, in every
//! editor, what changed since before the agent: added lines highlighted, removed ones
//! inline, and on each change *Keep* and *Undo*. Keeping a change makes it part of the
//! file's baseline; undoing puts the old lines back and saves. When nothing is left the
//! editor goes back to showing git's changes.
//!
//! It works through Zed's own hooks: an editor with a custom hunk renderer stops loading
//! git's diff, so ours takes its place, and gets it back when the renderer is removed.

use std::{collections::HashMap, ops::Range, path::PathBuf, sync::Arc};

use buffer_diff::BufferDiff;
use editor::Editor;
use gpui::{
    InteractiveElement as _, AnyElement, App, AppContext as _, AsyncApp, Context, Entity, Global, IntoElement, ParentElement as _, Pixels, Styled as _, Subscription, Task,
    WeakEntity, Window,
};
use language::{Buffer, BufferEvent};
use multi_buffer::Anchor;
use theme::ActiveTheme as _;
use ui::{Button, ButtonCommon as _, ButtonSize, ButtonStyle, Clickable as _, Color, IconButton, IconName, IconSize, LabelSize, Tooltip, h_flex};

use crate::thread::Thread;

gpui::actions!(forge_agent, [
    /// Keeps the agent's change at the cursor.
    KeepAgentChange,
    /// Undoes the agent's change at the cursor.
    UndoAgentChange,
]);

/// cmd-alt-y / cmd-alt-z keep or undo the agent's change at the cursor (the keys Zed uses
/// for staging and restoring git hunks). Bound after the default keymap so they win.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("secondary-alt-y", KeepAgentChange, Some("Editor")),
        gpui::KeyBinding::new("secondary-alt-z", UndoAgentChange, Some("Editor")),
    ]);
}

pub fn init(cx: &mut App) {
    bind_keys(cx);
    cx.observe_new(|editor: &mut Editor, window, cx| {
        if window.is_none() || editor.buffer().read(cx).as_singleton().is_none() {
            return;
        }
        let weak = cx.weak_entity();
        PendingChanges::global(cx).update(cx, |p, _| p.editors.push(weak));
        cx.defer_in(window.unwrap(), |editor, _, cx| sync_editor(editor, cx));
    })
    .detach();
    cx.observe_new(|workspace: &mut workspace::Workspace, _, _| {
        workspace
            .register_action(|ws, _: &KeepAgentChange, window, cx| at_cursor(ws, true, window, cx))
            .register_action(|ws, _: &UndoAgentChange, window, cx| at_cursor(ws, false, window, cx));
    })
    .detach();
}

/// Which thread has pending changes to which file, and the editors to show them in.
#[derive(Default)]
pub struct PendingChanges {
    files: HashMap<PathBuf, WeakEntity<Thread>>,
    editors: Vec<WeakEntity<Editor>>,
}

struct GlobalPending(Entity<PendingChanges>);
impl Global for GlobalPending {}

impl PendingChanges {
    pub fn global(cx: &mut App) -> Entity<PendingChanges> {
        if let Some(g) = cx.try_global::<GlobalPending>() {
            return g.0.clone();
        }
        let entity = cx.new(|_| PendingChanges::default());
        cx.set_global(GlobalPending(entity.clone()));
        entity
    }

    /// The content before the agent, if a thread has pending changes to `path`.
    fn baseline(&self, path: &PathBuf, cx: &App) -> Option<(Entity<Thread>, String)> {
        let thread = self.files.get(path)?.upgrade()?;
        let original = thread.read(cx).changes.iter().find(|c| &c.path == path)?.original.clone().unwrap_or_default();
        Some((thread, original))
    }
}

/// Another live thread with unreviewed changes to `path`, if any.
pub(crate) fn other_thread_changing(path: &PathBuf, me: &Entity<Thread>, cx: &mut App) -> Option<Entity<Thread>> {
    let owner = PendingChanges::global(cx).read(cx).files.get(path)?.upgrade()?;
    (owner != *me && owner.read(cx).changes.iter().any(|c| &c.path == path)).then_some(owner)
}

/// A thread's changed files changed: editors of those files follow.
pub(crate) fn thread_changed(thread: &Entity<Thread>, cx: &mut App) {
    let paths: Vec<PathBuf> = thread.read(cx).changes.iter().map(|c| c.path.clone()).collect();
    let weak = thread.downgrade();
    let pending = PendingChanges::global(cx);
    let editors = pending.update(cx, |p, _| {
        p.files.retain(|path, t| !(t == &weak && !paths.contains(path)) && t.upgrade().is_some());
        for path in paths {
            p.files.insert(path, weak.clone());
        }
        p.editors.retain(|e| e.upgrade().is_some());
        p.editors.clone()
    });
    for editor in editors.into_iter().filter_map(|e| e.upgrade()) {
        editor.update(cx, |editor, cx| sync_editor(editor, cx));
    }
}

/// The diff an editor shows for one of its buffers, against the content before the agent.
struct BufferOverlay {
    base: Arc<str>,
    diff: Option<Entity<BufferDiff>>,
    _task: Task<()>,
}

/// Kept in an editor that shows the agent's changes.
pub(crate) struct Overlay {
    buffers: HashMap<text::BufferId, BufferOverlay>,
    /// The review tab: shows the agent's renderer even with nothing left.
    sticky: bool,
}

impl editor::Addon for Overlay {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn to_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

fn buffer_path(buffer: &Entity<Buffer>, cx: &App) -> Option<PathBuf> {
    Some(buffer.read(cx).file()?.as_local()?.abs_path(cx))
}

/// Makes the editor show the agent's pending changes for its buffers (or stop showing them).
fn sync_editor(editor: &mut Editor, cx: &mut Context<Editor>) {
    let buffers = editor.buffer().read(cx).all_buffers();
    let pending = PendingChanges::global(cx);
    let mut wanted: HashMap<text::BufferId, (Entity<Buffer>, String)> = HashMap::new();
    for buffer in buffers {
        let Some(path) = buffer_path(&buffer, cx) else { continue };
        if let Some((_, original)) = pending.read(cx).baseline(&path, cx) {
            wanted.insert(buffer.read(cx).remote_id(), (buffer, original));
        }
    }
    let sticky = editor.addon::<Overlay>().is_some_and(|o| o.sticky);
    if wanted.is_empty() && !sticky {
        if editor.addon::<Overlay>().is_some() {
            editor.unregister_addon::<Overlay>();
            // Back to git's diff.
            editor.set_diff_hunk_renderer(None, cx);
        }
        return;
    }
    if editor.addon::<Overlay>().is_none() {
        editor.register_addon(Overlay { buffers: HashMap::new(), sticky: false });
        editor.set_diff_hunk_renderer(Some(Arc::new(AgentHunkRenderer)), cx);
    }
    let languages = editor.project().map(|p| p.read(cx).languages().clone());
    let multibuffer = editor.buffer().clone();
    let Some(overlay) = editor.addon_mut::<Overlay>() else { return };
    overlay.buffers.retain(|id, _| wanted.contains_key(id));
    for (id, (buffer, original)) in wanted {
        if overlay.buffers.get(&id).is_some_and(|o| *o.base == *original) {
            continue;
        }
        let base: Arc<str> = original.into();
        let task = show_diff(multibuffer.clone(), buffer, base.clone(), languages.clone(), cx);
        overlay.buffers.insert(id, BufferOverlay { base, diff: None, _task: task });
    }
}

/// Builds the diff against `base`, shows it expanded and keeps it current while the
/// buffer changes. Dropping the task stops following the buffer.
fn show_diff(multibuffer: Entity<multi_buffer::MultiBuffer>, buffer: Entity<Buffer>, base: Arc<str>, languages: Option<Arc<language::LanguageRegistry>>, cx: &mut Context<Editor>) -> Task<()> {
    cx.spawn(async move |editor, cx: &mut AsyncApp| {
        let diff = crate::diff::build_buffer_diff(base.clone(), &buffer, languages, cx).await;
        let id = cx.update(|cx| buffer.read(cx).remote_id());
        let shown = editor.update(cx, |editor, cx| {
            if let Some(o) = editor.addon_mut::<Overlay>().and_then(|o| o.buffers.get_mut(&id)) {
                o.diff = Some(diff.clone());
            }
            multibuffer.update(cx, |mb, cx| {
                mb.add_diff(diff.clone(), cx);
                mb.expand_diff_hunks(vec![Anchor::Min..Anchor::Max], cx);
            });
        });
        if shown.is_err() {
            return;
        }
        let _edits: Subscription = cx.update(|cx| {
            let (diff, base, multibuffer) = (diff.clone(), base.clone(), multibuffer.clone());
            cx.subscribe(&buffer, move |buffer, event: &BufferEvent, cx| {
                if matches!(event, BufferEvent::Edited { .. } | BufferEvent::Reloaded) {
                    let snapshot = buffer.read(cx).text_snapshot();
                    let (diff, base, multibuffer) = (diff.clone(), base.clone(), multibuffer.clone());
                    cx.spawn(async move |cx: &mut AsyncApp| {
                        let base_snapshot = diff.read_with(cx, |d, cx| d.base_text(cx));
                        let update = diff.update(cx, |d, cx| d.update_diff(snapshot, &base_snapshot, Some(base), cx)).await;
                        diff.update(cx, |d, cx| d.set_snapshot(update, cx));
                        // New hunks show expanded too.
                        multibuffer.update(cx, |mb, cx| mb.expand_diff_hunks(vec![Anchor::Min..Anchor::Max], cx));
                    })
                    .detach();
                }
            })
        });
        futures::future::pending::<()>().await;
    })
}

/// The review tab: every buffer of `editor` against its content before the agent, with
/// Keep / Undo on each change, following the threads' changes.
pub(crate) fn make_review_editor(editor: &mut Editor, cx: &mut Context<Editor>) {
    editor.register_addon(Overlay { buffers: HashMap::new(), sticky: true });
    editor.set_diff_hunk_renderer(Some(Arc::new(AgentHunkRenderer)), cx);
    let weak = cx.weak_entity();
    PendingChanges::global(cx).update(cx, |p, _| p.editors.push(weak));
    sync_editor(editor, cx);
}

/// The hunk at `range` in `editor`: its buffer, the text range in the buffer, the base
/// text range, the base text, and the file.
struct HunkTarget {
    buffer: Entity<Buffer>,
    buffer_range: Range<text::Anchor>,
    base_range: Range<usize>,
    base: String,
    path: PathBuf,
}

fn hunk_target(editor: &Editor, range: &Range<Anchor>, cx: &App) -> Option<HunkTarget> {
    let multibuffer = editor.buffer().read(cx);
    let buffer_id = range.start.buffer_id().or_else(|| range.end.buffer_id()).or_else(|| multibuffer.as_singleton().map(|b| b.read(cx).remote_id()))?;
    let buffer = multibuffer.buffer(buffer_id)?;
    let diff = editor.addon::<Overlay>()?.buffers.get(&buffer_id)?.diff.clone()?;
    let snapshot = buffer.read(cx).text_snapshot();
    let text_range = range.start.text_anchor_in(&buffer.read(cx).snapshot())..range.end.text_anchor_in(&buffer.read(cx).snapshot());
    let diff_snapshot = diff.read(cx).snapshot(cx);
    let hunk = diff_snapshot.hunks_intersecting_range(text_range, &snapshot).next()?;
    let base = diff_snapshot.base_text_string()?;
    Some(HunkTarget { path: buffer_path(&buffer, cx)?, buffer, buffer_range: hunk.buffer_range, base_range: hunk.diff_base_byte_range, base })
}

/// Keep: the change becomes part of the baseline. Undo: the old lines come back (saved).
fn resolve_hunk(editor: Entity<Editor>, range: Range<Anchor>, keep: bool, cx: &mut App) {
    let Some(target) = hunk_target(editor.read(cx), &range, cx) else { return };
    let pending = PendingChanges::global(cx);
    let Some((thread, _)) = pending.read(cx).baseline(&target.path, cx) else { return };
    let HunkTarget { buffer, buffer_range, base_range, base, path } = target;
    if keep {
        let new_lines: String = buffer.read(cx).text_for_range(buffer_range).collect();
        let mut baseline = base.clone();
        baseline.replace_range(base_range, &new_lines);
        let current = buffer.read(cx).text();
        thread.update(cx, |t, cx| t.set_change_baseline(&path, baseline, current, cx));
    } else {
        let old_lines = base[base_range].to_string();
        buffer.update(cx, |b, cx| {
            b.edit([(buffer_range, old_lines)], None, cx);
        });
        let current = buffer.read(cx).text();
        thread.update(cx, |t, cx| t.set_change_baseline(&path, base, current, cx));
        if let Some(project) = editor.read(cx).project().cloned() {
            project.update(cx, |p, cx| p.save_buffer(buffer, cx)).detach_and_log_err(cx);
        }
    }
}

/// Keep / Undo the agent's change under the cursor of the active editor.
fn at_cursor(workspace: &mut workspace::Workspace, keep: bool, window: &mut Window, cx: &mut Context<workspace::Workspace>) {
    let Some(editor) = workspace.active_item(cx).and_then(|i| i.act_as::<Editor>(cx)) else { return };
    let range = {
        let editor = editor.read(cx);
        if editor.addon::<Overlay>().is_none() {
            // No agent changes here: the keys do what they do in Zed (stage / restore the git hunk).
            let fallback = if keep { "git::ToggleStaged" } else { "git::Restore" };
            if let Ok(action) = cx.build_action(fallback, None) {
                window.dispatch_action(action, cx);
            }
            return;
        }
        let head = editor.selections.newest_anchor().head();
        head..head
    };
    resolve_hunk(editor, range, keep, cx);
}

use gpui::TaskExt as _;

struct AgentHunkRenderer;

impl editor::DiffHunkRenderer for AgentHunkRenderer {
    /// Never staged (it isn't git's diff): filled red and green, not hollow.
    fn render_hunk_as_staged(&self, _status: &buffer_diff::DiffHunkStatus, _cx: &App) -> bool {
        false
    }

    fn render_hunk_controls(
        &self,
        row: u32,
        _status: &buffer_diff::DiffHunkStatus,
        hunk_range: Range<Anchor>,
        _is_created_file: bool,
        line_height: Pixels,
        editor: &Entity<Editor>,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let keep = {
            let (editor, range) = (editor.clone(), hunk_range.clone());
            Button::new(("agent-keep", row as u64), "Keep")
                .style(ButtonStyle::Tinted(ui::TintColor::Success))
                .size(ButtonSize::Compact)
                .label_size(LabelSize::Small)
                .tooltip(Tooltip::for_action_title("Keep this change", &KeepAgentChange))
                .on_click(move |_, _, cx| resolve_hunk(editor.clone(), range.clone(), true, cx))
        };
        let undo = {
            let (editor, range) = (editor.clone(), hunk_range.clone());
            Button::new(("agent-undo", row as u64), "Undo")
                .style(ButtonStyle::Subtle)
                .size(ButtonSize::Compact)
                .label_size(LabelSize::Small)
                .tooltip(Tooltip::for_action_title("Put back what was there before the agent", &UndoAgentChange))
                .on_click(move |_, _, cx| resolve_hunk(editor.clone(), range.clone(), false, cx))
        };
        let next = {
            let editor = editor.clone();
            IconButton::new(("agent-next", row as u64), IconName::ArrowDown)
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::for_action_title("Next change", &editor::actions::GoToHunk))
                .on_click(move |_, window, cx| {
                    window.focus(&gpui::Focusable::focus_handle(editor.read(cx), cx), cx);
                    window.dispatch_action(Box::new(editor::actions::GoToHunk), cx);
                })
        };
        let prev = {
            let editor = editor.clone();
            IconButton::new(("agent-prev", row as u64), IconName::ArrowUp)
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::for_action_title("Previous change", &editor::actions::GoToPreviousHunk))
                .on_click(move |_, window, cx| {
                    window.focus(&gpui::Focusable::focus_handle(editor.read(cx), cx), cx);
                    window.dispatch_action(Box::new(editor::actions::GoToPreviousHunk), cx);
                })
        };
        h_flex()
            .h(line_height)
            .mr_1()
            .gap_1()
            .px_0p5()
            .pb_1()
            .border_x_1()
            .border_b_1()
            .border_color(colors.border_variant)
            .rounded_b_lg()
            .bg(colors.editor_background)
            .gap_1()
            .block_mouse_except_scroll()
            .shadow_md()
            .child(prev)
            .child(next)
            .child(undo)
            .child(keep)
            .into_any_element()
    }
}
