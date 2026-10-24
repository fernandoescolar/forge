//! The files the thread changed: the list under the conversation, and reviewing them
//! (all of them, or one turn's) as diffs.

use super::*;

impl ThreadView {
    /// A tab beside the thread with `paths` against how they were before the turn that
    /// started at the message `user_ix`.
    pub(super) fn open_turn_review(&mut self, user_ix: usize, paths: Vec<std::path::PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(turn) = self.thread.read(cx).turn_at(user_ix).cloned() else { return };
        let baselines: Vec<(std::path::PathBuf, String)> = paths.into_iter().filter_map(|p| turn.turn_files.get(&p).map(|(before, _)| (p.clone(), before.clone().unwrap_or_default()))).collect();
        let title = format!("Changes · turn {}", self.thread.read(cx).checkpoints.iter().position(|c| c.entry == user_ix).map(|n| n + 1).unwrap_or(1));
        self.open_diffs(baselines, None, title, window, cx);
    }

    /// Rolls back the turn that started at the message `user_ix` (only `path` if given),
    /// asking first when a later turn changed those files again.
    pub(super) fn undo_turn(&mut self, user_ix: usize, path: Option<std::path::PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let later = self.thread.read(cx).later_turns_touching(user_ix, path.as_deref());
        if later.is_empty() {
            self.thread.update(cx, |t, cx| t.undo_turn(user_ix, path, cx));
            return;
        }
        let root = self.thread.read(cx).root().clone();
        let names: Vec<String> = later.iter().map(|p| p.strip_prefix(&root).unwrap_or(p).to_string_lossy().into_owned()).collect();
        let detail = format!("Later turns changed {} again; undoing this turn loses those changes too.", names.join(", "));
        let answer = window.prompt(gpui::PromptLevel::Warning, "Undo this turn's changes?", Some(&detail), &["Undo", "Cancel"], cx);
        let thread = self.thread.downgrade();
        cx.spawn(async move |_, cx| {
            if answer.await.ok() == Some(0) {
                thread.update(cx, |t, cx| t.undo_turn(user_ix, path, cx)).ok();
            }
        })
        .detach();
    }

