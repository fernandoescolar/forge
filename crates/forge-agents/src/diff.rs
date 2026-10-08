//! Read-only diff views for agent edits, built the way Zed's own agent panel does it: the
//! new text in a buffer, a `BufferDiff` against the old text, and a multibuffer showing the
//! changed hunks (with syntax highlighting) in an embedded editor.

use buffer_diff::BufferDiff;
use editor::{Editor, EditorMode, MinimapVisibility, SizingBehavior};
use gpui::{App, AppContext as _, AsyncApp, Entity, Subscription, Task, Window};
use language::{Anchor, Buffer, BufferEvent, Capability, LanguageRegistry, OffsetRangeExt as _};
use multi_buffer::{MultiBuffer, PathKey, excerpt_context_lines};
use std::{cell::RefCell, ops::Range, path::Path, rc::Rc, sync::Arc};
use util::ResultExt as _;

/// A proposed change to one file.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub path: String,
    /// `None` when the file is new.
    pub old_text: Option<String>,
    pub new_text: String,
}

impl Edit {
    /// Parses ACP diff content: `{"type": "diff", "path", "oldText", "newText"}`.
    pub fn from_acp(content: &serde_json::Value) -> Option<Self> {
        (content.get("type")?.as_str()? == "diff").then_some(())?;
        Some(Self {
            path: content.get("path")?.as_str()?.to_string(),
            old_text: content.get("oldText").and_then(|v| v.as_str()).map(str::to_string),
            new_text: content.get("newText")?.as_str()?.to_string(),
        })
    }

    /// All diffs in a tool call's `content` array.
    pub fn all_from_tool_call(tool_call: &serde_json::Value) -> Vec<Self> {
        tool_call.get("content").and_then(|c| c.as_array()).map(|xs| xs.iter().filter_map(Self::from_acp).collect()).unwrap_or_default()
    }

    /// (added, removed) line counts.
    pub fn stats(&self) -> (usize, usize) {
        line_stats(self.old_text.as_deref().unwrap_or_default(), &self.new_text)
    }
}

/// (added, removed) line counts from `old` to `new`.
pub fn line_stats(old: &str, new: &str) -> (usize, usize) {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    // Strip the common prefix/suffix; good enough for a summary badge.
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..].iter().rev().zip(new[prefix..].iter().rev()).take_while(|(a, b)| a == b).count();
    (new.len() - prefix - suffix, old.len() - prefix - suffix)
}

/// A contiguous change, in line indices of the old and new text.
#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    pub old: Range<u32>,
    pub new: Range<u32>,
}

impl Hunk {
    /// "lines 10–12" (1-based, in the new text) for the hunk list.
    pub fn label(&self) -> String {
        match (self.new.len(), self.old.len()) {
            (0, n) => format!("delete {n} line{} at {}", if n == 1 { "" } else { "s" }, self.new.start + 1),
            (1, _) => format!("line {}", self.new.start + 1),
            _ => format!("lines {}–{}", self.new.start + 1, self.new.end),
        }
    }
}

pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    use imara_diff::{Algorithm, diff, intern::InternedInput, sources::lines_with_terminator};
    let input = InternedInput::new(lines_with_terminator(old), lines_with_terminator(new));
    let mut out = Vec::new();
    diff(Algorithm::Histogram, &input, |old: Range<u32>, new: Range<u32>| out.push(Hunk { old, new }));
    out
}

/// `old` with only the `accepted` hunks applied.
pub fn apply_hunks(old: &str, new: &str, hunks: &[Hunk], accepted: &[bool]) -> String {
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();
    let mut out = String::with_capacity(new.len().max(old.len()));
    let mut cursor = 0usize;
    for (hunk, take) in hunks.iter().zip(accepted) {
        out.extend(old_lines[cursor..hunk.old.start as usize].iter().copied());
        if *take {
            out.extend(new_lines[hunk.new.start as usize..hunk.new.end as usize].iter().copied());
        } else {
            out.extend(old_lines[hunk.old.start as usize..hunk.old.end as usize].iter().copied());
        }
        cursor = hunk.old.end as usize;
    }
    out.extend(old_lines[cursor..].iter().copied());
    out
}

pub struct DiffView {
    pub edit: Edit,
    pub editor: Entity<Editor>,
    /// The proposed text; editable while a write waits for review.
    pub buffer: Entity<Buffer>,
    _build: Task<()>,
    /// Recomputes the diff as the user edits the proposal.
    _live: Rc<RefCell<Option<Subscription>>>,
}

impl DiffView {
    /// A read-only view of the change.
    pub fn new(edit: Edit, languages: Arc<LanguageRegistry>, window: &mut Window, cx: &mut App) -> Self {
        Self::build(edit, languages, false, false, window, cx)
    }

    /// The whole file with the change shown in it, scrollable and with line numbers: for
    /// a pane, where the change is seen in place.
    pub fn whole_file(edit: Edit, languages: Arc<LanguageRegistry>, window: &mut Window, cx: &mut App) -> Self {
        Self::build(edit, languages, false, true, window, cx)
    }

