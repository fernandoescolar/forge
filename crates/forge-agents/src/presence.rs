//! The agents, seen from anywhere in Forge: a status bar item with what the workspace's
//! threads are doing (waiting for you, working, changes to review) that jumps to them,
//! and a system notification when a thread finishes or needs you while Forge is in the
//! background (clicking it brings you to the thread).

use std::collections::HashMap;

use gpui::{
    AnyWindowHandle, App, Context, Entity, EntityId, Global, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, SystemNotification, WeakEntity, Window,
};
use ui::{ButtonCommon as _, ButtonLike, ButtonStyle, Color, ContextMenu, Icon, IconSize, Label, LabelCommon as _, LabelSize, PopoverMenu, Tooltip, h_flex};
use workspace::{ItemHandle, StatusItemView, Workspace};

use crate::thread::{Status, Thread};
use crate::threads::{ThreadStore, open_view, store_for};

pub fn init(cx: &mut App) {
    cx.on_system_notification_response(|response, cx| {
        let Some(target) = cx.try_global::<Notified>().and_then(|n| n.targets.get(&response.tag).cloned()) else { return };
        cx.activate(true);
        let (window, workspace, thread) = target;
        window
            .update(cx, |_, window, cx| {
                window.activate_window();
                if let (Some(workspace), Some(thread)) = (workspace.upgrade(), thread.upgrade()) {
                    workspace.update(cx, |ws, cx| open_view(ws, thread, window, cx));
                }
            })
            .ok();
    });
}

/// What each thread was doing at its last update, to notice when it finishes or starts
/// waiting; and where each notification leads.
#[derive(Default)]
struct Notified {
    last: HashMap<EntityId, (bool, usize)>,
    targets: HashMap<SharedString, (AnyWindowHandle, WeakEntity<Workspace>, WeakEntity<Thread>)>,
}
impl Global for Notified {}

/// A thread changed: notify when it just finished, or just started waiting for the user,
/// and Forge isn't the active app.
pub(crate) fn thread_updated(thread: &Entity<Thread>, window: Option<AnyWindowHandle>, workspace: WeakEntity<Workspace>, cx: &mut App) {
    let t = thread.read(cx);
    let busy = matches!(t.status(), Status::Busy | Status::Connecting);
    let waiting = t.pending_reviews();
    let title = t.title().unwrap_or_else(|| "Thread".into());
    let agent = t.agent_label();
    let id = thread.entity_id();
    let was = cx.default_global::<Notified>().last.insert(id, (busy, waiting)).unwrap_or((false, 0));
    let body = notification_body(&agent, was, (busy, waiting));
    let (Some(body), Some(window)) = (body, window) else { return };
    if cx.active_window().is_some() {
        // Forge is in front: the status bar and the thread tab show it.
        return;
    }
    let tag: SharedString = format!("forge-thread-{}", id.as_u64()).into();
    cx.default_global::<Notified>().targets.insert(tag.clone(), (window, workspace, thread.downgrade()));
    cx.show_system_notification(SystemNotification { tag, title: title.into(), body: body.into(), actions: vec![] });
    window.update(cx, |_, window, _| window.request_attention()).ok();
}

/// What to tell the user when a thread goes from `was` to `now` (busy, requests waiting).
fn notification_body(agent: &str, was: (bool, usize), now: (bool, usize)) -> Option<String> {
    let ((was_busy, was_waiting), (busy, waiting)) = (was, now);
    if waiting > was_waiting {
        Some(format!("{agent} is waiting for you: {waiting} request{} to answer.", if waiting == 1 { "" } else { "s" }))
    } else if was_busy && !busy && waiting == 0 {
        Some(format!("{agent} finished."))
    } else {
        None
    }
}

