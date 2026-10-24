//! The follow pane: the file the agent reads or writes (as a diff while it changes it),
//! or the terminal of the command it runs, next to the conversation.

use super::*;

impl ThreadView {
    pub(super) fn follow_visible(&self) -> bool {
        self.follow.is_some() && !self.follow_hidden
    }

    /// Opens `path` beside the thread.
    pub(super) fn open_file(&mut self, path: std::path::PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        workspace.update(cx, |ws, cx| ws.open_abs_path(path, workspace::OpenOptions::default(), window, cx).detach_and_log_err(cx));
    }

    /// Shows the followed file against its content before the agent, when the agent
    /// changed it; back to the plain file once those changes are kept or undone.
    /// Shows the write waiting for review in the followed file, or stops showing it once
    /// it is answered.
    pub(super) fn sync_follow_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = self.follow.as_ref().and_then(|f| self.thread.read(cx).pending_review_for(&f.location.path)).map(|(ix, _, _)| ix);
        match pending {
            Some(ix) if self.follow_review.as_ref().map(|(shown, _)| *shown) != Some(ix) => {
                let thread = self.thread.read(cx);
                let Some(crate::thread::Entry::Review { diff, .. }) = thread.entries.get(ix) else { return };
                // The proposal as the thread's card has it (the user may have edited it there).
                let mut edit = diff.edit.clone();
                if let Some(text) = diff.edited_text(cx) {
                    edit.new_text = text;
                }
                let languages = thread.languages().clone();
                let first_change = crate::diff::hunks(edit.old_text.as_deref().unwrap_or_default(), &edit.new_text).first().map(|h| h.new.start);
                let view = crate::diff::DiffView::whole_file(edit, languages, window, cx);
                if let Some(line) = first_change {
                    let editor = view.editor.clone();
                    // Once the file is in the view: the first change in sight.
                    cx.spawn_in(window, async move |_, cx| {
                        cx.background_executor().timer(std::time::Duration::from_millis(150)).await;
                        cx.update(|window, cx| show_line(&editor, Some(line), window, cx)).ok();
                    })
                    .detach();
                }
                self.follow_review = Some((ix, view));
            }
            Some(_) => {}
            None => self.follow_review = None,
        }
    }

    pub(super) fn update_follow_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(follow) = self.follow.as_ref() else { return };
        let Some(editor) = follow.editor.clone() else { return };
        let thread = self.thread.read(cx);
        let base = thread.changes.iter().find(|c| c.path == follow.location.path).map(|c| c.original.clone().unwrap_or_default());
        match (base, follow.diff_base.is_some()) {
            (Some(base), _) if follow.diff_base.as_ref() != Some(&base) => {
                let languages = thread.languages().clone();
                let task = crate::diff::show_changes_since(&editor, base.clone(), languages, cx);
                let follow = self.follow.as_mut().unwrap();
                follow.diff_base = Some(base);
                follow._diff = Some(task);
            }
            (None, true) => {
                // Reopen the file without the diff.
                let location = follow.location.clone();
                self.follow = None;
                self.follow_to(location, window, cx);
            }
            _ => {}
        }
    }

    /// Moves the follow pane to where the agent is, unless paused or closed.
    pub(super) fn sync_follow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(location) = self.thread.read(cx).location().cloned() else { return };
        if self.follow_paused || self.follow_hidden || self.follow.as_ref().is_some_and(|f| f.location == location) {
            return;
        }
        self.follow_to(location, window, cx);
    }

    pub(super) fn follow_to(&mut self, location: AgentLocation, window: &mut Window, cx: &mut Context<Self>) {
        let same_file = self.follow.as_ref().is_some_and(|f| f.location.path == location.path && f.editor.is_some());
        if same_file {
            let follow = self.follow.as_mut().unwrap();
            follow.location = location.clone();
            if let Some(editor) = follow.editor.clone() {
                show_line(&editor, location.line, window, cx);
            }
            cx.notify();
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else { return };
        let project = workspace.read(cx).project().clone();
        let open = project.update(cx, |p, cx| p.open_local_buffer(&location.path, cx));
        let line = location.line;
        let task = cx.spawn_in(window, async move |this, cx| {
            let Ok(buffer) = open.await else { return };
            this.update_in(cx, |this, window, cx| {
                let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx));
                show_line(&editor, line, window, cx);
                if let Some(follow) = this.follow.as_mut() {
                    follow.editor = Some(editor);
                }
                this.update_follow_diff(window, cx);
                cx.notify();
            })
            .ok();
        });
        self.follow = Some(Follow { location, editor: None, _open: Some(task), diff_base: None, _diff: None });
        cx.notify();
    }

    #[cfg(test)]
    pub(super) fn render_follow_visible(&self, _: &App) -> bool {
        self.follow.is_some() && !self.follow_hidden
    }

    pub(super) fn pause_follow(&mut self, cx: &mut Context<Self>) {
        self.follow_paused = true;
        cx.notify();
    }

    /// Back to where the agent is now.
    pub(super) fn jump_to_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.follow_paused = false;
        self.follow_hidden = false;
        if let Some(location) = self.thread.read(cx).location().cloned() {
            self.follow_to(location, window, cx);
        }
        cx.notify();
    }

    pub(super) fn close_follow(&mut self, cx: &mut Context<Self>) {
        self.follow_hidden = true;
        cx.notify();
    }

    pub(super) fn render_follow(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let follow = self.follow.as_ref().filter(|_| !self.follow_hidden)?;
        let colors = cx.theme().colors().clone();
        let root = self.thread.read(cx).root().clone();
        let shown = follow.location.path.strip_prefix(&root).unwrap_or(&follow.location.path).to_string_lossy().into_owned();
        let agent_at = self.thread.read(cx).location().cloned();
        let review = self.thread.read(cx).pending_review_for(&follow.location.path);
        let line_label = follow.location.line.map(|l| format!("line {}", l + 1)).unwrap_or_default();
        // While a command runs, its terminal; otherwise the file the agent is in.
        let terminal = self.follow_terminal(cx).and_then(|_| {
            let thread = self.thread.read(cx);
            let (_, terminal) = thread.running_terminal()?;
            thread.terminal_view(&terminal)
        });
        // What the pane shows, said as what the agent does: "Editing src/main.rs  line 12",
        // "Running cargo test".
        let what = if terminal.is_some() {
            let running = self.thread.read(cx).running_tool().unwrap_or("a command").to_string();
            h_flex()
                .gap_2()
                .min_w_0()
                .child(Icon::new(IconName::Terminal).size(IconSize::Small).color(Color::Accent))
                .child(Label::new("Running").size(LabelSize::Small).color(Color::Accent))
                .child(Label::new(running).size(LabelSize::Small).truncate())
        } else {
            let verb = match follow.location.kind.as_deref() {
                _ if review.is_some() => "Proposing a change to",
                Some("edit" | "delete" | "move") => "Editing",
                Some("read") => "Reading",
                Some("search") => "Searching in",
                _ => "Looking at",
            };
            h_flex()
                .gap_2()
                .min_w_0()
                .child(Icon::new(IconName::File).size(IconSize::Small).color(Color::Muted))
                .child(div().flex_none().child(Label::new(verb).size(LabelSize::Small).color(if self.follow_paused { Color::Muted } else { Color::Accent })))
                .child(Label::new(shown).size(LabelSize::Small).buffer_font(cx).truncate())
                .child(Label::new(line_label).size(LabelSize::XSmall).color(Color::Muted))
        };
        let status = if self.follow_paused {
            let at = agent_at
                .map(|l| {
                    let file = l.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    match l.line {
                        Some(line) => format!("{file}:{}", line + 1),
                        None => file,
                    }
                })
                .unwrap_or_default();
            h_flex().gap_2().child(Label::new(format!("Paused · the agent is in {at}")).size(LabelSize::XSmall).color(Color::Muted)).child(
                Button::new("follow-jump", "Jump to agent").style(ButtonStyle::Subtle).size(ButtonSize::Compact).on_click(cx.listener(|this, _, window, cx| this.jump_to_agent(window, cx))),
            )
        } else {
            h_flex().child(
                IconButton::new("follow-pause", IconName::DebugPause)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Stay here (stop following the agent)"))
                    .on_click(cx.listener(|this, _, _, cx| this.pause_follow(cx))),
            )
        };
        let header = h_flex()
            .justify_between()
            .gap_2()
            .px_3()
            .py_1p5()
            .border_b_1()
            .border_color(colors.border)
            .child(what)
            .child(
                h_flex().gap_1().child(status).child(
                    IconButton::new("follow-close", IconName::Close).icon_size(IconSize::Small).tooltip(Tooltip::text("Close (the agent keeps working)")).on_click(cx.listener(|this, _, _, cx| this.close_follow(cx))),
                ),
            );
        // The proposed change, shown as a diff of what it adds and removes, to accept here.
        let proposal = self.follow_review.as_ref().filter(|(ix, _)| review.is_some_and(|(r, _, _)| r == *ix)).map(|(ix, view)| (*ix, view.editor.clone()));
        let banner = review.map(|(ix, added, removed)| {
            let thread = self.thread.downgrade();
            let answer = move |accept: bool| {
                let thread = thread.clone();
                move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                    thread.update(cx, |t, cx| t.answer_review(ix, accept, cx)).ok();
                }
            };
            h_flex()
                .gap_2()
                .px_3()
                .py_1()
                .bg(cx.theme().status().info_background)
                .child(Icon::new(IconName::Pencil).size(IconSize::XSmall).color(Color::Info))
                .child(div().flex_1().min_w_0().child(Label::new(format!("The agent proposes this change (+{added} −{removed})")).size(LabelSize::XSmall).truncate()))
                .child(Button::new("follow-accept", "Accept").style(ButtonStyle::Filled).size(ButtonSize::Compact).start_icon(Icon::new(IconName::Check).size(IconSize::XSmall)).on_click(answer(true)))
                .child(Button::new("follow-reject", "Reject").size(ButtonSize::Compact).start_icon(Icon::new(IconName::Close).size(IconSize::XSmall)).on_click(answer(false)))
        });
        // What the agent is doing besides the pane's tool call: waiting for you, thinking…
        let running = self.thread.read(cx).running_tool().map(str::to_string);
        let activity = self.thread.read(cx).activity().filter(|(text, waiting)| *waiting || running.as_deref() != Some(text.as_str())).map(|(text, waiting)| {
            h_flex()
                .gap_2()
                .px_3()
                .py_1()
                .border_b_1()
                .border_color(colors.border_variant)
                .child(if waiting { Icon::new(IconName::Warning).size(IconSize::XSmall).color(Color::Warning).into_any_element() } else { turns::spinner("follow-spinner", IconSize::XSmall, Color::Accent) })
                .child(div().min_w_0().flex_1().child(Label::new(text).size(LabelSize::Small).color(if waiting { Color::Warning } else { Color::Accent }).truncate()))
        });
        let body = match (&terminal, &follow.editor) {
            (Some(view), _) => div().flex_1().min_h_0().p_1().child(view.clone()).into_any_element(),
            (None, _) if proposal.is_some() => div().flex_1().min_h_0().child(proposal.clone().map(|(_, editor)| editor).unwrap()).into_any_element(),
            (None, Some(editor)) => div().flex_1().min_h_0().child(editor.clone()).into_any_element(),
            (None, None) => div().p_4().child(Label::new("Opening…").size(LabelSize::Small).color(Color::Muted)).into_any_element(),
        };
        Some(
            v_flex()
                .w(relative(0.48))
                .flex_shrink_0()
                .h_full()
                .border_l_1()
                .border_color(colors.border)
                .bg(colors.editor_background)
                .child(header)
                .children(activity)
                .when_some(banner.filter(|_| terminal.is_none()), |el, b| el.child(b))
                .child(body)
                .into_any_element(),
        )
    }
}

/// Scrolls `editor` to `line` and highlights it as the agent's.
pub(super) fn show_line(editor: &Entity<Editor>, line: Option<u32>, window: &mut Window, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        editor.clear_row_highlights::<AgentLine>();
        let Some(line) = line else { return };
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let line = line.min(snapshot.max_point().row);
        let anchor = snapshot.anchor_before(Point::new(line, 0));
        let color = |cx: &App| cx.theme().colors().border_focused.opacity(0.18);
        editor.highlight_rows::<AgentLine>(anchor..anchor, color, RowHighlightOptions { autoscroll: true, include_gutter: true }, cx);
        editor.go_to_singleton_buffer_point(Point::new(line, 0), window, cx);
    });
}