    /// A change the user can edit before accepting it; the diff follows their edits.
    pub fn editable(edit: Edit, languages: Arc<LanguageRegistry>, window: &mut Window, cx: &mut App) -> Self {
        Self::build(edit, languages, true, false, window, cx)
    }

    /// The proposal as the user left it, if they changed it.
    pub fn edited_text(&self, cx: &App) -> Option<String> {
        let text = self.buffer.read(cx).text();
        (text != self.edit.new_text).then_some(text)
    }

    pub fn set_read_only(&self, cx: &mut App) {
        self.editor.update(cx, |editor, _| editor.set_read_only(true));
    }

    fn build(edit: Edit, languages: Arc<LanguageRegistry>, editable: bool, whole_file: bool, window: &mut Window, cx: &mut App) -> Self {
        let capability = if editable { Capability::ReadWrite } else { Capability::ReadOnly };
        let multibuffer = cx.new(|_| MultiBuffer::without_headers(capability));
        let buffer = cx.new(|cx| Buffer::local(edit.new_text.clone(), cx));
        let editor = cx.new(|cx| {
            let sizing_behavior = if whole_file { SizingBehavior::Default } else { SizingBehavior::SizeByContent };
            let mut editor = Editor::new(
                EditorMode::Full { scale_ui_elements_with_buffer_font_size: false, show_active_line_background: false, sizing_behavior },
                multibuffer.clone(),
                None,
                window,
                cx,
            );
            // Filled red and green: a proposal isn't git's diff, staged or not.
            editor.set_diff_hunk_renderer(Some(Arc::new(editor::HiddenUnstagedDiffHunkRenderer)), cx);
            editor.set_show_gutter(whole_file, cx);
            editor.disable_inline_diagnostics();
            editor.disable_expand_excerpt_buttons(cx);
            editor.set_show_vertical_scrollbar(false, cx);
            editor.set_minimap_visibility(MinimapVisibility::Disabled, window, cx);
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::None, cx);
            editor.scroll_manager.set_forbid_vertical_scroll(!whole_file);
            editor.set_show_indent_guides(false, cx);
            editor.set_read_only(!editable);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_code_actions(false, cx);
            editor.set_show_git_diff_gutter(false, cx);
            editor.set_expand_all_diff_hunks(cx);
            editor
        });
        let (path, old_text) = (edit.path.clone(), edit.old_text.clone().unwrap_or_default());
        let live: Rc<RefCell<Option<Subscription>>> = Rc::default();
        let live_slot = live.clone();
        let live_buffer = buffer.clone();
        let base_text: Arc<str> = old_text.clone().into();
        let view_buffer = buffer.clone();
        let build = cx.spawn(async move |cx: &mut AsyncApp| {
            let language = languages.load_language_for_file_path(Path::new(&path)).await.log_err();
            buffer.update(cx, |b, cx| b.set_language(language, cx));
            buffer.update(cx, |b, _| b.parsing_idle()).await;
            let diff = build_buffer_diff(old_text.into(), &buffer, languages, cx).await;
            multibuffer.update(cx, |multibuffer, cx| {
                let hunks: Vec<_> = {
                    let b = buffer.read(cx);
                    if whole_file {
                        vec![language::Point::zero()..b.max_point()]
                    } else {
                        diff.read(cx)
                            .snapshot(cx)
                            .hunks_intersecting_range(Anchor::min_for_buffer(b.remote_id())..Anchor::max_for_buffer(b.remote_id()), b)
                            .map(|h| h.buffer_range.to_point(b))
                            .collect()
                    }
                };
                multibuffer.set_excerpts_for_path(PathKey::for_buffer(&buffer, cx), buffer.clone(), hunks, excerpt_context_lines(cx), cx);
                multibuffer.add_diff(diff.clone(), cx);
            });
            if editable {
                let subscription = cx.update(|cx| {
                    cx.subscribe(&live_buffer, move |buffer, event: &BufferEvent, cx| {
                        if matches!(event, BufferEvent::Edited { .. }) {
                            refresh_diff(diff.clone(), buffer, base_text.clone(), cx);
                        }
                    })
                });
                *live_slot.borrow_mut() = Some(subscription);
            }
        });
        Self { edit, editor, buffer: view_buffer, _build: build, _live: live }
    }
}

/// Recomputes `diff` against `base_text` after `buffer` changed.
fn refresh_diff(diff: Entity<BufferDiff>, buffer: Entity<Buffer>, base_text: Arc<str>, cx: &mut App) {
    let text_snapshot = buffer.read(cx).text_snapshot();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let base_snapshot = diff.read_with(cx, |d, cx| d.base_text(cx));
        let update = diff.update(cx, |d, cx| d.update_diff(text_snapshot, &base_snapshot, Some(base_text), cx)).await;
        diff.update(cx, |d, cx| d.set_snapshot(update, cx));
    })
    .detach();
}

