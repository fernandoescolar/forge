//! The Extensions panel: every extension as a card (what it is, where it comes from,
//! whether it is working, the panels and commands it adds) with its actions, plus
//! installing new ones.

use crate::{
    host::{ExtensionHost, LoadedExtension, Origin},
    panel::{ShowPanel, ToggleFocus},
};
use editor::{Editor, EditorEvent};
use gpui::{
    Action as _, AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use std::{str::FromStr as _, sync::Arc};
use theme::ActiveTheme as _;
use ui::{
    Button, ButtonCommon as _, ButtonSize, ButtonStyle, Clickable as _, Color, ContextMenu, Icon, IconButton, IconName, IconSize, Indicator, Label, LabelCommon as _, LabelSize,
    PopoverMenu, Tooltip, h_flex, v_flex,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent, PanelHandle},
};

pub struct ExtensionsPanel {
    host: Entity<ExtensionHost>,
    focus_handle: FocusHandle,
    position: DockPosition,
    search: Entity<Editor>,
    _subscriptions: Vec<Subscription>,
}

impl ExtensionsPanel {
    pub fn new(host: Entity<ExtensionHost>, workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let weak = workspace.weak_handle();
        let handle = window.window_handle();
        host.update(cx, |h, cx| h.set_workspace(weak, handle, cx));
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search extensions", window, cx);
            editor
        });
        let subscriptions = vec![
            cx.observe(&host, |_, _, cx| cx.notify()),
            cx.subscribe(&search, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::Edited { .. }) {
                    cx.notify();
                }
            }),
        ];
        Self {
            host,
            focus_handle: cx.focus_handle(),
            position: forge_ui::dock_position::load(<Self as Panel>::panel_key(), cx).unwrap_or(DockPosition::Right),
            search,
            _subscriptions: subscriptions,
        }
    }

    fn install_menu() -> impl IntoElement {
        PopoverMenu::new("forge-extensions-install")
            .trigger(Button::new("forge-extensions-install-button", "Install").style(ButtonStyle::Filled).size(ButtonSize::Compact).start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall)))
            .menu(|window, cx| {
                Some(ContextMenu::build(window, cx, |menu, _, _| {
                    menu.entry("From a Package (.forgeext)…", None, |_, cx| crate::install::install_from_package(cx))
                        .entry("From a Folder…", None, |_, cx| crate::install::install_from_folder(cx))
                        .separator()
                        .entry("Reveal Extensions Folder", None, |_, cx| {
                            let dir = paths::data_dir().join("extensions");
                            std::fs::create_dir_all(&dir).ok();
                            cx.reveal_path(&dir);
                        })
                }))
            })
    }

    fn render_empty(&self) -> AnyElement {
        v_flex()
            .p_4()
            .gap_2()
            .child(Label::new("No extensions yet").weight(FontWeight::MEDIUM))
            .child(
                Label::new("Extensions add panels, tabs and commands to Forge. They are shared as .forgeext packages: install one with Install above.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_card(&self, index: usize, extension: &LoadedExtension, cx: &App) -> AnyElement {
        let host = self.host.read(cx);
        let colors = cx.theme().colors();
        let errors = host.errors_of(&extension.id);
        let origin = host.origin(&extension.id);
        let panels: Vec<_> = host.panels.iter().filter(|p| p.extension.as_deref() == Some(extension.id.as_str())).cloned().collect();
        let commands: Vec<_> = host.commands.iter().filter(|c| c.extension.as_deref() == Some(extension.id.as_str())).cloned().collect();

        let status = if errors.is_empty() {
            h_flex().gap_1().child(Indicator::dot().color(Color::Success)).child(Label::new("Running").size(LabelSize::XSmall).color(Color::Muted))
        } else {
            h_flex().gap_1().child(Indicator::dot().color(Color::Error)).child(Label::new("Error").size(LabelSize::XSmall).color(Color::Error))
        };

        let header = h_flex()
            .gap_2()
            .w_full()
            .child(Icon::new(IconName::Blocks).color(Color::Muted))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Label::new(extension.name.clone()).weight(FontWeight::MEDIUM).truncate())
                            .children(extension.version.clone().map(|v| Label::new(v).size(LabelSize::XSmall).color(Color::Muted))),
                    )
                    .child(Label::new(origin.label()).size(LabelSize::XSmall).color(Color::Muted)),
            )
            .child(status)
            .child(self.actions_menu(index, extension, origin));

        let mut card = v_flex().id(("forge-extension-card", index)).gap_2().p_2().rounded_md().border_1().border_color(colors.border_variant).bg(colors.surface_background).child(header);
        if let Some(description) = &extension.description {
            card = card.child(Label::new(description.clone()).size(LabelSize::Small).color(Color::Muted));
        }
        if !panels.is_empty() {
            let mut row = h_flex().gap_1().flex_wrap();
            for (i, panel) in panels.iter().enumerate() {
                let icon = IconName::from_str(&panel.icon).unwrap_or(IconName::Sparkle);
                let action = ShowPanel { id: panel.id.clone() };
                row = row.child(
                    Button::new(SharedString::from(format!("forge-ext-{index}-panel-{i}")), panel.title.clone())
                        .style(ButtonStyle::Outlined)
                        .size(ButtonSize::Compact)
                        .start_icon(Icon::new(icon).size(IconSize::XSmall))
                        .tooltip(Tooltip::text("Show this panel"))
                        .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx)),
                );
            }
            card = card.child(section("Panels", row));
        }
        if !commands.is_empty() {
            let mut list = v_flex().gap_0p5();
            for (i, command) in commands.iter().enumerate() {
                let (host, id) = (self.host.clone(), command.id.clone());
                list = list.child(
                    Button::new(SharedString::from(format!("forge-ext-{index}-command-{i}")), command.title.clone())
                        .style(ButtonStyle::Transparent)
                        .size(ButtonSize::Compact)
                        .start_icon(Icon::new(IconName::ChevronRight).size(IconSize::XSmall).color(Color::Muted))
                        .tooltip(Tooltip::text("Run this command (also in the Extensions menu and the command palette)"))
                        .on_click(move |_, _, cx| host.read(cx).run_command(&id)),
                );
            }
            card = card.child(section("Commands", list));
        }
        for error in errors.iter().rev().take(3) {
            card = card.child(
                h_flex()
                    .gap_1()
                    .items_start()
                    .child(Icon::new(IconName::XCircle).size(IconSize::XSmall).color(Color::Error))
                    .child(Label::new(error.clone()).size(LabelSize::XSmall).color(Color::Error)),
            );
        }
        card.into_any_element()
    }

    /// ⋯: Reload, Settings, Export, Reveal in Finder, Uninstall.
    fn actions_menu(&self, index: usize, extension: &LoadedExtension, origin: Origin) -> AnyElement {
        let (host, id, name, path, has_settings) = (self.host.clone(), extension.id.clone(), extension.name.clone(), extension.path.clone(), extension.has_settings);
        PopoverMenu::new(("forge-ext-actions", index))
            .trigger(IconButton::new(("forge-ext-actions-button", index), IconName::Ellipsis).icon_size(IconSize::Small).tooltip(Tooltip::text("Actions")))
            .menu(move |window, cx| {
                let (host, id, name, path) = (host.clone(), id.clone(), name.clone(), path.clone());
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    let (reload_host, reload_id) = (host.clone(), id.clone());
                    menu = menu.entry("Reload", None, move |_, cx| {
                        reload_host.update(cx, |h, cx| {
                            if let Err(e) = h.reload(&reload_id, cx) {
                                h.errors.push(format!("{reload_id}: {e:#}"));
                            }
                        })
                    });
                    if has_settings {
                        let page = format!("ext:{id}");
                        menu = menu.entry("Settings", None, move |window, cx| window.dispatch_action(Box::new(forge_ui::OpenSettingsPage { page: page.clone() }), cx));
                    }
                    let export_id = id.clone();
                    menu = menu.entry("Export as Package…", None, move |_, cx| crate::install::export(&export_id, cx));
                    let reveal = path.clone();
                    menu = menu.entry("Reveal in Finder", None, move |_, cx| cx.reveal_path(&reveal));
                    if origin == Origin::Installed {
                        let (host, id, name) = (host.clone(), id.clone(), name.clone());
                        menu = menu.separator().entry("Uninstall…", None, move |window, cx| {
                            let answer = window.prompt(gpui::PromptLevel::Warning, &format!("Uninstall {name}?"), Some("Its folder is deleted."), &["Uninstall", "Cancel"], cx);
                            let (host, id) = (host.clone(), id.clone());
                            cx.spawn(async move |cx| {
                                if answer.await.ok() == Some(0) {
                                    host.update(cx, |h, cx| {
                                        if let Err(e) = h.uninstall(&id, cx) {
                                            h.errors.push(format!("{id}: {e:#}"));
                                        }
                                    });
                                }
                            })
                            .detach();
                        });
                    }
                    menu
                }))
            })
            .into_any_element()
    }
}

