//! The conversation as turns: your message, then the agent's part (its answer, thoughts and
//! a compact list of what it did), then a summary of the files that turn changed. Long
//! tool output stays folded unless it matters now (running, failed, waiting for you).

use std::path::PathBuf;

use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, App, Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px};
use markdown::{MarkdownElement, MarkdownFont, MarkdownStyle};
use theme::ActiveTheme as _;
use ui::{Button, ButtonCommon as _, ButtonSize, ButtonStyle, Clickable as _, Color, CommonAnimationExt as _, Icon, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip, h_flex, v_flex};

use super::ThreadView;
use crate::diff::{DiffView, Edit};
use crate::entry_ui::{render_diff, render_entry, render_permission_answer};
use crate::thread::{Entry, Status};

/// What a tool call card shows folded and unfolded.
fn is_running(status: &str) -> bool {
    status == "in_progress" || status == "pending"
}

/// "12s", "1m 05s", "1h 02m".
pub(crate) fn duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

pub(crate) fn spinner(id: impl Into<gpui::ElementId>, size: IconSize, color: Color) -> AnyElement {
    Icon::new(IconName::LoadCircle).size(size).color(color).with_keyed_rotate_animation(id, 1).into_any_element()
}

/// Where each turn starts: entries before the first message, then one range per message.
fn turns(entries: &[Entry]) -> (std::ops::Range<usize>, Vec<std::ops::Range<usize>>) {
    let starts: Vec<usize> = entries.iter().enumerate().filter(|(_, e)| matches!(e, Entry::User(..))).map(|(ix, _)| ix).collect();
    let preamble = 0..starts.first().copied().unwrap_or(entries.len());
    let turns = starts.iter().enumerate().map(|(i, start)| *start..starts.get(i + 1).copied().unwrap_or(entries.len())).collect();
    (preamble, turns)
}

impl ThreadView {
    /// The whole conversation, turn by turn.
    pub(super) fn render_turns(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let thread = self.thread.read(cx);
        let (preamble, turns) = turns(&thread.entries);
        let mut out: Vec<AnyElement> = Vec::new();
        let handle = self.thread.downgrade();
        let app: &App = cx;
        for ix in preamble {
            out.push(render_entry(thread, &handle, ix, &thread.entries[ix], window, app));
        }
        let last_turn = turns.len().saturating_sub(1);
        let mut rendered = Vec::new();
        for (n, turn) in turns.iter().enumerate() {
            rendered.push(self.render_turn(turn.clone(), n == last_turn, window, cx));
        }
        out.extend(rendered);
        out
    }

    fn render_turn(&mut self, turn: std::ops::Range<usize>, is_last: bool, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let thread = self.thread.read(cx);
        let handle = self.thread.downgrade();
        let user_ix = turn.start;
        let working = is_last && thread.status() == Status::Busy;
        let checkpoint = thread.turn_at(user_ix).cloned();
        let follow_terminal = self.follow_terminal(cx);
        let user = render_entry(thread, &handle, user_ix, &thread.entries[user_ix], window, cx);

        // The agent's part: answers and thoughts as they come, tool calls grouped.
        enum Part {
            Tools(Vec<usize>),
            Thought(usize, gpui::Entity<markdown::Markdown>, bool),
            Entry(usize),
        }
        let mut parts: Vec<Part> = Vec::new();
        let last_ix = turn.end.saturating_sub(1);
        for ix in turn.start + 1..turn.end {
            let entry = &thread.entries[ix];
            if matches!(entry, Entry::Tool { .. }) {
                match parts.last_mut() {
                    Some(Part::Tools(tools)) => tools.push(ix),
                    _ => parts.push(Part::Tools(vec![ix])),
                }
                continue;
            }
            if let Entry::Permission { tool_call_id: Some(tid), .. } = entry {
                // Shown in its tool call's row.
                if thread.entries.iter().any(|e| matches!(e, Entry::Tool { id, .. } if id == tid)) {
                    continue;
                }
            }
            parts.push(match entry {
                Entry::Thought(md) => Part::Thought(ix, md.clone(), working && ix == last_ix),
                _ => Part::Entry(ix),
            });
        }
        let mut items: Vec<AnyElement> = Vec::new();
        for part in parts {
            items.push(match part {
                Part::Tools(ixs) => self.render_activity(ixs, follow_terminal.as_deref(), window, cx),
                Part::Thought(ix, md, live) => self.render_thought(ix, md, live, window, cx),
                Part::Entry(ix) => {
                    let thread = self.thread.read(cx);
                    render_entry(thread, &handle, ix, &thread.entries[ix], window, cx)
                }
            });
        }

        let thread = self.thread.read(cx);
        let state: AnyElement = if working {
            let (activity, waiting) = thread.activity().unwrap_or(("Working".into(), false));
            let elapsed = checkpoint.as_ref().map(|c| duration(c.started.elapsed())).unwrap_or_default();
            h_flex()
                .gap_1()
                .min_w_0()
                .child(if waiting { Icon::new(IconName::Warning).size(IconSize::XSmall).color(Color::Warning).into_any_element() } else { spinner(("turn-spinner", user_ix), IconSize::XSmall, Color::Accent) })
                .child(Label::new(activity).size(LabelSize::Small).color(if waiting { Color::Warning } else { Color::Accent }).truncate())
                .child(Label::new(elapsed).size(LabelSize::Small).color(Color::Muted))
                .into_any_element()
        } else {
            match checkpoint.as_ref().and_then(|c| c.duration) {
                Some(d) => Label::new(format!("Worked for {}", duration(d))).size(LabelSize::Small).color(Color::Muted).into_any_element(),
                None => div().into_any_element(),
            }
        };
        let agent_header = h_flex()
            .gap_2()
            .min_w_0()
            .child(Icon::from_path("icons/forge_agents.svg").size(IconSize::Small).color(Color::Accent))
            .child(Label::new(thread.agent_label()).size(LabelSize::Small).color(Color::Default))
            .child(div().flex_1().min_w_0().child(state));
        let summary = checkpoint.filter(|c| c.duration.is_some() && !c.turn_files.is_empty()).map(|c| self.render_turn_changes(user_ix, &c, window, cx));
        let has_agent_part = !items.is_empty() || working;

        v_flex()
            .gap_3()
            .child(user)
            .when(has_agent_part, |el| el.child(v_flex().gap_2().pl_1().child(agent_header).children(items)))
            .children(summary)
            .into_any_element()
    }

