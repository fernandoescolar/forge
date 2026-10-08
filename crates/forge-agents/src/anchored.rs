//! Threads anchored in the code: `ctrl-enter` in an editor opens a question right below
//! the selection (or the cursor's line). Sending it starts a thread with that code as
//! context; the conversation then lives in a card between the lines (latest answer,
//! the agent's questions and changes waiting for you, the files it changed, a reply box),
//! folds to one line, and opens in full in the thread tab. While the agent changes the
//! file the card is in, the editor shows those changes inline.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use editor::{
    Editor, EditorEvent,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    Subscription, WeakEntity, Window, actions, div,
};
use language::Point;
use markdown::{MarkdownElement, MarkdownFont, MarkdownStyle};
use theme::ActiveTheme as _;
use ui::{
    Button, ButtonCommon as _, ButtonSize, ButtonStyle, Clickable as _, Color, CommonAnimationExt as _, Disableable as _, Icon, IconButton, IconName,
    IconSize, Label, LabelCommon as _, LabelSize, StyledTypography as _, Tooltip, h_flex, v_flex,
};
use workspace::Workspace;

use crate::{
    thread::{Entry, Status, Thread, ThreadEvent},
    threads::{self, Send},
};

actions!(forge_agent, [AskHere, DismissAsk]);

/// Block heights, in lines.
const COMPOSING_LINES: u32 = 3;
const EXPANDED_LINES: u32 = 14;
/// While something waits for the user (a question, a change to review): room for its diff.
const WAITING_LINES: u32 = 26;
const FOLDED_LINES: u32 = 2;

pub fn init(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("ctrl-enter", AskHere, Some("Editor && mode == full")),
        gpui::KeyBinding::new("escape", DismissAsk, Some("ForgeAnchoredAsk > Editor")),
    ]);
    cx.observe_new(|editor: &mut Editor, window, cx| {
        if window.is_none() || !editor.mode().is_full() {
            return;
        }
        let weak = cx.entity().downgrade();
        editor
            .register_action(move |_: &AskHere, window, cx| {
                if let Some(editor) = weak.upgrade() {
                    ask_here(&editor, window, cx);
                }
            })
            .detach();
    })
    .detach();
}

/// The code a question is about.
#[derive(Clone)]
struct CodeContext {
    /// Path relative to the project root, for `@` mentions and labels.
    path: Option<String>,
    language: String,
    first_line: u32,
    last_line: u32,
    text: String,
}

impl CodeContext {
    fn label(&self) -> String {
        let lines = if self.first_line == self.last_line { format!("line {}", self.first_line) } else { format!("lines {}–{}", self.first_line, self.last_line) };
        match &self.path {
            Some(path) => format!("{path}, {lines}"),
            None => lines,
        }
    }

    /// The question with the code it is about.
    fn prompt(&self, question: &str) -> String {
        let about = match &self.path {
            Some(path) => format!("About @{path} ({}):", self.label_lines()),
            None => format!("About this code ({}):", self.label_lines()),
        };
        format!("{question}\n\n{about}\n```{}\n{}\n```", self.language, self.text.trim_end())
    }

    fn label_lines(&self) -> String {
        if self.first_line == self.last_line { format!("line {}", self.first_line) } else { format!("lines {}–{}", self.first_line, self.last_line) }
    }
}

