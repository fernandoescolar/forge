//! Forge's welcome page, shown in windows without a project (replacing Zed's).

use crate::menus;
use gpui::TaskExt as _;
use gpui::{
    Action, App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, WeakEntity, Window, img, px,
};
use std::path::{Path, PathBuf};
use theme::ActiveTheme as _;
use ui::{Color, Divider, Headline, Icon, IconName, IconSize, KeyBinding, Label, LabelCommon as _, LabelSize, WithScrollbar as _, div, h_flex, v_flex};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

const MAX_RECENT: usize = 8;

pub struct ForgeWelcome {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    /// Recently opened local projects (`None` while loading).
    recent: Option<Vec<Recent>>,
    scroll: ScrollHandle,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Recent {
    pub paths: Vec<PathBuf>,
    pub opened_at: i64,
}

impl Recent {
    /// "forge-ide" or "api, web" for multi-folder projects.
    pub fn name(&self) -> String {
        let names: Vec<_> = self.paths.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
        if names.is_empty() { "Untitled".into() } else { names.join(", ") }
    }

    /// Parent folder with the home directory shown as `~`.
    pub fn location(&self, home: Option<&Path>) -> String {
        let Some(first) = self.paths.first() else { return String::new() };
        let parent = first.parent().unwrap_or(first);
        match home.and_then(|h| parent.strip_prefix(h).ok()) {
            Some(rel) if rel.as_os_str().is_empty() => "~".into(),
            Some(rel) => format!("~/{}", rel.display()),
            None => parent.display().to_string(),
        }
    }
}

/// "just now", "5 min ago", "yesterday", "3 days ago", …
pub fn relative_time(then: i64, now: i64) -> String {
    let d = (now - then).max(0);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", d / 60),
        3600..86400 => format!("{} h ago", d / 3600),
        86400..172800 => "yesterday".into(),
        172800..2_592_000 => format!("{} days ago", d / 86400),
        _ => format!("{} months ago", d / 2_592_000),
    }
}

impl ForgeWelcome {
    fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let recent = load_recent(workspace, cx);
        cx.spawn_in(window, async move |this, cx| {
            let recent: Vec<Recent> = recent.await.into_iter().take(MAX_RECENT).collect();
            this.update(cx, |this, cx| {
                this.recent = Some(recent);
                cx.notify();
            })
            .ok();
        })
        .detach();
        Self { focus_handle: cx.focus_handle(), workspace: workspace.weak_handle(), recent: None, scroll: ScrollHandle::new() }
    }

    /// Opening a project looks at every item of the workspace (for unsaved changes), this
    /// page among them, which is busy handling the click: open right after it.
    fn open_recent(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |ws, cx| ws.open_workspace_for_paths(workspace::OpenMode::Activate, paths, window, cx).detach_and_log_err(cx))
                .ok();
        });
    }

    fn render_recent(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let recent = self.recent.as_ref().filter(|r| !r.is_empty())?;
        let colors = cx.theme().colors().clone();
        let home = std::env::home_dir();
        let now = chrono::Utc::now().timestamp();
        let mut list = v_flex().gap_0p5().child(Label::new("RECENT").size(LabelSize::XSmall).color(Color::Muted)).child(Divider::horizontal());
        for (i, r) in recent.iter().enumerate() {
            let paths = r.paths.clone();
            let tooltip = r.paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
            list = list.child(
                h_flex()
                    .id(SharedString::from(format!("welcome-recent-{i}")))
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(colors.ghost_element_hover))
                    .tooltip(ui::Tooltip::text(tooltip))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_recent(paths.clone(), window, cx)))
                    .child(
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .child(Icon::new(IconName::Folder).size(IconSize::Small).color(Color::Muted))
                            .child(Label::new(r.name()))
                            .child(Label::new(r.location(home.as_deref())).size(LabelSize::Small).color(Color::Muted).truncate()),
                    )
                    .child(Label::new(relative_time(r.opened_at, now)).size(LabelSize::XSmall).color(Color::Muted)),
            );
        }
        Some(list.into_any_element())
    }
}

/// Recently opened local projects, newest first.
fn load_recent(workspace: &Workspace, cx: &App) -> gpui::Task<Vec<Recent>> {
    let fs = workspace.app_state().fs.clone();
    let db = workspace::WorkspaceDb::global(cx);
    cx.background_spawn(async move {
        let workspaces = db.recent_project_workspaces(fs.as_ref()).await.unwrap_or_default();
        workspaces
            .into_iter()
            .filter(|w| matches!(w.location, workspace::SerializedWorkspaceLocation::Local) && !w.paths.paths().is_empty())
            .map(|w| Recent { paths: w.paths.paths().to_vec(), opened_at: w.timestamp.timestamp() })
            .collect()
    })
}

