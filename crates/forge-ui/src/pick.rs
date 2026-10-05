//! Small modal pickers: choose one of several items (templates, projects, folders,
//! versions) or type a value (a name), with the result handed to a callback.

use std::sync::Arc;

use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{App, Context, DismissEvent, SharedString, Task, WeakEntity, Window};
use picker::{Picker, PickerDelegate};
use ui::{HighlightedLabel, Label, LabelCommon as _, LabelSize, ListItem, ListItemSpacing, Toggleable as _, prelude::*};
use workspace::Workspace;

#[derive(Clone, Debug)]
pub struct Choice {
    pub label: SharedString,
    pub detail: Option<SharedString>,
}

impl Choice {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self { label: label.into(), detail: None }
    }

    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

type OnPick = Box<dyn FnOnce(usize, &mut Window, &mut App)>;
type OnInput = Box<dyn FnOnce(String, &mut Window, &mut App)>;

pub struct ChoiceDelegate {
    placeholder: Arc<str>,
    choices: Vec<Choice>,
    matches: Vec<StringMatch>,
    selected: usize,
    query: String,
    on_pick: Option<OnPick>,
    on_input: Option<OnInput>,
}

impl ChoiceDelegate {
    fn all_matches(&self) -> Vec<StringMatch> {
        (0..self.choices.len()).map(|i| StringMatch { candidate_id: i, score: 0., positions: Vec::new(), string: self.choices[i].label.to_string() }).collect()
    }
}

impl PickerDelegate for ChoiceDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "ForgeChoicePicker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = ix;
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        self.placeholder.clone()
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        if self.on_input.is_some() { None } else { Some("No matches".into()) }
    }

    fn update_matches(&mut self, query: String, window: &mut Window, cx: &mut Context<Picker<Self>>) -> Task<()> {
        self.query = query.clone();
        if query.is_empty() {
            self.matches = self.all_matches();
            self.selected = 0;
            return Task::ready(());
        }
        let candidates: Vec<StringMatchCandidate> = self
            .choices
            .iter()
            .enumerate()
            .map(|(i, c)| StringMatchCandidate::new(i, &format!("{} {}", c.label, c.detail.as_ref().map(|d| d.as_ref()).unwrap_or(""))))
            .collect();
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = fuzzy::match_strings(&candidates, &query, false, true, 200, &Default::default(), executor).await;
            picker
                .update(cx, |picker, cx| {
                    let label_len = |m: &StringMatch| picker.delegate.choices[m.candidate_id].label.len();
                    picker.delegate.matches = matches
                        .into_iter()
                        .map(|mut m| {
                            // Only highlight within the label.
                            let len = label_len(&m);
                            m.positions.retain(|&p| p < len);
                            m
                        })
                        .collect();
                    picker.delegate.selected = 0;
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(on_input) = self.on_input.take() {
            let value = self.query.trim().to_string();
            if value.is_empty() {
                self.on_input = Some(on_input);
                return;
            }
            on_input(value, window, cx);
            cx.emit(DismissEvent);
            return;
        }
        let Some(m) = self.matches.get(self.selected) else { return };
        if let Some(on_pick) = self.on_pick.take() {
            on_pick(m.candidate_id, window, cx);
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(&self, ix: usize, selected: bool, _: &mut Window, _: &mut Context<Picker<Self>>) -> Option<Self::ListItem> {
        let m = self.matches.get(ix)?;
        let choice = &self.choices[m.candidate_id];
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .justify_between()
                        .child(HighlightedLabel::new(choice.label.clone(), m.positions.clone()))
                        .children(choice.detail.clone().map(|d| Label::new(d).size(LabelSize::Small).color(Color::Muted).truncate())),
                ),
        )
    }
}

/// Runs `f` on the workspace after the current update: opening a modal or focusing a
/// panel reads the workspace and the entity that asked, so it cannot happen inside them.
pub fn defer_workspace(
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
    f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    window.defer(cx, move |window, cx| {
        workspace.update(cx, |workspace, cx| f(workspace, window, cx)).ok();
    });
}

/// Shows `choices` and calls `on_pick` with the index of the chosen one.
pub fn pick(
    workspace: &mut Workspace,
    placeholder: &str,
    choices: Vec<Choice>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    on_pick: impl FnOnce(usize, &mut Window, &mut App) + 'static,
) {
    let mut delegate = ChoiceDelegate {
        placeholder: placeholder.into(),
        choices,
        matches: Vec::new(),
        selected: 0,
        query: String::new(),
        on_pick: Some(Box::new(on_pick)),
        on_input: None,
    };
    delegate.matches = delegate.all_matches();
    workspace.toggle_modal(window, cx, |window, cx| Picker::uniform_list(delegate, window, cx).initial_width(rems(34.)));
}

/// Asks for a line of text, prefilled with `initial`.
pub fn ask(
    workspace: &mut Workspace,
    placeholder: &str,
    initial: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    on_input: impl FnOnce(String, &mut Window, &mut App) + 'static,
) {
    let delegate = ChoiceDelegate {
        placeholder: placeholder.into(),
        choices: Vec::new(),
        matches: Vec::new(),
        selected: 0,
        query: initial.to_string(),
        on_pick: None,
        on_input: Some(Box::new(on_input)),
    };
    let initial = initial.to_string();
    workspace.toggle_modal(window, cx, |window, cx| {
        let picker = Picker::uniform_list(delegate, window, cx).initial_width(rems(34.));
        picker.set_query(&initial, window, cx);
        picker
    });
}