/// Opens a question card below the newest selection of `editor`.
pub(crate) fn ask_here(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) -> Option<Entity<AnchoredCard>> {
    let workspace = editor.read(cx).workspace()?;
    let (anchor, context) = editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let selection = editor.selections.newest::<Point>(&display);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let (start, end) = (selection.start.min(selection.end), selection.start.max(selection.end));
        let (first, last) = if start == end { (start.row, start.row) } else { (start.row, if end.column == 0 && end.row > start.row { end.row - 1 } else { end.row }) };
        let range = Point::new(first, 0)..Point::new(last, snapshot.line_len(multi_buffer::MultiBufferRow(last)));
        let text: String = snapshot.text_for_range(range).collect();
        let anchor = snapshot.anchor_after(Point::new(last, 0));

        let buffer = editor.buffer().read(cx).as_singleton();
        let root = workspace.read(cx).project().read(cx).visible_worktrees(cx).next().map(|t| t.read(cx).abs_path().to_path_buf());
        let path = buffer.as_ref().and_then(|b| b.read(cx).file()).and_then(|f| f.as_local()).map(|f| f.abs_path(cx)).map(|abs: PathBuf| match &root {
            Some(root) => abs.strip_prefix(root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| abs.to_string_lossy().into_owned()),
            None => abs.to_string_lossy().into_owned(),
        });
        let language = buffer.and_then(|b| b.read(cx).language().map(|l| l.name().as_ref().to_lowercase())).unwrap_or_default();
        (anchor, CodeContext { path, language, first_line: first + 1, last_line: last + 1, text })
    });

    let card = cx.new(|cx| AnchoredCard::new(editor.downgrade(), workspace.downgrade(), context, window, cx));
    let render_card = card.clone();
    let block = editor.update(cx, |editor, cx| {
        editor.insert_blocks(
            [BlockProperties {
                placement: BlockPlacement::Below(anchor),
                height: Some(COMPOSING_LINES),
                style: BlockStyle::Sticky,
                render: Arc::new(move |cx| div().w_full().pl(cx.margins.gutter.full_width()).pr_4().py_0p5().child(render_card.clone()).into_any_element()),
                priority: 0,
            }],
            None,
            cx,
        )[0]
    });
    card.update(cx, |card, _| card.block = Some(block));
    let focus = card.read(cx).prompt.focus_handle(cx);
    window.focus(&focus, cx);
    Some(card)
}

