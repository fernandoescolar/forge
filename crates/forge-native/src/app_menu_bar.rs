//! The app's menus (`cx.set_menus`) as a row of buttons in the title bar: on platforms without
//! a global menu bar, the window has to show them itself. Like any menu bar, it is reached
//! with the keyboard (Alt+letter, F10, then the arrows), moving over another menu while one is
//! open opens that one, and what can't be done now is dimmed. In a narrow window the menus
//! fold into one button.

use gpui::{
    Action, App, Context, HighlightStyle, InteractiveElement as _, IntoElement, KeyBinding, NoAction, OwnedMenuItem, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, StyledText, UnderlineStyle, Window, actions, div, px,
};
use gpui::prelude::FluentBuilder as _;
use schemars::JsonSchema;
use serde::Deserialize;
use ui::{ButtonCommon as _, ButtonLike, ButtonStyle, ContextMenu, IconButton, IconName, IconSize, PopoverMenu, PopoverMenuHandle, StyledTypography as _, Tooltip, h_flex};

// The names of Zed's menu bar (`title_bar`, which Forge doesn't use): its keymaps bind them.
actions!(
    app_menu,
    [
        /// Opens the menu to the right of the open one.
        ActivateMenuRight,
        /// Opens the menu to the left of the open one.
        ActivateMenuLeft,
    ]
);

/// Opens the menu with this name, or else the first one.
#[derive(Clone, Deserialize, JsonSchema, PartialEq, Default, Action)]
#[action(namespace = app_menu)]
pub struct OpenApplicationMenu(String);

/// Each menu's Alt+letter, underlined in its name. Taken where Zed's keymaps leave the letter
/// free: `g` starts Editor chords (Alt+G B), `r` toggles regex search, `t` reruns the task.
/// The keys are spelled out because key bindings take only static strings.
const MNEMONICS: &[(&str, char, &str)] = &[
    ("File", 'f', "alt-f"),
    ("Edit", 'e', "alt-e"),
    ("View", 'v', "alt-v"),
    ("Go", 'o', "alt-o"),
    ("Run", 'u', "alt-u"),
    ("Git", 'i', "alt-i"),
    ("Agents", 'a', "alt-a"),
    ("Tools", 'l', "alt-l"),
    ("Help", 'h', "alt-h"),
];

/// Below this the menus don't fit next to what the title bar's other side shows.
const FOLD_BELOW: gpui::Pixels = px(1000.);

pub fn bind_keys(cx: &mut App) {
    for (name, letter, keys) in MNEMONICS {
        cx.bind_keys([KeyBinding::new(keys, OpenApplicationMenu((*name).into()), Some("Workspace"))]);
        // A terminal's programs get Alt+letter (Alt+F already goes to them in Zed's keymap).
        if *letter != 'f' {
            cx.bind_keys([KeyBinding::new(keys, NoAction, Some("Terminal"))]);
        }
    }
    // Zed's Windows keymap gives these to the search bars from any pane: only in them, here.
    if cfg!(windows) {
        cx.bind_keys([
            KeyBinding::new("alt-f", OpenApplicationMenu("File".into()), Some("Pane")),
            KeyBinding::new("alt-l", OpenApplicationMenu("Tools".into()), Some("Pane")),
            KeyBinding::new("alt-f", search::project_search::ToggleFilters, Some("ProjectSearchBar")),
            KeyBinding::new("alt-f", search::project_search::ToggleFilters, Some("ProjectSearchView")),
            KeyBinding::new("alt-l", search::ToggleSelection, Some("BufferSearchBar")),
        ]);
    }
}

pub struct AppMenuBar {
    /// One per menu, by name: they outlive the menus, which are set again as things change.
    handles: Vec<(SharedString, PopoverMenuHandle<ContextMenu>)>,
    folded: PopoverMenuHandle<ContextMenu>,
    /// To open once the bar has drawn (a menu's handle needs its element).
    pending_open: Option<String>,
}

impl AppMenuBar {
    pub fn new() -> Self {
        Self { handles: Vec::new(), folded: PopoverMenuHandle::default(), pending_open: None }
    }

    pub fn open(&mut self, action: &OpenApplicationMenu, cx: &mut Context<Self>) {
        self.pending_open = Some(action.0.clone());
        cx.notify();
    }

    fn handle(&mut self, name: &SharedString) -> PopoverMenuHandle<ContextMenu> {
        if let Some((_, handle)) = self.handles.iter().find(|(n, _)| n == name) {
            return handle.clone();
        }
        let handle = PopoverMenuHandle::default();
        self.handles.push((name.clone(), handle.clone()));
        handle
    }

    fn activate_neighbor(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(current) = self.handles.iter().position(|(_, handle)| handle.is_deployed()) else {
            cx.propagate();
            return;
        };
        let next = (current as isize + delta).rem_euclid(self.handles.len() as isize) as usize;
        self.handles[current].1.hide(cx);
        // Deferred so the next menu can take the focus from this one.
        let next = self.handles[next].1.clone();
        cx.defer_in(window, move |_, window, cx| next.show(window, cx));
    }