/// File › Open Recent (Zed's `projects::OpenRecent`, so its key bindings work): picks a
/// recent project other than this one and opens it here, or in a new window.
pub fn open_recent(workspace: &mut Workspace, new_window: bool, window: &mut Window, cx: &mut Context<Workspace>) {
    let current: Vec<PathBuf> = workspace.visible_worktrees(cx).map(|t| t.read(cx).abs_path().to_path_buf()).collect();
    let recent = load_recent(workspace, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let recent: Vec<Recent> = recent.await.into_iter().filter(|r| r.paths != current).collect();
        workspace.update_in(cx, |ws, window, cx| {
            if recent.is_empty() {
                let id = workspace::notifications::NotificationId::named("forge-no-recent".into());
                ws.show_toast(workspace::Toast::new(id, "No recent projects yet."), cx);
                return;
            }
            let home = std::env::home_dir();
            let now = chrono::Utc::now().timestamp();
            let choices = recent.iter().map(|r| forge_ui::pick::Choice::new(r.name()).detail(format!("{} · {}", r.location(home.as_deref()), relative_time(r.opened_at, now)))).collect();
            let weak = cx.entity().downgrade();
            forge_ui::pick::pick(ws, "Open a recent project…", choices, window, cx, move |ix, window, cx| {
                let mode = if new_window { workspace::OpenMode::NewWindow } else { workspace::OpenMode::Activate };
                let paths = recent[ix].paths.clone();
                weak.update(cx, |ws, cx| ws.open_workspace_for_paths(mode, paths, window, cx).detach_and_log_err(cx)).ok();
            });
        })
    })
    .detach_and_log_err(cx);
}

/// Opens the welcome page (or focuses it if it is already open).
pub fn show(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace.active_pane().read(cx).items_of_type::<ForgeWelcome>().next();
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let page = cx.new(|cx| ForgeWelcome::new(workspace, window, cx));
    workspace.add_item_to_active_pane(Box::new(page), None, true, window, cx);
}

struct Entry {
    icon: IconName,
    label: &'static str,
    action: Box<dyn Action>,
}

fn entry(icon: IconName, label: &'static str, action: impl Action) -> Entry {
    Entry { icon, label, action: Box::new(action) }
}

fn sections() -> Vec<(&'static str, Vec<Entry>)> {
    vec![
        (
            "Start",
            vec![
                entry(IconName::FolderOpen, "Open Folder…", workspace::Open::default()),
                entry(IconName::File, "New File", workspace::NewFile),
                entry(IconName::Terminal, "New Terminal", workspace::NewTerminal::default()),
                entry(IconName::MagnifyingGlass, "Command Palette", zed_actions::command_palette::Toggle),
            ],
        ),
        (
            "Agents & extensions",
            vec![
                entry(IconName::ZedAgent, "Threads", forge_agents::OpenThreads),
                entry(IconName::Settings, "Agent Settings…", forge_agents::OpenAgentSettings),
                entry(IconName::Blocks, "Extensions", forge_extension_host::panel::ToggleFocus),
            ],
        ),
        (
            "Customize",
            vec![
                entry(IconName::Sparkle, "Select Theme…", zed_actions::theme_selector::Toggle::default()),
                entry(IconName::Notepad, "Colour Palettes…", menus::OpenPalettesFolder),
                entry(IconName::Settings, "Settings", crate::settings_view::OpenSettings),
                entry(IconName::Keyboard, "Key Bindings", menus::OpenKeymapFile),
            ],
        ),
        ("Learn", vec![entry(IconName::Book, "Forge Guide", menus::OpenGuide)]),
    ]
}