/// Shows what changed in `editor`'s file since `base` (before the agent's edits): added
/// lines highlighted, removed ones inline, kept current as the file changes. Used by the
/// follow pane. Dropping the returned task stops following the buffer's edits.
pub fn show_changes_since(editor: &Entity<Editor>, base: String, languages: Arc<LanguageRegistry>, cx: &mut App) -> Task<()> {
    let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else { return Task::ready(()) };
    let editor = editor.downgrade();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let base_text: Arc<str> = base.into();
        let diff = build_buffer_diff(base_text.clone(), &buffer, languages, cx).await;
        let shown = editor.update(cx, |editor, cx| {
            editor.buffer().update(cx, |multibuffer, cx| multibuffer.add_diff(diff.clone(), cx));
            editor.set_expand_all_diff_hunks(cx);
        });
        if shown.is_err() {
            return;
        }
        let _edits = cx.update(|cx| {
            cx.subscribe(&buffer, move |buffer, event: &BufferEvent, cx| {
                if matches!(event, BufferEvent::Edited { .. } | BufferEvent::Reloaded) {
                    refresh_diff(diff.clone(), buffer, base_text.clone(), cx);
                }
            })
        });
        futures::future::pending::<()>().await;
    })
}

/// One tab with every file the agent changed, each against its content before the agent
/// (added lines highlighted, removed ones inline), kept current while the agent goes on.
/// `files`: each buffer and its content before the agent; `focus`: the file to show.
pub fn review_editor(
    files: Vec<(Entity<Buffer>, String)>,
    focus: Option<usize>,
    title: String,
    project: Entity<project::Project>,
    languages: Arc<LanguageRegistry>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    let multibuffer = cx.new(|cx| {
        let mut multibuffer = MultiBuffer::new(Capability::ReadWrite).with_title(title);
        for (buffer, _) in &files {
            let end = buffer.read(cx).max_point();
            multibuffer.set_excerpts_for_buffer(buffer.clone(), [language::Point::zero()..end], 0, cx);
        }
        multibuffer
    });
    let editor = cx.new(|cx| {
        let mut editor = Editor::for_multibuffer(multibuffer.clone(), Some(project), window, cx);
        // Every file against its content before the agent, with Keep / Undo on each change.
        crate::agent_review::make_review_editor(&mut editor, cx);
        editor
    });
    if let Some((buffer, _)) = focus.and_then(|ix| files.get(ix)) {
        let at = multibuffer.read(cx).location_for_path(&PathKey::for_buffer(buffer, cx), cx);
        if let Some(at) = at {
            editor.update(cx, |editor, cx| {
                editor.change_selections(editor::SelectionEffects::scroll(editor::scroll::Autoscroll::top_relative(0.)), window, cx, |s| s.select_anchor_ranges([at..at]));
            });
        }
    }
    let _ = languages;
    editor
}

/// A diff of `buffer` against `old_text`, the way Zed's agent builds its own.
pub(crate) async fn build_buffer_diff(old_text: Arc<str>, buffer: &Entity<Buffer>, languages: impl Into<Option<Arc<LanguageRegistry>>>, cx: &mut AsyncApp) -> Entity<BufferDiff> {
    let language = cx.update(|cx| buffer.read(cx).language().cloned());
    let snapshot = cx.update(|cx| buffer.read(cx).snapshot());
    let diff = cx.new(|cx| BufferDiff::new(&snapshot, language, languages.into(), cx));
    diff.update(cx, |d, cx| d.set_base_text(Some(old_text), snapshot.text.clone(), cx)).await;
    diff
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn partial_application_of_hunks() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nB\nc\nd\nE\nf\n";
        let hs = hunks(old, new);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].label(), "line 2");
        assert_eq!(apply_hunks(old, new, &hs, &[true, true]), new);
        assert_eq!(apply_hunks(old, new, &hs, &[false, false]), old);
        assert_eq!(apply_hunks(old, new, &hs, &[true, false]), "a\nB\nc\nd\ne\n");
        assert_eq!(apply_hunks(old, new, &hs, &[false, true]), "a\nb\nc\nd\nE\nf\n");

        let deleted = hunks("x\ny\nz\n", "x\n");
        assert_eq!(deleted[0].label(), "delete 2 lines at 2");
        assert_eq!(hunks("", "new\n").len(), 1, "new files are one hunk");
    }

    #[test]
    fn parses_acp_diffs_and_counts_lines() {
        let call = json!({"content": [
            {"type": "content", "content": {"type": "text", "text": "hi"}},
            {"type": "diff", "path": "/p/a.rs", "oldText": "a\nb\nc\n", "newText": "a\nB\nB2\nc\n"},
            {"type": "diff", "path": "/p/new.rs", "oldText": null, "newText": "x\ny\n"}
        ]});
        let edits = Edit::all_from_tool_call(&call);
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].stats(), (2, 1));
        assert_eq!(edits[1].old_text, None);
        assert_eq!(edits[1].stats(), (2, 0));
    }
}