pub struct AnchoredCard {
    editor: WeakEntity<Editor>,
    workspace: WeakEntity<Workspace>,
    block: Option<CustomBlockId>,
    context: CodeContext,
    /// The question, then replies once the thread exists.
    prompt: Entity<Editor>,
    thread: Option<Entity<Thread>>,
    expanded: bool,
    /// The block's current height, in lines.
    lines: u32,
    /// The agent's changes to this editor's file shown inline: the file before them.
    inline_diff: Option<(String, gpui::Task<()>)>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl AnchoredCard {
    fn new(editor: WeakEntity<Editor>, workspace: WeakEntity<Workspace>, context: CodeContext, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let prompt = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 4, window, cx);
            editor.set_placeholder_text("Ask about this code — Enter to send, Esc or click away to close", window, cx);
            editor
        });
        let prompt_focus = prompt.focus_handle(cx);
        let subscriptions = vec![
            cx.subscribe(&prompt, |_, _, _: &EditorEvent, cx| cx.notify()),
            // A question left empty goes away when you move on, like with Esc.
            cx.on_blur(&prompt_focus, window, |this, window, cx| {
                if this.is_empty_question(cx) {
                    this.remove(window, cx);
                }
            }),
        ];
        Self {
            editor,
            workspace,
            block: None,
            context,
            prompt,
            thread: None,
            expanded: true,
            lines: COMPOSING_LINES,
            inline_diff: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    #[cfg(test)]
    fn thread(&self) -> Option<&Entity<Thread>> {
        self.thread.as_ref()
    }

    fn resize(&mut self, lines: u32, cx: &mut App) {
        let (Some(editor), Some(block)) = (self.editor.upgrade(), self.block) else { return };
        if self.lines == lines {
            return;
        }
        self.lines = lines;
        editor.update(cx, |editor, cx| editor.resize_blocks(HashMap::from_iter([(block, lines)]), None, cx));
    }

    /// Whether the agent waits for the user: a question or a change to review.
    fn waiting(&self, cx: &App) -> bool {
        self.thread.as_ref().is_some_and(|t| {
            t.read(cx).entries.iter().any(|e| matches!(e, Entry::Permission { resolved: None, .. } | Entry::Review { reply: Some(_), .. }))
        })
    }

    /// After every thread update: open and grow the card while the agent waits for the
    /// user, and show the agent's changes to this file in the editor.
    fn thread_updated(&mut self, cx: &mut Context<Self>) {
        if self.waiting(cx) {
            self.expanded = true;
            self.resize(WAITING_LINES, cx);
        } else if self.expanded {
            self.resize(EXPANDED_LINES, cx);
        }
        self.update_inline_diff(cx);
        cx.notify();
    }

    fn update_inline_diff(&mut self, cx: &mut Context<Self>) {
        let (Some(editor), Some(thread)) = (self.editor.upgrade(), self.thread.as_ref()) else { return };
        let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else { return };
        let Some(path) = buffer.read(cx).file().and_then(|f| f.as_local()).map(|f| f.abs_path(cx)) else { return };
        let thread = thread.read(cx);
        let base = thread.changes.iter().find(|c| c.path == path).map(|c| c.original.clone().unwrap_or_default());
        match base {
            Some(base) if self.inline_diff.as_ref().map(|(b, _)| b) != Some(&base) => {
                let task = crate::diff::show_changes_since(&editor, base.clone(), thread.languages().clone(), cx);
                self.inline_diff = Some((base, task));
            }
            None if self.inline_diff.take().is_some() => {
                // Kept or undone: back to the editor's usual diff, against git.
                let Some(project) = editor.read(cx).project().cloned() else { return };
                let open = project.update(cx, |p, cx| p.open_uncommitted_diff(buffer, cx));
                let editor = editor.downgrade();
                cx.spawn(async move |_, cx| {
                    if let Ok(diff) = open.await {
                        editor.update(cx, |editor, cx| editor.buffer().update(cx, |mb, cx| mb.add_diff(diff, cx))).ok();
                    }
                })
                .detach();
            }
            _ => {}
        }
    }

    /// Not asked yet and nothing typed.
    fn is_empty_question(&self, cx: &App) -> bool {
        self.thread.is_none() && self.prompt.read(cx).text(cx).trim().is_empty()
    }

    /// Esc: an unasked question goes away; a conversation stays and focus returns to the code.
    fn dismiss(&mut self, _: &DismissAsk, window: &mut Window, cx: &mut Context<Self>) {
        if self.thread.is_none() {
            self.remove(window, cx);
        } else if let Some(editor) = self.editor.upgrade() {
            window.focus(&editor.focus_handle(cx), cx);
        }
    }

    /// Takes the card out of the code (a thread it started stays in Threads).
    fn remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(editor), Some(block)) = (self.editor.upgrade(), self.block.take()) else { return };
        editor.update(cx, |editor, cx| {
            editor.remove_blocks(HashSet::from_iter([block]), None, cx);
            window.focus(&editor.focus_handle(cx), cx);
        });
    }

    fn submit(&mut self, _: &Send, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.prompt.read(cx).text(cx).trim().to_string();
        if text.is_empty() {
            return;
        }
        match &self.thread {
            None => {
                let Some(workspace) = self.workspace.upgrade() else { return };
                let thread = workspace.update(cx, |ws, cx| cx.new(|cx| Thread::new(ws, None, window, cx)));
                self.start(thread, &text, window, cx);
            }
            Some(thread) => {
                let thread = thread.clone();
                match thread.read(cx).status() {
                    Status::Ready => {
                        thread.update(cx, |t, cx| t.send(text, None, cx));
                    }
                    Status::Disconnected => thread.update(cx, |t, cx| t.ask(text, window, cx)),
                    Status::Connecting | Status::Busy => return,
                }
            }
        }
        self.prompt.update(cx, |e, cx| e.set_text("", window, cx));
        cx.notify();
    }

    /// Asks `question` about the card's code in `thread`, which joins the workspace's threads.
    pub(crate) fn start(&mut self, thread: Entity<Thread>, question: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |ws, cx| threads::register_thread(ws, thread.clone(), cx));
        }
        self._subscriptions.push(cx.subscribe(&thread, |this, _, _: &ThreadEvent, cx| this.thread_updated(cx)));
        let prompt = self.context.prompt(question);
        thread.update(cx, |t, cx| t.ask(prompt, window, cx));
        self.thread = Some(thread);
        self.prompt.update(cx, |e, cx| {
            e.set_text("", window, cx);
            e.set_placeholder_text("Reply…", window, cx);
        });
        self.expanded = true;
        self.resize(EXPANDED_LINES, cx);
        cx.notify();
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.expanded && self.waiting(cx) {
            // Folding would hide what the agent waits for.
            return;
        }
        self.expanded = !self.expanded;
        self.resize(if self.expanded { EXPANDED_LINES } else { FOLDED_LINES }, cx);
        cx.notify();
    }

    fn open_thread(&self, window: &mut Window, cx: &mut App) {
        let (Some(workspace), Some(thread)) = (self.workspace.upgrade(), self.thread.clone()) else { return };
        workspace.update(cx, |ws, cx| threads::open_view(ws, thread, window, cx));
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_text = !self.prompt.read(cx).text(cx).trim().is_empty();
        h_flex()
            .gap_2()
            .child(div().key_context("ForgeAgentInput").flex_1().min_w_0().child(self.prompt.clone()))
            .child(
                Button::new("anchored-send", if self.thread.is_some() { "Reply" } else { "Ask" })
                    .style(ButtonStyle::Filled)
                    .size(ButtonSize::Compact)
                    .disabled(!has_text)
                    .on_click(cx.listener(|this, _, window, cx| this.submit(&Send, window, cx))),
            )
    }
}

