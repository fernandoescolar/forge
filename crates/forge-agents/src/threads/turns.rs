//! The conversation as turns: your message, then the agent's part (its answer, thoughts and
//! a compact list of what it did). The files changed so far are listed once, above the
//! input, with undoing the last turn (`render_changes`). Long
//! tool output stays folded unless it matters now (running, failed, waiting for you).
//!
//! Each message, answer, thought and tool call is a row of its own, so the list paints only
//! what is on screen: a turn of hundreds of tool calls costs a repaint no more than a short
//! one (a row per turn made every keystroke repaint the whole turn).

use std::collections::{HashMap, HashSet};

use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, App, Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px};
use markdown::{MarkdownElement, MarkdownFont, MarkdownStyle};
use theme::ActiveTheme as _;
use ui::{Color, CommonAnimationExt as _, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, h_flex, v_flex};

use super::ThreadView;
use crate::diff::DiffView;
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

/// A row of the conversation list.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Row {
    /// An entry before the first message.
    Preamble(usize),
    /// Your message, starting a turn.
    User(usize),
    /// The agent's name over its part of the turn that `user` starts (`last`: the latest turn).
    Agent { user: usize, last: bool },
    /// A tool call, in a run of consecutive ones drawn as one box.
    Tool { ix: usize, first: bool, last: bool },
    /// A thought; `latest`: the last entry of the latest turn (open while the agent thinks).
    Thought { ix: usize, latest: bool },
    /// Anything else the agent's part shows (answers, cards).
    Entry(usize),
}

/// The conversation's rows, and the permission request (entry) shown in each tool call's
/// row, by tool call id. `busy`: the agent is working on the latest turn.
pub(super) fn rows(entries: &[Entry], busy: bool) -> (Vec<Row>, HashMap<String, usize>) {
    let tool_ids: HashSet<&str> = entries.iter().filter_map(|e| if let Entry::Tool { id, .. } = e { Some(id.as_str()) } else { None }).collect();
    let mut permissions = HashMap::new();
    for (ix, entry) in entries.iter().enumerate() {
        if let Entry::Permission { tool_call_id: Some(tid), .. } = entry {
            permissions.entry(tid.clone()).or_insert(ix);
        }
    }
    let starts: Vec<usize> = entries.iter().enumerate().filter(|(_, e)| matches!(e, Entry::User(..))).map(|(ix, _)| ix).collect();
    let mut rows: Vec<Row> = (0..starts.first().copied().unwrap_or(entries.len())).map(Row::Preamble).collect();
    for (n, &start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(entries.len());
        let last = n + 1 == starts.len();
        let mut parts: Vec<Row> = Vec::new();
        for ix in start + 1..end {
            match &entries[ix] {
                Entry::Tool { .. } => parts.push(Row::Tool { ix, first: !matches!(parts.last(), Some(Row::Tool { .. })), last: false }),
                // Shown in its tool call's row.
                Entry::Permission { tool_call_id: Some(tid), .. } if tool_ids.contains(tid.as_str()) => {}
                Entry::Thought(_) => parts.push(Row::Thought { ix, latest: last && ix + 1 == end }),
                _ => parts.push(Row::Entry(ix)),
            }
        }
        // A run of tool calls ends where something else starts, or with the turn.
        for i in 0..parts.len() {
            let next_is_tool = matches!(parts.get(i + 1), Some(Row::Tool { .. }));
            if let Row::Tool { last: run_ends, .. } = &mut parts[i] {
                *run_ends = !next_is_tool;
            }
        }
        rows.push(Row::User(start));
        if !parts.is_empty() || (last && busy) {
            rows.push(Row::Agent { user: start, last });
        }
        rows.extend(parts);
    }
    (rows, permissions)
}