impl Render for ForgeWelcome {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let focus = self.focus_handle.clone();
        let mut column = v_flex().max_w(px(520.)).w_full().gap_6().p_8().child(
            h_flex()
                .gap_4()
                .child(img("images/forge_logo.svg").size(px(64.)))
                .child(
                    v_flex()
                        .child(Headline::new("Welcome to Forge"))
                        .child(Label::new("A native editor for working with agents").size(LabelSize::Small).color(Color::Muted).italic()),
                ),
        );
        if let Some(recent) = self.render_recent(cx) {
            column = column.child(recent);
        }
        for (si, (title, entries)) in sections().into_iter().enumerate() {
            let mut list = v_flex().gap_0p5().child(Label::new(title.to_uppercase()).size(LabelSize::XSmall).color(Color::Muted)).child(Divider::horizontal());
            for (ei, e) in entries.into_iter().enumerate() {
                let action = e.action.boxed_clone();
                let keys = KeyBinding::for_action_in(&*e.action, &focus, cx);
                list = list.child(
                    h_flex()
                        .id(SharedString::from(format!("welcome-{si}-{ei}")))
                        .justify_between()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|s| s.bg(colors.ghost_element_hover))
                        .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
                        .child(h_flex().gap_2().child(Icon::new(e.icon).size(IconSize::Small).color(Color::Muted)).child(Label::new(e.label)))
                        .child(keys),
                );
            }
            column = column.child(list);
        }
        // The whole page scrolls (scrollbar at its edge); the column stays centred while it fits.
        v_flex()
            .key_context("ForgeWelcome")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.editor_background)
            .child(
                div()
                    .id("forge-welcome")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(h_flex().w_full().min_h_full().justify_center().items_center().child(column)),
            )
            .vertical_scrollbar_for(&self.scroll, window, cx)
    }
}

impl Focusable for ForgeWelcome {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for ForgeWelcome {}

impl Item for ForgeWelcome {
    type Event = ItemEvent;

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        "Welcome".into()
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Sparkle))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    /// Forge as a window starts it: Forge's panels, docks and title bar on every workspace.
    pub(crate) fn init_forge(cx: &mut TestAppContext) -> std::sync::Arc<workspace::AppState> {
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            language_model::init(cx);
            workspace::init(params.clone(), cx);
            project_panel::init(cx);
            terminal_view::init(cx);
            forge_agents::init(cx);
            forge_extension_host::panel::init(cx);
            forge_extension_host::install::init(cx);
            forge_git::init(cx);
            forge_dotnet::init(cx);
            crate::menus::init(cx);
            crate::open_editors::init(cx);
            crate::docks::init(cx);
            forge_tests::panel::init(cx);
            forge_run::init(cx);
            forge_output::init(cx);
            debugger_ui::init(cx);
            crate::titlebar::init(cx);
            crate::add_panels_to_new_workspaces(cx);
            workspace::AppState::set_global(params.clone(), cx);
        });
        params
    }

    /// A new window's welcome page opens a recent project in that window.
    #[gpui::test]
    async fn opens_a_recent_project_from_a_new_window(cx: &mut TestAppContext) {
        let params = init_forge(cx);
        params.fs.as_fake().insert_tree("/proj", serde_json::json!({ "a.txt": "hi" })).await;
        let project = project::Project::test(params.fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        workspace.update_in(cx, |ws, window, cx| show(ws, window, cx));
        cx.run_until_parked();
        let page = workspace.read_with(cx, |ws, cx| ws.active_pane().read(cx).items_of_type::<ForgeWelcome>().next()).unwrap();
        page.update_in(cx, |page, window, cx| page.open_recent(vec!["/proj".into()], window, cx));
        cx.run_until_parked();
        let shown = window.read_with(cx, |mw, cx| mw.workspace().read(cx).visible_worktrees(cx).map(|t| t.read(cx).abs_path().to_path_buf()).collect::<Vec<_>>()).unwrap();
        assert_eq!(shown, [PathBuf::from("/proj")], "the window shows the project");

    }

    #[test]
    fn recent_entries_are_formatted_for_people() {
        let home = Path::new("/Users/me");
        let one = Recent { paths: vec!["/Users/me/projects/forge-ide".into()], opened_at: 0 };
        assert_eq!(one.name(), "forge-ide");
        assert_eq!(one.location(Some(home)), "~/projects");
        let multi = Recent { paths: vec!["/srv/api".into(), "/srv/web".into()], opened_at: 0 };
        assert_eq!(multi.name(), "api, web");
        assert_eq!(multi.location(Some(home)), "/srv");
        assert_eq!(Recent { paths: vec!["/Users/me/x".into()], opened_at: 0 }.location(Some(home)), "~");
        assert_eq!(relative_time(1000, 1030), "just now");
        assert_eq!(relative_time(0, 600), "10 min ago");
        assert_eq!(relative_time(0, 100_000), "yesterday");
        assert_eq!(relative_time(0, 5 * 86400), "5 days ago");
    }
}