impl Render for AnchoredCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let accent = colors.border_focused;
        let card = v_flex()
            .key_context("ForgeAnchoredAsk")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::dismiss))
            .size_full()
            .gap_1()
            .px_3()
            .py_1p5()
            .rounded_lg()
            .border_1()
            .border_color(accent.opacity(0.6))
            .bg(colors.elevated_surface_background)
            .font_ui(cx);

        let Some(thread) = self.thread.clone() else {
            // Composing the question.
            return card
                .child(
                    h_flex()
                        .gap_2()
                        .child(Icon::from_path("icons/forge_agents.svg").size(IconSize::XSmall).color(Color::Accent))
                        .child(Label::new(format!("Ask about {}", self.context.label())).size(LabelSize::XSmall).color(Color::Muted)),
                )
                .child(self.render_composer(cx))
                .into_any_element();
        };

        let t = thread.read(cx);
        let title = t.title().unwrap_or_else(|| "New thread".into());
        let (summary, summary_color) = t.summary();
        let status = t.status();
        let reviews = t.entries.iter().filter(|e| matches!(e, Entry::Review { reply: Some(_), .. })).count();
        let answer = t.entries.iter().rev().find_map(|e| match e {
            Entry::Agent(md) => Some(md.clone()),
            _ => None,
        });

        let header = h_flex()
            .justify_between()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(Icon::from_path("icons/forge_agents.svg").size(IconSize::Small).color(Color::Accent))
                    .child(Label::new(title).size(LabelSize::Small).weight(gpui::FontWeight::SEMIBOLD).truncate())
                    .child(Label::new(summary).size(LabelSize::XSmall).color(summary_color)),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        Button::new("anchored-open", "Open thread")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .on_click(cx.listener(|this, _, window, cx| this.open_thread(window, cx))),
                    )
                    .child(
                        IconButton::new("anchored-fold", if self.expanded { IconName::ChevronUp } else { IconName::ChevronDown })
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(if self.expanded { "Fold" } else { "Unfold" }))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle(cx))),
                    )
                    .child(
                        IconButton::new("anchored-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Close (the thread stays in Threads)"))
                            .on_click(cx.listener(|this, _, window, cx| this.remove(window, cx))),
                    ),
            );
        if !self.expanded {
            return card.child(header).into_any_element();
        }

        let body = div()
            .id("anchored-answer")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(match answer {
                Some(md) => MarkdownElement::new(md, MarkdownStyle::themed(MarkdownFont::Agent, window, cx)).into_any_element(),
                None => h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::ArrowCircle).size(IconSize::XSmall).color(Color::Accent).with_rotate_animation(2))
                    .child(Label::new(if status == Status::Connecting { "Connecting to the agent…" } else { "Working…" }).size(LabelSize::Small).color(Color::Muted))
                    .into_any_element(),
            });

        let handle = thread.downgrade();
        // What the agent waits for: its questions and the changes to review, with their diffs.
        let mut waiting = v_flex().gap_2();
        for (ix, entry) in t.entries.iter().enumerate() {
            match entry {
                Entry::Permission { request_id, title, options, resolved: None, diffs, .. } => {
                    waiting = waiting.child(
                        v_flex()
                            .gap_1()
                            .p_2()
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().status().warning_border)
                            .bg(cx.theme().status().warning_background)
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(Icon::new(IconName::Warning).size(IconSize::Small).color(Color::Warning))
                                    .child(div().flex_1().min_w_0().child(Label::new(title.clone()).size(LabelSize::Small))),
                            )
                            .children(diffs.iter().map(|d| crate::entry_ui::render_diff(t, &handle, d, cx)))
                            .child(crate::entry_ui::render_permission_answer(&handle, request_id, options, None)),
                    );
                }
                Entry::Review { diff, reply: Some(_), .. } => {
                    let (accept, reject) = (handle.clone(), handle.clone());
                    waiting = waiting.child(
                        v_flex()
                            .gap_1()
                            .child(crate::entry_ui::render_diff(t, &handle, diff, cx))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Button::new(("anchored-review-accept", ix), "Accept").style(ButtonStyle::Filled).size(ButtonSize::Compact).on_click(move |_, _, cx| {
                                        accept.update(cx, |t, cx| t.answer_review(ix, true, cx)).ok();
                                    }))
                                    .child(Button::new(("anchored-review-reject", ix), "Reject").size(ButtonSize::Compact).on_click(move |_, _, cx| {
                                        reject.update(cx, |t, cx| t.answer_review(ix, false, cx)).ok();
                                    })),
                            ),
                    );
                }
                _ => {}
            }
        }
        let is_waiting = t.entries.iter().any(|e| matches!(e, Entry::Permission { resolved: None, .. } | Entry::Review { reply: Some(_), .. }));
        // The files the agent changed: keep or undo them from here too.
        let changes_row = (!t.changes.is_empty()).then(|| {
            let (added, removed) = t.changes.iter().map(|c| c.stats()).fold((0, 0), |(a, r), (x, y)| (a + x, r + y));
            let count = t.changes.len();
            let (keep, undo) = (handle.clone(), handle.clone());
            h_flex()
                .gap_2()
                .child(Icon::new(IconName::FileDiff).size(IconSize::XSmall).color(Color::Accent))
                .child(Label::new(format!("{count} file{} changed", if count == 1 { "" } else { "s" })).size(LabelSize::Small))
                .child(Label::new(format!("+{added}")).size(LabelSize::XSmall).color(Color::Created))
                .child(Label::new(format!("−{removed}")).size(LabelSize::XSmall).color(Color::Deleted))
                .child(div().flex_1())
                .child(Button::new("anchored-keep", "Keep").style(ButtonStyle::Filled).size(ButtonSize::Compact).on_click(move |_, _, cx| {
                    keep.update(cx, |t, cx| t.keep_changes(None, cx)).ok();
                }))
                .child(Button::new("anchored-undo", "Undo").size(ButtonSize::Compact).on_click(move |_, _, cx| {
                    undo.update(cx, |t, cx| t.undo_changes(None, cx)).ok();
                }))
        });
        let review_row = (reviews > 1).then(|| {
            let (accept, reject) = (handle.clone(), handle.clone());
            h_flex()
                .gap_2()
                .child(Label::new(format!("{reviews} change{} to review", if reviews == 1 { "" } else { "s" })).size(LabelSize::Small).color(Color::Accent))
                .child(Button::new("anchored-accept", "Accept").style(ButtonStyle::Filled).size(ButtonSize::Compact).on_click(move |_, _, cx| {
                    accept.update(cx, |t, cx| t.answer_all_reviews(true, cx)).ok();
                }))
                .child(Button::new("anchored-reject", "Reject").size(ButtonSize::Compact).on_click(move |_, _, cx| {
                    reject.update(cx, |t, cx| t.answer_all_reviews(false, cx)).ok();
                }))
                .child(Button::new("anchored-review", "Review in thread").style(ButtonStyle::Subtle).size(ButtonSize::Compact).on_click(cx.listener(|this, _, window, cx| this.open_thread(window, cx))))
        });

        // The answer only while nothing waits: the question takes the room.
        let body = if is_waiting { div().id("anchored-waiting").flex_1().min_h_0().overflow_y_scroll().child(waiting).into_any_element() } else { body.into_any_element() };
        card.child(header)
            .child(body)
            .when_some(review_row, |el, row| el.child(row))
            .when_some(changes_row, |el, row| el.child(row))
            .child(self.render_composer(cx))
            .into_any_element()
    }
}