impl ThreadView {
    /// One row of the conversation, in the centred column. The list only asks for the
    /// rows on screen, so a long conversation costs no more to repaint than a short one.
    pub(super) fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let count = self.rows.len();
        let Some(row) = self.rows.get(ix).cloned() else { return div().into_any_element() };
        let element = match &row {
            Row::Preamble(entry) | Row::User(entry) => {
                let md = self.user_markdown(*entry, cx);
                let thread = self.thread.read(cx);
                render_entry(thread, &self.thread.downgrade(), *entry, &thread.entries[*entry], md.as_ref(), window, cx)
            }
            Row::Agent { user, last } => self.render_agent_header(*user, *last, cx),
            Row::Tool { ix, first, last } => self.render_tool(*ix, *first, *last, cx),
            Row::Thought { ix, latest } => {
                let Some(Entry::Thought(md)) = self.thread.read(cx).entries.get(*ix) else { return div().into_any_element() };
                let live = *latest && self.thread.read(cx).status() == Status::Busy;
                self.render_thought(*ix, md.clone(), live, window, cx)
            }
            Row::Entry(entry) => {
                let thread = self.thread.read(cx);
                render_entry(thread, &self.thread.downgrade(), *entry, &thread.entries[*entry], None, window, cx)
            }
        };
        // Room between turns, so each message and its answer read as one; the agent's part
        // sits a little in from the message.
        let in_agent_part = matches!(row, Row::Tool { .. } | Row::Thought { .. } | Row::Entry(_));
        h_flex()
            .w_full()
            .justify_center()
            .px_4()
            .child(
                // `min_w_0`: in a row, a column otherwise grows to its widest line instead
                // of wrapping it.
                v_flex()
                    .w_full()
                    .min_w_0()
                    .max_w(px(880.))
                    .map(|el| match &row {
                        _ if ix == 0 => el.pt_6(),
                        Row::Preamble(_) | Row::User(_) => el.pt_8(),
                        Row::Agent { .. } => el.pt_3(),
                        Row::Tool { first: false, .. } => el,
                        _ => el.pt_2(),
                    })
                    .when(in_agent_part, |el| el.pl_1())
                    .when(ix + 1 == count, |el| el.pb_6())
                    .child(element),
            )
            .into_any_element()
    }

    /// The agent's name, with what it is doing (and for how long) while it works, or how
    /// long it worked.
    fn render_agent_header(&mut self, user_ix: usize, last: bool, cx: &mut Context<Self>) -> AnyElement {
        let thread = self.thread.read(cx);
        let working = last && thread.status() == Status::Busy;
        // Not a clone of the checkpoint: it holds whole files.
        let timing = thread.turn_at(user_ix).map(|c| (c.started, c.duration));
        let state: AnyElement = if working {
            let (activity, waiting) = thread.activity().unwrap_or(("Working".into(), false));
            let elapsed = timing.map(|(started, _)| duration(started.elapsed())).unwrap_or_default();
            h_flex()
                .gap_1()
                .min_w_0()
                .child(if waiting { Icon::new(IconName::Warning).size(IconSize::XSmall).color(Color::Warning).into_any_element() } else { spinner(("turn-spinner", user_ix), IconSize::XSmall, Color::Accent) })
                .child(Label::new(activity).size(LabelSize::Small).color(if waiting { Color::Warning } else { Color::Accent }).truncate())
                .child(Label::new(elapsed).size(LabelSize::Small).color(Color::Muted))
                .into_any_element()
        } else {
            match timing.and_then(|(_, d)| d) {
                Some(d) => Label::new(format!("Worked for {}", duration(d))).size(LabelSize::Small).color(Color::Muted).into_any_element(),
                None => div().into_any_element(),
            }
        };
        h_flex()
            .gap_2()
            .pl_1()
            .min_w_0()
            .child(Icon::from_path("icons/forge_agents.svg").size(IconSize::Small).color(Color::Accent))
            .child(Label::new(thread.agent_label()).size(LabelSize::Small).color(Color::Default))
            .child(div().flex_1().min_w_0().child(state))
            .into_any_element()
    }

    /// A thought: its first line, unfolding on click; the latest one stays open while the
    /// agent is still thinking.
    fn render_thought(&mut self, ix: usize, md: gpui::Entity<markdown::Markdown>, live: bool, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let key = format!("thought-{ix}");
        let open = live != self.toggled.contains(&key);
        let source = md.read(cx).source();
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

    /// A tool call: a line, unfolding to its output, diffs and question. Consecutive ones
    /// read as one list in a box (`first` and `last` close it).
    fn render_tool(&mut self, ix: usize, first: bool, last: bool, cx: &mut Context<Self>) -> AnyElement {
        #[cfg(test)]
        {
            self.rendered_tools += 1;
        }
        let follow_terminal = self.follow_terminal(cx);
        let thread = self.thread.read(cx);
        let handle = self.thread.downgrade();
        let colors = cx.theme().colors().clone();
        let Some(Entry::Tool { id, title, kind, status, detail, terminal, diffs, command }) = thread.entries.get(ix) else { return div().into_any_element() };
        let permission = self.tool_permissions.get(id).and_then(|&p| match thread.entries.get(p) {
            Some(Entry::Permission { request_id, options, resolved, diffs, .. }) => Some((request_id.clone(), options, resolved.clone(), diffs)),
            _ => None,
        });
        let waiting = permission.as_ref().is_some_and(|(_, _, resolved, _)| resolved.is_none());
        let running = is_running(status);
        let failed = status == "failed";
        let terminal_view = terminal.as_ref().and_then(|t| thread.terminal_view(t));
        let in_follow = follow_terminal.as_deref() == Some(id.as_str());
        let permission_diffs: Vec<&DiffView> = match &permission {
            Some((_, _, _, pdiffs)) if diffs.is_empty() => pdiffs.iter().collect(),
            _ => Vec::new(),
        };
        let has_body = detail.is_some() || terminal_view.is_some() || !diffs.is_empty() || !permission_diffs.is_empty() || permission.is_some();
        let key = format!("tool-{id}");
        // Open by default when it needs you or is worth watching.
        let default_open = waiting || failed || (running && terminal_view.is_some());
        let open = has_body && (default_open != self.toggled.contains(&key));
        let (added, removed) = diffs.iter().chain(permission_diffs.iter().copied()).map(|d| d.stats).fold((0, 0), |(a, r), (x, y)| (a + x, r + y));
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
        let line = h_flex()
            .id(SharedString::from(key))
            .gap_2()
            .px_2()
            .py_1()
            .min_w_0()
            .rounded_sm()
            .when(has_body, |el| el.cursor_pointer().hover(|el| el.bg(colors.element_hover)))
            .child(div().w_3().flex_none().when(has_body, |el| el.child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).size(IconSize::XSmall).color(Color::Muted))))
            .child(div().flex_none().child(status_icon))
            .child(div().min_w_0().max_w(gpui::relative(0.6)).child(Label::new(title.clone()).size(LabelSize::Small).color(if running || waiting { Color::Default } else { Color::Muted }).truncate()))
            // The command line next to the agent's description of it.
            .child(div().flex_1().min_w_0().when_some(command.clone(), |el, c| el.child(Label::new(c).size(LabelSize::Small).color(Color::Muted).buffer_font(cx).truncate())))
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
        // The box around the run: its top edge on the first, a line between the others.
        v_flex()
            .border_x_1()
            .border_t_1()
            .border_color(colors.border_variant)
            .bg(colors.surface_background)
            .when(first, |el| el.rounded_t_md().pt_0p5())
            .when(last, |el| el.border_b_1().rounded_b_md().pb_0p5())
            .child(line)
            .children(body)
            .into_any_element()
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

    fn tool(id: &str) -> Entry {
        Entry::Tool { id: id.into(), title: id.into(), kind: "read".into(), status: "completed".into(), detail: None, terminal: None, diffs: vec![], command: None }
    }

    fn permission(tool: &str) -> Entry {
        Entry::Permission { request_id: format!("r-{tool}"), tool_call_id: Some(tool.into()), title: String::new(), options: vec![], resolved: None, diffs: vec![] }
    }

    #[test]
    fn a_row_for_each_message_tool_call_and_answer() {
        let entries = vec![
            Entry::System("connected".into(), Color::Muted),
            Entry::User("hi".into(), vec![]),
            tool("a"),
            permission("a"),
            tool("b"),
            Entry::System("an answer".into(), Color::Muted),
            tool("c"),
            Entry::User("again".into(), vec![]),
        ];
        let (rows, permissions) = rows(&entries, false);
        assert_eq!(
            rows,
            vec![
                Row::Preamble(0),
                Row::User(1),
                Row::Agent { user: 1, last: false },
                // The permission request shows in its tool call's row, not in the run.
                Row::Tool { ix: 2, first: true, last: false },
                Row::Tool { ix: 4, first: false, last: true },
                Row::Entry(5),
                Row::Tool { ix: 6, first: true, last: true },
                // Nothing from the agent yet, and it isn't working: no header.
                Row::User(7),
            ]
        );
        assert_eq!(permissions.get("a"), Some(&3));
        // Working on the latest turn: its header shows, with what it is doing.
        assert_eq!(super::rows(&entries, true).0.last(), Some(&Row::Agent { user: 7, last: true }));
    }
}
