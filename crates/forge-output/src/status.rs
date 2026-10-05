//! Status-bar item for the language servers: a spinner and what the busy one is doing
//! ("OmniSharp: Loading projects… 40%"), or a check and the servers' names once idle.
//! Clicking it opens that server's log in the Output panel.

use gpui::{Context, Entity, IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, WeakEntity, Window};
use language::LanguageServerId;
use project::Project;
use ui::{ButtonCommon as _, ButtonLike, ButtonStyle, Clickable as _, Color, CommonAnimationExt as _, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip, h_flex};
use workspace::{ItemHandle, StatusItemView, Workspace};

use crate::panel::{OutputPanel, Source};

pub struct LanguageServerStatus {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    _subscription: Subscription,
}

impl LanguageServerStatus {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let subscription = cx.observe(&project.read(cx).lsp_store(), |_, _, cx| cx.notify());
        Self { project, workspace: workspace.weak_handle(), _subscription: subscription }
    }

    /// The busy server and a description of its work, if any; otherwise every server.
    fn summary(&self, cx: &gpui::App) -> Option<(Option<String>, Vec<(LanguageServerId, SharedString)>)> {
        let project = self.project.read(cx);
        let mut servers = Vec::new();
        let mut busy = None;
        for (id, status) in project.language_server_statuses(cx) {
            let name = SharedString::from(status.name.to_string());
            if busy.is_none()
                && let Some(progress) = status.pending_work.values().next()
            {
                let mut text = format!("{name}:");
                for part in [&progress.title, &progress.message].into_iter().flatten() {
                    text.push(' ');
                    text.push_str(part);
                }
                if let Some(percentage) = progress.percentage {
                    text.push_str(&format!(" {percentage}%"));
                }
                busy = Some((text, id, name.clone()));
            }
            servers.push((id, name));
        }
        if servers.is_empty() {
            return None;
        }
        match busy {
            Some((text, id, name)) => Some((Some(text), vec![(id, name)])),
            None => Some((None, servers)),
        }
    }
}

impl Render for LanguageServerStatus {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((busy, servers)) = self.summary(cx) else {
            return h_flex().into_any_element();
        };
        let first = servers.first().map(|(id, _)| *id);
        let (icon, label) = match &busy {
            Some(text) => (Icon::new(IconName::ArrowCircle).size(IconSize::XSmall).color(Color::Accent).with_rotate_animation(2).into_any_element(), text.clone()),
            None => {
                let names: Vec<_> = servers.iter().map(|(_, name)| name.to_string()).collect();
                let label = match names.len() {
                    1..=2 => names.join(", "),
                    n => format!("{}, {} +{}", names[0], names[1], n - 2),
                };
                (Icon::new(IconName::Check).size(IconSize::XSmall).color(Color::Success).into_any_element(), label)
            }
        };
        let workspace = self.workspace.clone();
        ButtonLike::new("forge-lsp-status")
            .style(ButtonStyle::Subtle)
            .child(h_flex().gap_1().child(icon).child(Label::new(truncate(&label, 60)).size(LabelSize::XSmall).color(Color::Muted)))
            .tooltip(Tooltip::text("Language servers — click for their log"))
            .on_click(move |_, window, cx| {
                let Some(workspace) = workspace.upgrade() else { return };
                workspace.update(cx, |workspace, cx| {
                    if let Some(panel) = workspace.focus_panel::<OutputPanel>(window, cx)
                        && let Some(id) = first
                    {
                        panel.update(cx, |panel, cx| panel.show(Source::Server(id), window, cx));
                    }
                });
            })
            .into_any_element()
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max - 1).collect::<String>())
    }
}

impl StatusItemView for LanguageServerStatus {
    fn set_active_pane_item(&mut self, _: Option<&dyn ItemHandle>, _: &mut Window, _: &mut Context<Self>) {}
    /// Always shown: no "Hide Button" entry.
    fn hide_setting(&self, _: &gpui::App) -> Option<workspace::HideStatusItem> {
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn truncates_long_progress() {
        assert_eq!(super::truncate("short", 10), "short");
        assert_eq!(super::truncate("a long message", 6), "a lon…");
    }
}