fn section(title: &'static str, content: impl IntoElement) -> impl IntoElement {
    v_flex().gap_1().child(Label::new(title).size(LabelSize::XSmall).color(Color::Muted)).child(content)
}

impl Render for ExtensionsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let handle: Arc<dyn PanelHandle> = Arc::new(cx.entity());
        let header = h_flex()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(colors.border)
            .child(forge_ui::panel_grip("extensions-grip", handle, "Extensions", Some(IconName::Blocks)))
            .child(Label::new("Extensions").weight(FontWeight::BOLD))
            .child(div().flex_1())
            .child(Self::install_menu());

        let query = self.search.read(cx).text(cx).to_lowercase();
        let host = self.host.read(cx);
        let mut extensions: Vec<LoadedExtension> = host
            .extensions
            .iter()
            .filter(|e| query.is_empty() || e.name.to_lowercase().contains(&query) || e.description.as_deref().is_some_and(|d| d.to_lowercase().contains(&query)))
            .cloned()
            .collect();
        extensions.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        let total = host.extensions.len();
        let other_errors = host.other_errors();

        let mut list = v_flex().gap_2().p_2();
        if total == 0 {
            list = list.child(self.render_empty());
        } else if extensions.is_empty() {
            list = list.child(Label::new("No extension matches.").size(LabelSize::Small).color(Color::Muted));
        }
        for (i, extension) in extensions.iter().enumerate() {
            list = list.child(self.render_card(i, extension, cx));
        }
        if !other_errors.is_empty() {
            let mut errors = v_flex().gap_1().child(Label::new("Other errors").size(LabelSize::XSmall).color(Color::Muted));
            for error in other_errors.iter().rev().take(5) {
                errors = errors.child(Label::new(error.clone()).size(LabelSize::XSmall).color(Color::Error));
            }
            list = list.child(errors);
        }

        v_flex()
            .key_context("ForgeExtensionsPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.panel_background)
            .child(header)
            .when(total > 0, |el| {
                el.child(div().px_2().pt_2().child(div().px_2().py_1().border_1().border_color(colors.border).rounded_md().bg(colors.editor_background).child(self.search.clone())))
            })
            .child(div().id("forge-extensions-overview").flex_1().overflow_y_scroll().child(list))
    }
}