    /// A thought: its first line, unfolding on click; the latest one stays open while the
    /// agent is still thinking.
    fn render_thought(&mut self, ix: usize, md: gpui::Entity<markdown::Markdown>, live: bool, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let key = format!("thought-{ix}");
        let open = live != self.toggled.contains(&key);
        let source = md.read(cx).source().to_string();
        let first = source.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default().trim_start_matches('#').trim().to_string();
        let colors = cx.theme().colors().clone();
        v_flex()
            .child(
                h_flex()
                    .id(SharedString::from(key.clone()))
                    .gap_1p5()
                    .cursor_pointer()
                    .child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).size(IconSize::XSmall).color(Color::Muted))
                    .child(if live { spinner(("thought-spinner", ix), IconSize::XSmall, Color::Muted) } else { Icon::new(IconName::ToolThink).size(IconSize::XSmall).color(Color::Muted).into_any_element() })
                    .child(Label::new(if live { "Thinking" } else { "Thought" }).size(LabelSize::Small).color(Color::Muted))
                    .when(!open, |el| el.child(div().min_w_0().flex_1().child(Label::new(first).size(LabelSize::Small).color(Color::Muted).italic().truncate())))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(&key, cx))),
            )
            .when(open, |el| {
                el.child(div().ml_4().mt_1().pl_2().border_l_2().border_color(colors.border_variant).opacity(0.8).child(MarkdownElement::new(md.clone(), MarkdownStyle::themed(MarkdownFont::Agent, window, cx))))
            })
            .into_any_element()
    }

    pub(super) fn toggle(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.toggled.remove(key) {
            self.toggled.insert(key.to_string());
        }
        cx.notify();
    }

    /// The tool call whose terminal the follow pane shows instead of a file.
    pub(super) fn follow_terminal(&self, cx: &App) -> Option<String> {
        (self.follow_visible() && !self.follow_paused).then(|| self.thread.read(cx).running_terminal().map(|(id, _)| id)).flatten()
    }

    /// Consecutive tool calls as one list: a line each, unfolding to their output, diffs
    /// and questions.
    fn render_activity(&mut self, ixs: Vec<usize>, follow_terminal: Option<&str>, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let thread = self.thread.read(cx);
        let handle = self.thread.downgrade();
        let colors = cx.theme().colors().clone();
        let mut rows = v_flex().py_0p5();
        let count = ixs.len();
        for (n, ix) in ixs.into_iter().enumerate() {
            let Entry::Tool { id, title, kind, status, detail, terminal, diffs } = &thread.entries[ix] else { continue };
            let permission = thread.entries.iter().find_map(|e| match e {
                Entry::Permission { request_id, tool_call_id: Some(tid), options, resolved, diffs, .. } if tid == id => Some((request_id.clone(), options, resolved.clone(), diffs)),
                _ => None,
            });
            let waiting = permission.as_ref().is_some_and(|(_, _, resolved, _)| resolved.is_none());
            let running = is_running(status);
            let failed = status == "failed";
            let terminal_view = terminal.as_ref().and_then(|t| thread.terminal_view(t));
            let in_follow = follow_terminal == Some(id.as_str());
            let permission_diffs: Vec<&DiffView> = match &permission {
                Some((_, _, _, pdiffs)) if diffs.is_empty() => pdiffs.iter().collect(),
                _ => Vec::new(),
            };
            let has_body = detail.is_some() || terminal_view.is_some() || !diffs.is_empty() || !permission_diffs.is_empty() || permission.is_some();
            let key = format!("tool-{id}");
            // Open by default when it needs you or is worth watching.
            let default_open = waiting || failed || (running && terminal_view.is_some());
            let open = has_body && (default_open != self.toggled.contains(&key));
            let (added, removed) = diffs.iter().chain(permission_diffs.iter().copied()).map(|d| d.edit.stats()).fold((0, 0), |(a, r), (x, y)| (a + x, r + y));
            let status_icon = if waiting {
                Icon::new(IconName::Warning).size(IconSize::Small).color(Color::Warning).into_any_element()
            } else if running {
                spinner(SharedString::from(format!("tool-spinner-{id}")), IconSize::Small, Color::Accent)
            } else if failed {
                Icon::new(IconName::XCircle).size(IconSize::Small).color(Color::Error).into_any_element()
            } else {
                Icon::new(tool_icon(kind)).size(IconSize::Small).color(Color::Muted).into_any_element()
            };
            let toggle_key = key.clone();
            let row = h_flex()
                .id(SharedString::from(key.clone()))
                .gap_2()
                .px_2()
                .py_1()
                .min_w_0()
                .rounded_sm()
                .when(has_body, |el| el.cursor_pointer().hover(|el| el.bg(colors.element_hover)))
                .child(div().w_3().flex_none().when(has_body, |el| el.child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).size(IconSize::XSmall).color(Color::Muted))))
                .child(div().flex_none().child(status_icon))
                .child(div().flex_1().min_w_0().child(Label::new(title.clone()).size(LabelSize::Small).color(if running || waiting { Color::Default } else { Color::Muted }).truncate()))
                .when(added + removed > 0, |el| {
                    el.child(div().flex_none().child(Label::new(format!("+{added}")).size(LabelSize::Small).color(Color::Created)))
                        .child(div().flex_none().child(Label::new(format!("−{removed}")).size(LabelSize::Small).color(Color::Deleted)))
                })
                .when(in_follow, |el| el.child(div().flex_none().child(Label::new("in the follow pane →").size(LabelSize::Small).color(Color::Muted))))
                .when(has_body, |el| el.on_click(cx.listener(move |this, _, _, cx| this.toggle(&toggle_key, cx))));
            let body = open.then(|| {
                v_flex()
                    .gap_1()
                    .pl(px(36.))
                    .pr_2()
                    .pb_2()
                    .when_some(detail.clone().filter(|_| terminal_view.is_none()), |el, d| el.child(Label::new(d).size(LabelSize::Small).color(Color::Muted).buffer_font(cx)))
                    .when_some(terminal_view.filter(|_| !in_follow), |el, view| el.child(div().rounded_sm().overflow_hidden().border_1().border_color(colors.border_variant).child(view)))
                    .children(diffs.iter().chain(permission_diffs.iter().copied()).map(|d| render_diff(thread, &handle, d, cx)))
                    .when_some(permission.clone(), |el, (request_id, options, resolved, _)| el.child(render_permission_answer(&handle, &request_id, &options, resolved.as_deref())))
            });
            rows = rows.child(v_flex().when(n > 0, |el| el.border_t_1().border_color(colors.border_variant)).child(row).children(body));
        }
        let _ = count;
        v_flex().rounded_md().border_1().border_color(colors.border_variant).bg(colors.surface_background).child(rows).into_any_element()
    }

    /// The files a finished turn changed: a line each with its counts, unfolding to its
    /// diff (from before the turn to after it).
    fn render_turn_changes(&mut self, user_ix: usize, turn: &crate::thread::Checkpoint, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let thread = self.thread.read(cx);
        let root = thread.root().clone();
        let languages = thread.languages().clone();
        let colors = cx.theme().colors().clone();
        let (added, removed) = turn.turn_stats();
        let count = turn.turn_files.len();
        let paths: Vec<PathBuf> = turn.turn_files.keys().cloned().collect();
        let review_paths = paths.clone();
        let header = h_flex()
            .gap_2()
            .px_3()
            .py_1p5()
            .child(Icon::new(IconName::FileDiff).size(IconSize::Small).color(Color::Accent))
            .child(Label::new(format!("{count} file{} changed in this turn", if count == 1 { "" } else { "s" })).size(LabelSize::Small))
            .child(Label::new(format!("+{added}")).size(LabelSize::Small).color(Color::Created))
            .child(Label::new(format!("−{removed}")).size(LabelSize::Small).color(Color::Deleted))
            .child(div().flex_1())
            .child(
                Button::new(("turn-review", user_ix), "Open diffs")
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .start_icon(Icon::new(IconName::Diff).size(IconSize::XSmall))
                    .tooltip(Tooltip::text("This turn's changes in a tab beside the thread"))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_turn_review(user_ix, review_paths.clone(), window, cx))),
            );
        let mut list = v_flex();
        for (n, (path, (before, after))) in turn.turn_files.iter().enumerate() {
            let shown = path.strip_prefix(&root).unwrap_or(path);
            let name = shown.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            let dir = shown.parent().map(|d| d.to_string_lossy().into_owned()).filter(|d| !d.is_empty());
            let edit = Edit { path: path.to_string_lossy().into_owned(), old_text: before.clone(), new_text: after.clone() };
            let (a, r) = edit.stats();
            let key = (user_ix, path.clone());
            let open = self.turn_diffs.contains_key(&key);
            let icon = file_icons::FileIcons::get_icon(path, cx).map(Icon::from_path).unwrap_or_else(|| Icon::new(IconName::File));
            let (toggle_key, toggle_edit, toggle_languages) = (key.clone(), edit.clone(), languages.clone());
            let open_path = path.clone();
            list = list.child(
                v_flex()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(
                        h_flex()
                            .id(("turn-file", user_ix * 1000 + n))
                            .gap_2()
                            .px_3()
                            .py_1()
                            .cursor_pointer()
                            .hover(|el| el.bg(colors.element_hover))
                            .child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).size(IconSize::XSmall).color(Color::Muted))
                            .child(icon.size(IconSize::Small).color(Color::Muted))
                            .child(Label::new(name).size(LabelSize::Small).color(if before.is_none() { Color::Created } else { Color::Default }))
                            .children(dir.map(|d| div().min_w_0().flex_1().child(Label::new(d).size(LabelSize::Small).color(Color::Muted).truncate())))
                            .when(before.is_none(), |el| el.child(Label::new("new").size(LabelSize::Small).color(Color::Created)))
                            .child(Label::new(format!("+{a}")).size(LabelSize::Small).color(Color::Created))
                            .child(Label::new(format!("−{r}")).size(LabelSize::Small).color(Color::Deleted))
                            .child(
                                IconButton::new(("turn-file-open", user_ix * 1000 + n), IconName::ArrowUpRight)
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(Tooltip::text("Open the file"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.open_file(open_path.clone(), window, cx);
                                    })),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if this.turn_diffs.remove(&toggle_key).is_none() {
                                    let view = DiffView::new(toggle_edit.clone(), toggle_languages.clone(), window, cx);
                                    this.turn_diffs.insert(toggle_key.clone(), view);
                                }
                                cx.notify();
                            })),
                    )
                    .when_some(self.turn_diffs.get(&key), |el, view| el.child(div().px_3().pb_2().child(render_diff(thread, &self.thread.downgrade(), view, cx)))),
            );
        }
        let _ = window;
        v_flex().rounded_md().border_1().border_color(colors.border).bg(colors.surface_background).child(header).child(list).into_any_element()
    }
}

/// The icon for a kind of tool call (ACP `kind`).
fn tool_icon(kind: &str) -> IconName {
    match kind {
        "read" => IconName::Eye,
        "edit" => IconName::Pencil,
        "delete" => IconName::Trash,
        "move" => IconName::ArrowRight,
        "search" => IconName::MagnifyingGlass,
        "execute" => IconName::Terminal,
        "think" => IconName::ToolThink,
        "fetch" => IconName::Link,
        _ => IconName::Check,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(duration(std::time::Duration::from_secs(9)), "9s");
        assert_eq!(duration(std::time::Duration::from_secs(65)), "1m 05s");
        assert_eq!(duration(std::time::Duration::from_secs(3720)), "1h 02m");
    }
}