impl Focusable for AnchoredCard {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.prompt.focus_handle(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::Fs as _;
    use gpui::{TestAppContext, VisualTestContext};
    use ide_api::AgentSpec;
    use project::Project;
    use serde_json::json;
    use std::time::{Duration, Instant};

    async fn wait_for(cx: &mut VisualTestContext, thread: &Entity<Thread>, what: &str, pred: impl Fn(&Thread) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if thread.read_with(cx, |t, _| pred(t)) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            cx.background_executor.timer(Duration::from_millis(20)).await;
        }
    }

    fn blocks(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
        editor.update_in(cx, |editor, window, cx| {
            let snapshot = editor.snapshot(window, cx);
            snapshot.blocks_in_range(editor::display_map::DisplayRow(0)..editor::display_map::DisplayRow(u32::MAX)).filter(|(_, b)| matches!(b, editor::display_map::Block::Custom(_))).count()
        })
    }

    /// ctrl-enter on a line opens a question there; the thread it starts carries the code,
    /// answers in the card, and its change can be accepted without leaving the file.
    #[gpui::test]
    async fn asks_about_code_in_place(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({ "a.txt": "one\ntwo\nthree\n" })).await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let item = workspace
            .update_in(cx, |ws, window, cx| ws.open_abs_path("/root/a.txt".into(), workspace::OpenOptions::default(), window, cx))
            .await
            .unwrap();
        let editor = item.downcast::<Editor>().unwrap();
        editor.update_in(cx, |editor, window, cx| editor.go_to_singleton_buffer_point(Point::new(1, 0), window, cx));
        let before = blocks(&editor, cx);