/// The status bar's line about the threads, if there is anything to say.
fn status_line(threads: &[Entity<Thread>], cx: &App) -> Option<(String, Color, &'static str)> {
    let waiting = threads.iter().filter(|t| t.read(cx).pending_reviews() > 0).count();
    let working = threads.iter().filter(|t| matches!(t.read(cx).status(), Status::Busy | Status::Connecting)).count();
    let files: usize = threads.iter().map(|t| t.read(cx).changes.len()).sum();
    if waiting > 0 {
        Some((format!("{waiting} waiting for you"), Color::Accent, "Agents waiting for an answer or a review"))
    } else if working > 0 {
        Some((format!("{working} working…"), Color::Info, "Agents at work"))
    } else if files > 0 {
        Some((format!("{files} file{} to review", if files == 1 { "" } else { "s" }), Color::Muted, "Changes by agents you haven't kept or undone"))
    } else {
        None
    }
}

/// The status bar item.
pub struct AgentStatus {
    workspace: WeakEntity<Workspace>,
    store: Option<Entity<ThreadStore>>,
    _subscription: Option<Subscription>,
}

impl AgentStatus {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let id = workspace.weak_handle().entity_id();
        // The thread store is created with the workspace; look it up once it exists.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |this, cx| this.attach(id, cx)).ok();
        });
        Self { workspace: workspace.weak_handle(), store: None, _subscription: None }
    }

    fn attach(&mut self, workspace: EntityId, cx: &mut Context<Self>) {
        if let Some(store) = store_for(workspace, cx) {
            self._subscription = Some(cx.observe(&store, |_, _, cx| cx.notify()));
            self.store = Some(store);
            cx.notify();
        }
    }
}

impl Render for AgentStatus {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(store) = &self.store else { return h_flex().into_any_element() };
        let threads: Vec<Entity<Thread>> = store.read(cx).threads().to_vec();
        let Some((text, color, tooltip)) = status_line(&threads, cx) else { return h_flex().into_any_element() };
        let workspace = self.workspace.clone();
        PopoverMenu::new("forge-agent-status")
            .trigger(
                ButtonLike::new("forge-agent-status-button")
                    .style(ButtonStyle::Subtle)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Icon::from_path("icons/forge_agents.svg").size(IconSize::XSmall).color(color))
                            .child(Label::new(text).size(LabelSize::XSmall).color(color)),
                    )
                    .tooltip(Tooltip::text(tooltip)),
            )
            .menu(move |window, cx| {
                let (threads, workspace) = (threads.clone(), workspace.clone());
                Some(ContextMenu::build(window, cx, move |mut menu, _, cx| {
                    menu = menu.header("Threads");
                    for thread in threads {
                        let t = thread.read(cx);
                        let (summary, _) = t.summary();
                        let changes = t.changes.len();
                        let title = t.title().unwrap_or_else(|| "New thread".into());
                        let label = if changes > 0 {
                            format!("{title} · {summary} · {changes} file{} changed", if changes == 1 { "" } else { "s" })
                        } else {
                            format!("{title} · {summary}")
                        };
                        let workspace = workspace.clone();
                        menu = menu.entry(label, None, move |window, cx| {
                            if let Some(workspace) = workspace.upgrade() {
                                let thread = thread.clone();
                                workspace.update(cx, |ws, cx| open_view(ws, thread, window, cx));
                            }
                        });
                    }
                    menu
                }))
            })
            .into_any_element()
    }
}

impl StatusItemView for AgentStatus {
    fn set_active_pane_item(&mut self, _: Option<&dyn ItemHandle>, _: &mut Window, _: &mut Context<Self>) {}

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn notifies_when_done_or_waiting() {
        assert_eq!(notification_body("claude", (true, 0), (false, 0)).as_deref(), Some("claude finished."));
        assert_eq!(notification_body("claude", (true, 0), (true, 1)).as_deref(), Some("claude is waiting for you: 1 request to answer."));
        assert_eq!(notification_body("claude", (true, 1), (true, 1)), None, "already told");
        assert_eq!(notification_body("claude", (true, 1), (false, 1)), None, "still waiting: not finished");
        assert_eq!(notification_body("claude", (false, 0), (false, 0)), None);
    }

    pub(crate) fn line(threads: &[Entity<Thread>], cx: &App) -> Option<String> {
        status_line(threads, cx).map(|(text, _, _)| text)
    }
}