    fn show_pending(&mut self, fold: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.pending_open.take() else { return };
        let target = if fold { Some(self.folded.clone()) } else { self.handles.iter().find(|(n, _)| **n == *name).or(self.handles.first()).map(|(_, handle)| handle.clone()) };
        let Some(target) = target else { return };
        if target.is_deployed() {
            return;
        }
        for (_, handle) in self.handles.iter().filter(|(_, handle)| handle.is_deployed()) {
            handle.hide(cx);
        }
        // Deferred so it can take the focus, once this frame has drawn the menu's element.
        window.defer(cx, move |window, cx| target.show(window, cx));
    }
}

impl Render for AppMenuBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let menus = cx.get_menus().unwrap_or_default();
        let fold = window.viewport_size().width < FOLD_BELOW;
        let bar = h_flex()
            .key_context("ApplicationMenu")
            .occlude()
            .on_action(cx.listener(|this, _: &ActivateMenuLeft, window, cx| this.activate_neighbor(-1, window, cx)))
            .on_action(cx.listener(|this, _: &ActivateMenuRight, window, cx| this.activate_neighbor(1, window, cx)));
        let bar = if fold {
            let items = std::rc::Rc::new(menus);
            bar.child(
                PopoverMenu::new("app-menu-folded")
                    .trigger_with_tooltip(IconButton::new("app-menu-folded-button", IconName::Menu).style(ButtonStyle::Subtle).icon_size(IconSize::Small), Tooltip::text("Menus"))
                    .with_handle(self.folded.clone())
                    .menu(move |window, cx| {
                        let menus: Vec<(SharedString, Vec<OwnedMenuItem>)> = items.iter().map(|menu| (menu.name.to_string().into(), with_availability(&menu.items, window, cx))).collect();
                        Some(ContextMenu::build(window, cx, move |menu, window, cx| {
                            let mut menu = menu.when_some(window.focused(cx), |menu, focused| menu.context(focused));
                            for (name, items) in &menus {
                                let items = std::rc::Rc::new(items.clone());
                                menu = menu.submenu(name.to_string(), move |menu, _, _| fill_menu(menu, &items));
                            }
                            menu
                        }))
                    }),
            )
        } else {
            let all: Vec<_> = menus.iter().map(|menu| self.handle(&menu.name.to_string().into())).collect();
            bar.children(menus.into_iter().zip(all.clone()).map(|(menu, handle)| {
                let name: SharedString = menu.name.to_string().into();
                let items = std::rc::Rc::new(menu.items);
                let all = all.clone();
                let hovered = handle.clone();
                div()
                    .id(SharedString::from(format!("app-menu-item-{name}")))
                    .on_hover(move |entered, window, cx| {
                        if *entered && !hovered.is_deployed() && all.iter().any(|handle| handle.is_deployed()) {
                            all.iter().for_each(|handle| handle.hide(cx));
                            // Deferred so this menu can take the focus from the other.
                            let hovered = hovered.clone();
                            window.defer(cx, move |window, cx| hovered.show(window, cx));
                        }
                    })
                    .child(
                        PopoverMenu::new(SharedString::from(format!("app-menu-{name}")))
                            .trigger(ButtonLike::new(SharedString::from(format!("app-menu-button-{name}"))).style(ButtonStyle::Subtle).child(title(&name, cx)))
                            .with_handle(handle)
                            .menu(move |window, cx| {
                                let items = with_availability(&items, window, cx);
                                Some(ContextMenu::build(window, cx, move |menu, window, cx| {
                                    // Actions apply to what had focus before the menu opened.
                                    let menu = menu.when_some(window.focused(cx), |menu, focused| menu.context(focused));
                                    fill_menu(menu, &items)
                                }))
                            }),
                    )
            }))
        };
        self.show_pending(fold, window, cx);
        bar
    }
}

/// The menu's name with its Alt+letter underlined.
fn title(name: &SharedString, cx: &App) -> impl IntoElement {
    let letter = MNEMONICS.iter().find(|(menu, _, _)| *menu == name.as_ref()).map(|(_, letter, _)| *letter);
    let start = letter.and_then(|letter| name.char_indices().find(|(_, c)| c.to_ascii_lowercase() == letter).map(|(i, _)| i));
    let underline = HighlightStyle { underline: Some(UnderlineStyle { thickness: px(1.), ..Default::default() }), ..Default::default() };
    let highlights: Vec<_> = start.map(|start| (start..start + 1, underline)).into_iter().collect();
    div().text_ui_sm(cx).child(StyledText::new(name.clone()).with_highlights(highlights))
}