impl Focusable for ExtensionsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ExtensionsPanel {}

impl Panel for ExtensionsPanel {
    fn persistent_name() -> &'static str {
        "ForgeExtensionsPanel"
    }
    fn panel_key() -> &'static str {
        "ForgeExtensionsPanel"
    }
    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }
    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }
    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        forge_ui::dock_position::save(Self::panel_key(), position, cx);
        gpui::BorrowAppContext::update_global::<settings::SettingsStore, _>(cx, |_, _| {});
        cx.notify();
    }
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(320.)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Blocks)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Extensions")
    }
    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }
    fn activation_priority(&self) -> u32 {
        8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Each extension gets a card with its panels, commands and own errors; the search box
    /// filters them, and an empty panel explains how to install one.
    #[gpui::test]
    async fn cards_show_what_each_extension_offers(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init_for_tests(cx);
        });
        let tmp = tempfile::tempdir().unwrap();
        for (id, code) in [
            ("good", "var __forgeExtension = { activate() { const f = __forge.modules['@forge-ide/api'].forge; f.panels.register({ id: 'good.panel', title: 'Good Panel', render: () => null }); f.commands.register('good.run', 'Do It', () => {}); } };"),
            ("broken", "var __forgeExtension = { activate() { throw new Error('boom'); } };"),
        ] {
            let ext = tmp.path().join(id);
            std::fs::create_dir_all(ext.join("dist")).unwrap();
            std::fs::write(ext.join("package.json"), format!(r#"{{"name":"{id}","displayName":"{id} ext","version":"1.0.0","description":"The {id} one","forge":{{}}}}"#)).unwrap();
            std::fs::write(ext.join("dist/extension.js"), code).unwrap();
        }
        let host = cx.update(|cx| ExtensionHost::init(vec![tmp.path().to_path_buf()], cx)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while host.read_with(cx, |h, _| h.commands.is_empty() || h.errors.is_empty()) {
            assert!(Instant::now() < deadline, "the extensions never activated");
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
        }
        host.read_with(cx, |h, _| {
            assert_eq!(h.errors_of("broken").len(), 1, "{:?}", h.errors);
            assert!(h.errors_of("good").is_empty());
            assert!(h.other_errors().is_empty());
            assert_eq!(h.commands[0].extension.as_deref(), Some("good"));
            assert_eq!(h.panels[0].extension.as_deref(), Some("good"));
            assert_eq!(h.origin("good"), Origin::Development);
        });

        let project = project::Project::test(params.fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        let panel = workspace.update_in(cx, |ws, window, cx| {
            crate::panel::add_panels(host.clone(), ws, window, cx);
            ws.focus_panel::<ExtensionsPanel>(window, cx).unwrap()
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let search = panel.read_with(cx, |p, _| p.search.clone());
        cx.update(|window, cx| search.update(cx, |e, cx| e.set_text("brok", window, cx)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}