        // The real keys: ctrl-enter opens a question, escape closes it.
        let focus_code = |cx: &mut VisualTestContext| editor.update_in(cx, |editor, window, cx| window.focus(&editor.focus_handle(cx), cx));
        focus_code(cx);
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(blocks(&editor, cx), before + 1, "ctrl-enter opens a question card");
        cx.simulate_keystrokes("escape");
        assert_eq!(blocks(&editor, cx), before, "escape closes it");

        // Left empty, it also goes away when focus moves on; with text, it stays.
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(blocks(&editor, cx), before + 1);
        focus_code(cx);
        cx.run_until_parked();
        assert_eq!(blocks(&editor, cx), before, "an empty question closes on blur");
        cx.simulate_keystrokes("ctrl-enter");
        cx.simulate_input("half a question");
        focus_code(cx);
        cx.run_until_parked();
        assert_eq!(blocks(&editor, cx), before + 1, "a question with text survives losing focus");
        cx.simulate_keystrokes("ctrl-enter");
        cx.run_until_parked();
        let open_cards = blocks(&editor, cx);
        focus_code(cx);
        cx.run_until_parked();
        assert_eq!(blocks(&editor, cx), open_cards - 1, "the second, empty one closed; the typed one is still there");
        let before = blocks(&editor, cx);

        let card = cx.update(|window, cx| ask_here(&editor, window, cx)).expect("a question card opens");
        assert_eq!(blocks(&editor, cx), before + 1, "the card sits between the lines");
        let (label, prompt) = card.read_with(cx, |c, _| (c.context.label(), c.context.prompt("what is this?")));
        assert_eq!(label, "a.txt, line 2");
        assert!(prompt.starts_with("what is this?") && prompt.contains("@a.txt") && prompt.contains("two"), "{prompt}");