/// The items with what can't be done right now disabled. Asked here, before the menu opens:
/// once it has the focus, the actions of what had it are out of reach.
fn with_availability(items: &[OwnedMenuItem], window: &mut Window, cx: &mut App) -> Vec<OwnedMenuItem> {
    // Nothing focused says nothing about what the window can do.
    let check = window.focused(cx).is_some();
    items
        .iter()
        .map(|item| match item.clone() {
            OwnedMenuItem::Action { name, action, os_action, checked, disabled } => {
                let available = !check || window.is_action_available(&*action, cx) || cx.is_action_available(&*action);
                OwnedMenuItem::Action { name, action, os_action, checked, disabled: disabled || !available }
            }
            OwnedMenuItem::Submenu(mut submenu) => {
                submenu.items = with_availability(&submenu.items, window, cx);
                OwnedMenuItem::Submenu(submenu)
            }
            item => item,
        })
        .collect()
}

fn fill_menu(menu: ContextMenu, items: &[OwnedMenuItem]) -> ContextMenu {
    let mut menu = menu;
    let mut last_was_separator = true;
    for item in items {
        match item {
            OwnedMenuItem::Separator => {
                if !last_was_separator {
                    menu = menu.separator();
                    last_was_separator = true;
                }
            }
            OwnedMenuItem::Action { name, action, checked, disabled, .. } => {
                menu = menu.action_checked_with_disabled(name.clone(), action.boxed_clone(), *checked, *disabled);
                last_was_separator = false;
            }
            OwnedMenuItem::Submenu(submenu) if !submenu.items.is_empty() => {
                let items = std::rc::Rc::new(submenu.items.clone());
                menu = menu.submenu(submenu.name.to_string(), move |menu, _, _| fill_menu(menu, &items));
                last_was_separator = false;
            }
            OwnedMenuItem::Submenu(_) | OwnedMenuItem::SystemMenu(_) => {}
        }
    }
    menu
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Menu, MenuItem, TestAppContext, VisualTestContext};

    actions!(test_app_menu, [Available, Unavailable]);

    struct Host {
        bar: gpui::Entity<AppMenuBar>,
        focus: gpui::FocusHandle,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().track_focus(&self.focus).on_action(|_: &Available, _, _| {}).child(self.bar.clone())
        }
    }

    fn deployed(bar: &gpui::Entity<AppMenuBar>, cx: &mut VisualTestContext) -> Vec<String> {
        bar.read_with(cx, |bar, _| bar.handles.iter().filter(|(_, handle)| handle.is_deployed()).map(|(name, _)| name.to_string()).collect())
    }

    /// The keyboard opens a menu by name and moves between them, wrapping around.
    #[gpui::test]
    async fn the_keyboard_reaches_the_menus(cx: &mut TestAppContext) {
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            workspace::init(params.clone(), cx);
            cx.set_menus([
                Menu::new("File").items([MenuItem::action("Available", Available)]),
                Menu::new("Edit").items([MenuItem::action("Unavailable", Unavailable)]),
            ]);
        });
        let window = cx.add_window(|window, cx| {
            let focus = cx.focus_handle();
            window.focus(&focus, cx);
            Host { bar: cx.new(|_| AppMenuBar::new()), focus }
        });
        window.update(cx, |_, window, cx| {
            window.resize(gpui::size(px(1400.), px(800.)));
            assert!(window.viewport_size().width >= FOLD_BELOW, "{:?}", window.viewport_size());
            cx.notify();
        }).unwrap();
        let bar = window.read_with(cx, |host, _| host.bar.clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();

        bar.update(cx, |bar, cx| bar.open(&OpenApplicationMenu("Edit".into()), cx));
        cx.update(|window, cx| drop(window.draw(cx)));
        cx.run_until_parked();
        assert_eq!(deployed(&bar, cx), ["Edit"]);

        bar.update_in(cx, |bar, window, cx| bar.activate_neighbor(1, window, cx));
        cx.run_until_parked();
        assert_eq!(deployed(&bar, cx), ["File"], "right of the last is the first");

        bar.update_in(cx, |bar, window, cx| bar.activate_neighbor(-1, window, cx));
        cx.run_until_parked();
        assert_eq!(deployed(&bar, cx), ["Edit"], "left of the first is the last");
    }

    /// What nothing handles right now is dimmed; what has a handler isn't.
    #[gpui::test]
    async fn what_cant_be_done_is_disabled(cx: &mut TestAppContext) {
        let window = cx.add_window(|window, cx| {
            let focus = cx.focus_handle();
            window.focus(&focus, cx);
            Host { bar: cx.new(|_| AppMenuBar::new()), focus }
        });
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        let items: Vec<OwnedMenuItem> = Menu::new("Test").items([MenuItem::action("Available", Available), MenuItem::submenu(Menu::new("Nested").items([MenuItem::action("Unavailable", Unavailable)]))]).owned().items;
        let checked = cx.update(|window, cx| with_availability(&items, window, cx));
        assert!(matches!(&checked[0], OwnedMenuItem::Action { disabled: false, .. }));
        let OwnedMenuItem::Submenu(nested) = &checked[1] else { panic!("a submenu") };
        assert!(matches!(&nested.items[0], OwnedMenuItem::Action { disabled: true, .. }), "nested items are checked too");
    }
}
