//! The thread picker (`cmd-shift-a`): jump to any open thread or saved conversation, or
//! start a new one, by typing part of its title or agent.

use std::path::PathBuf;
use std::sync::Arc;

use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, ParentElement as _, Render, Styled as _, WeakEntity, Window};
use picker::{Picker, PickerDelegate};
use ui::{Color, HighlightedLabel, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, ListItem, ListItemSpacing, Toggleable as _, h_flex, rems, v_flex};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::{
    history::{self, SessionSummary},
    settings,
    thread::Thread,
    threads::{self, OpenThreads},
};

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenThreads, window, cx| toggle(workspace, window, cx));
    })
    .detach();
}

pub fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let weak = workspace.weak_handle();
    let choices = choices(workspace, cx);
    workspace.toggle_modal(window, cx, move |window, cx| ThreadPicker::new(weak, choices, window, cx));
}

#[derive(Clone)]
pub enum Choice {
    New { agent: String },
    Open { thread: Entity<Thread>, title: String, detail: String },
    Saved { summary: SessionSummary },
}

impl Choice {
    fn search_text(&self) -> String {
        match self {
            Choice::New { agent } => format!("New thread {agent}"),
            Choice::Open { title, detail, .. } => format!("{title} {detail}"),
            Choice::Saved { summary } => format!("{} {}", summary.title, summary.agent_id),
        }
    }

    fn label(&self) -> String {
        match self {
            Choice::New { agent } => format!("New thread with {agent}"),
            Choice::Open { title, .. } => title.clone(),
            Choice::Saved { summary } => summary.title.clone(),
        }
    }
}

/// What the picker offers, in order: a new thread, open threads, saved conversations.
pub fn choices(workspace: &Workspace, cx: &App) -> Vec<Choice> {
    let settings = settings::global(cx);
    let settings = settings.read(cx);
    let mut out = Vec::new();
    if let Some(agent) = settings.default_agent() {
        out.push(Choice::New { agent: agent.id.clone() });
    }
    let open: Vec<Entity<Thread>> = threads::store_for(workspace.weak_handle().entity_id(), cx).map(|s| s.read(cx).threads().to_vec()).unwrap_or_default();
    let open_sessions: Vec<String> = open.iter().filter_map(|t| t.read(cx).session_id().map(str::to_string)).collect();
    for thread in open {
        let t = thread.read(cx);
        let title = t.title().unwrap_or_else(|| "New thread".into());
        let detail = format!("{} · {}", t.summary().0, t.agent_label());
        out.push(Choice::Open { thread: thread.clone(), title, detail });
    }
    let root: Option<PathBuf> = workspace.project().read(cx).visible_worktrees(cx).next().map(|t| t.read(cx).abs_path().to_path_buf()).or_else(|| std::env::current_dir().ok());
    if let Some(root) = root {
        for summary in history::list(settings.history_dir(), &root).into_iter().filter(|s| !open_sessions.contains(&s.session_id)).take(50) {
            out.push(Choice::Saved { summary });
        }
    }
    out
}

pub struct ThreadPicker {
    picker: Entity<Picker<ThreadPickerDelegate>>,
}

impl ThreadPicker {
    fn new(workspace: WeakEntity<Workspace>, choices: Vec<Choice>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let candidates = choices.iter().enumerate().map(|(i, c)| StringMatchCandidate::new(i, &c.search_text())).collect();
        // Open threads first when nothing is typed: the new-thread entry is one keystroke away.
        let selected_index = choices.iter().position(|c| matches!(c, Choice::Open { .. })).unwrap_or(0);
        let delegate = ThreadPickerDelegate { picker: cx.entity().downgrade(), workspace, choices, candidates, matches: Vec::new(), selected_index };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }

    #[cfg(test)]
    pub(crate) fn set_query(&self, query: &str, window: &mut Window, cx: &mut App) {
        self.picker.update(cx, |p, cx| p.set_query(query, window, cx));
    }

    #[cfg(test)]
    pub(crate) fn picker(&self) -> Entity<Picker<ThreadPickerDelegate>> {
        self.picker.clone()
    }

    #[cfg(test)]
    pub(crate) fn delegate_labels(&self, cx: &App) -> Vec<String> {
        let delegate = &self.picker.read(cx).delegate;
        delegate.matches.iter().map(|m| delegate.choices[m.candidate_id].label()).collect()
    }
}

impl Render for ThreadPicker {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        v_flex().key_context("ForgeThreadPicker").w(rems(36.)).child(self.picker.clone())
    }
}

impl Focusable for ThreadPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for ThreadPicker {}
impl ModalView for ThreadPicker {}

pub struct ThreadPickerDelegate {
    picker: WeakEntity<ThreadPicker>,
    workspace: WeakEntity<Workspace>,
    choices: Vec<Choice>,
    candidates: Vec<StringMatchCandidate>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl PickerDelegate for ThreadPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "ForgeThreadPicker"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Go to a thread, or start one…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn update_matches(&mut self, query: String, window: &mut Window, cx: &mut Context<Picker<Self>>) -> gpui::Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self.candidates.clone();
        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates.into_iter().enumerate().map(|(i, c)| StringMatch { candidate_id: i, string: c.string, positions: Vec::new(), score: 0.0 }).collect()
            } else {
                match_strings(&candidates, &query, false, true, 100, &Default::default(), background).await
            };
            this.update_in(cx, |this, window, cx| {
                let keep = if query.is_empty() { this.delegate.selected_index.min(matches.len().saturating_sub(1)) } else { 0 };
                this.delegate.matches = matches;
                this.set_selected_index(keep, None, false, window, cx);
                cx.notify();
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(choice) = self.matches.get(self.selected_index).and_then(|m| self.choices.get(m.candidate_id)).cloned() else { return };
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |ws, cx| match choice {
                Choice::New { .. } => {
                    threads::new_thread(ws, None, window, cx);
                }
                Choice::Open { thread, .. } => threads::open_view(ws, thread, window, cx),
                Choice::Saved { summary } => {
                    let thread = cx.new(|cx| Thread::new(ws, None, window, cx));
                    thread.update(cx, |t, cx| t.open_session(&summary, cx));
                    threads::add_thread(ws, thread, window, cx);
                }
            });
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker.update(cx, |_, cx| cx.emit(DismissEvent)).log_err();
    }

    fn render_match(&self, ix: usize, selected: bool, _: &mut Window, _: &mut Context<Picker<Self>>) -> Option<Self::ListItem> {
        let m = self.matches.get(ix)?;
        let choice = self.choices.get(m.candidate_id)?;
        let label = choice.label();
        // Highlight only the part of the match that falls in the label.
        let positions: Vec<usize> = m.positions.iter().copied().filter(|p| *p < label.len()).collect();
        let (icon, color, detail) = match choice {
            Choice::New { .. } => (IconName::Plus, Color::Accent, String::new()),
            Choice::Open { detail, .. } => (IconName::ZedAgent, Color::Default, detail.clone()),
            Choice::Saved { summary } => (IconName::HistoryRerun, Color::Muted, format!("Earlier · {} · {}", summary.agent_id, history::relative_time(summary.updated_at, history::now()))),
        };
        Some(
            ListItem::new(ix).inset(true).spacing(ListItemSpacing::Sparse).toggle_state(selected).start_slot(Icon::new(icon).size(IconSize::Small).color(color)).child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .gap_2()
                    .child(HighlightedLabel::new(label, positions))
                    .child(Label::new(detail).size(LabelSize::XSmall).color(Color::Muted)),
            ),
        )
    }
}