        let agent = AgentSpec {
            id: "mock".into(),
            command: "python3".into(),
            args: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
            env: vec![],
            cwd: Some(tmp.path().to_path_buf()),
        };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let thread = workspace.update_in(cx, |ws, window, cx| cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx)));
        card.update_in(cx, |c, window, cx| c.start(thread.clone(), "what is this?", window, cx));
        wait_for(cx, &thread, "an answer", |t| t.status() == Status::Ready && t.entries.iter().any(|e| matches!(e, Entry::Agent(_)))).await;
        assert_eq!(card.read_with(cx, |c, _| c.thread().cloned()), Some(thread.clone()));
        let in_store = workspace.read_with(cx, |_, cx| threads::store_for(workspace.entity_id(), cx).unwrap().read(cx).threads().contains(&thread));
        assert!(in_store, "the anchored thread is listed with the others");
        cx.run_until_parked();

        // A change proposed from the card is accepted from the card.
        card.update_in(cx, |c, window, cx| {
            c.prompt.update(cx, |e, cx| e.set_text("edit a.txt ONE", window, cx));
            c.submit(&Send, window, cx);
        });
        wait_for(cx, &thread, "the change to review", |t| t.pending_reviews() == 1).await;
        cx.run_until_parked();
        assert_eq!(card.read_with(cx, |c, _| (c.expanded, c.lines)), (true, WAITING_LINES), "the card opens up for the change to review");
        thread.update(cx, |t, cx| t.answer_all_reviews(true, cx));
        wait_for(cx, &thread, "the turn to end", |t| t.status() == Status::Ready && t.pending_reviews() == 0).await;
        cx.run_until_parked();
        assert_eq!(fs.load("/root/a.txt".as_ref()).await.unwrap(), "ONE\n");
        // The change is listed, and shown inline in this file's editor until kept.
        assert!(thread.read_with(cx, |t, _| t.changes.iter().any(|c| c.path == std::path::Path::new("/root/a.txt"))));
        assert_eq!(card.read_with(cx, |c, _| c.inline_diff.as_ref().map(|(base, _)| base.clone())), Some("one\ntwo\nthree\n".to_string()));
        assert_eq!(card.read_with(cx, |c, _| c.lines), EXPANDED_LINES, "back to its usual size");
        thread.update(cx, |t, cx| t.keep_changes(None, cx));
        cx.run_until_parked();
        assert!(card.read_with(cx, |c, _| c.inline_diff.is_none()), "kept: the editor shows its usual diff again");

        // The agent's questions show in the card too, and are answered there.
        card.update_in(cx, |c, window, cx| {
            c.prompt.update(cx, |e, cx| e.set_text("run echo from-the-card", window, cx));
            c.submit(&Send, window, cx);
        });
        wait_for(cx, &thread, "the permission request", |t| t.entries.iter().any(|e| matches!(e, Entry::Permission { resolved: None, .. }))).await;
        cx.run_until_parked();
        assert!(card.read_with(cx, |c, cx| c.waiting(cx)), "the card waits for the answer");
        let request = thread.read_with(cx, |t, _| {
            t.entries.iter().find_map(|e| match e {
                Entry::Permission { request_id, resolved: None, options, .. } => Some((request_id.clone(), options.iter().find(|o| o.allow).map(|o| (o.id.clone(), o.name.clone())))),
                _ => None,
            })
        });
        let (request_id, allow) = request.unwrap();
        thread.update(cx, |t, cx| t.answer_permission(request_id, allow, cx));
        wait_for(cx, &thread, "the command to run", |t| t.status() == Status::Ready).await;
        cx.run_until_parked();
        assert!(!card.read_with(cx, |c, cx| c.waiting(cx)));

        // With a conversation, Esc only returns to the code; losing focus keeps it too.
        card.update_in(cx, |c, window, cx| c.dismiss(&DismissAsk, window, cx));
        cx.run_until_parked();
        assert_eq!(blocks(&editor, cx), before + 1, "a conversation stays on Esc");
        assert!(editor.update_in(cx, |e, window, cx| e.focus_handle(cx).is_focused(window)), "focus is back in the code");

        card.update_in(cx, |c, _, cx| c.toggle(cx));
        assert!(!card.read_with(cx, |c, _| c.expanded));
        card.update_in(cx, |c, window, cx| c.remove(window, cx));
        assert_eq!(blocks(&editor, cx), before, "the close button removes the card");
    }
}