    /// A tab beside the thread (split to the right) with each file against `baseline`.
    pub(super) fn open_diffs(&mut self, baselines: Vec<(std::path::PathBuf, String)>, focus: Option<std::path::PathBuf>, title: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        let languages = self.thread.read(cx).languages().clone();
        let project = workspace.read(cx).project().clone();
        let opens: Vec<_> = baselines.iter().map(|(path, _)| project.update(cx, |p, cx| p.open_local_buffer(path, cx))).collect();
        cx.spawn_in(window, async move |_, cx| {
            let mut files = Vec::new();
            let mut focus_ix = None;
            for ((path, baseline), open) in baselines.into_iter().zip(opens) {
                let Ok(buffer) = open.await else { continue };
                if focus.as_ref() == Some(&path) {
                    focus_ix = Some(files.len());
                }
                files.push((buffer, baseline));
            }
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let editor = crate::diff::review_editor(files, focus_ix, title, project, languages, window, cx);
                    // Beside the thread, so the conversation and its lists stay in view.
                    workspace.split_item(workspace::SplitDirection::Right, Box::new(editor), window, cx);
                })
                .ok();
        })
        .detach();
    }

    /// Lists the files the agent changed (until kept or undone), like Copilot's working
    /// set: each opens in the review tab (its diff against before the agent), and can be
    /// kept or undone; or all at once.
    pub(super) fn render_changes(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Not the files' contents: this runs on every repaint, keystrokes included.
        let changes: Vec<(std::path::PathBuf, bool, (usize, usize))> = self.thread.read(cx).changes.iter().map(|c| (c.path.clone(), c.original.is_none(), c.stats())).collect();
        if changes.is_empty() {
            return None;
        }
        let colors = cx.theme().colors().clone();
        let root = self.thread.read(cx).root().clone();
        let (added, removed) = changes.iter().map(|(_, _, stats)| *stats).fold((0, 0), |(a, r), (x, y)| (a + x, r + y));
        let count = changes.len();
        let header = h_flex()
            .id("changes-header")
            .flex_wrap()
            .gap_2()
            .px_3()
            .py_1()
            .cursor_pointer()
            .child(Icon::new(if self.changes_open { IconName::ChevronDown } else { IconName::ChevronRight }).size(IconSize::XSmall).color(Color::Muted))
            .child(Icon::new(IconName::FileDiff).size(IconSize::Small).color(Color::Accent))
            .child(Label::new(format!("{count} file{} changed", if count == 1 { "" } else { "s" })).size(LabelSize::Small).weight(FontWeight::SEMIBOLD))
            .child(Label::new(format!("+{added}")).size(LabelSize::XSmall).color(Color::Created))
            .child(Label::new(format!("−{removed}")).size(LabelSize::XSmall).color(Color::Deleted))
            .child(div().flex_1())
            .child(
                // Wraps under the summary as one group when the pane is narrow.
                h_flex().gap_1().child(
                Button::new("changes-review", "Review")
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .start_icon(Icon::new(IconName::Diff).size(IconSize::XSmall))
                    .tooltip(Tooltip::text("Every change in one tab, against before the agent"))
                    .on_click(cx.listener(|this, _, window, cx| this.open_review(None, window, cx))),
            )
            .child(
                Button::new("changes-keep-all", "Keep all")
                    .style(ButtonStyle::Filled)
                    .size(ButtonSize::Compact)
                    .start_icon(Icon::new(IconName::Check).size(IconSize::XSmall))
                    .tooltip(Tooltip::text("Accept every change: they stop being listed here"))
                    .on_click(cx.listener(|this, _, _, cx| this.thread.update(cx, |t, cx| t.keep_changes(None, cx)))),
            )
            .child(
                Button::new("changes-undo-all", "Undo all")
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .start_icon(Icon::new(IconName::Undo).size(IconSize::XSmall))
                    .tooltip(Tooltip::text("Put every file back as it was before the agent"))
                    .on_click(cx.listener(|this, _, _, cx| this.thread.update(cx, |t, cx| t.undo_changes(None, cx)))),
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                this.changes_open = !this.changes_open;
                cx.notify();
            }));
        let mut list = v_flex().id("changes-list").max_h(px(220.)).overflow_y_scroll().py_0p5();
        if self.changes_open {
            for (ix, (path, is_new, (a, r))) in changes.iter().enumerate() {
                let shown = path.strip_prefix(&root).unwrap_or(path);
                let name = shown.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let dir = shown.parent().map(|d| d.to_string_lossy().into_owned()).filter(|d| !d.is_empty());
                let (open_path, keep_path, undo_path) = (path.clone(), path.clone(), path.clone());
                let icon = file_icons::FileIcons::get_icon(path, cx).map(Icon::from_path).unwrap_or_else(|| Icon::new(IconName::File));
                list = list.child(
                    h_flex()
                        .id(("change-row", ix))
                        .gap_2()
                        .px_3()
                        .py_0p5()
                        .cursor_pointer()
                        .hover(|el| el.bg(colors.element_hover))
                        .tooltip(Tooltip::text("Open its diff"))
                        .child(icon.size(IconSize::Small).color(Color::Muted))
                        .child(Label::new(name).size(LabelSize::Small).color(if *is_new { Color::Created } else { Color::Modified }))
                        .children(dir.map(|d| div().min_w_0().child(Label::new(d).size(LabelSize::XSmall).color(Color::Muted).truncate())))
                        .when(*is_new, |el| el.child(Label::new("new").size(LabelSize::XSmall).color(Color::Created)))
                        .child(Label::new(format!("+{a}")).size(LabelSize::XSmall).color(Color::Created))
                        .child(Label::new(format!("−{r}")).size(LabelSize::XSmall).color(Color::Deleted))
                        .child(div().flex_1())
                        .child(
                            IconButton::new(("change-keep", ix), IconName::Check)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Keep these changes"))
                                .on_click(cx.listener(move |this, _, _, cx| this.thread.update(cx, |t, cx| t.keep_changes(Some(&keep_path), cx)))),
                        )
                        .child(
                            IconButton::new(("change-undo", ix), IconName::Undo)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text(if *is_new { "Undo: delete the file the agent created" } else { "Undo: back to before the agent" }))
                                .on_click(cx.listener(move |this, _, _, cx| this.thread.update(cx, |t, cx| t.undo_changes(Some(undo_path.clone()), cx)))),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| this.open_review(Some(open_path.clone()), window, cx))),
                );
            }
        }
        Some(centered(
            v_flex()
                .w_full()
                .mb_2()
                .rounded_md()
                .border_1()
                .border_color(colors.border)
                .bg(colors.surface_background)
                .child(header)
                .when(self.changes_open, |el| el.child(div().border_t_1().border_color(colors.border_variant).child(list))),
        ))
    }

    /// The review tab: every changed file against before the agent, at `focus` if given,
    /// beside the thread.
    pub(super) fn open_review(&mut self, focus: Option<std::path::PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let thread = self.thread.read(cx);
        let baselines = thread.changes.iter().map(|c| (c.path.clone(), c.original.clone().unwrap_or_default())).collect();
        let title = format!("Changes · {}", thread.title().unwrap_or_else(|| "agent".into()));
        self.open_diffs(baselines, focus, title, window, cx);
    }
}
