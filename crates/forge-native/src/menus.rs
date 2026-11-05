//! The menus of the menu bar (macOS's global one, or the row in the title bar elsewhere; see
//! `app_menu_bar`) and the app-level actions behind them. macOS and Linux/Windows have menus
//! of their own: where things are, what they're called and what they show follows each.
//!
//! Most items dispatch Zed's own actions (editor, workspace, search…). Actions that Zed's
//! `zed` crate normally handles (settings files, zoom, window management, About) are
//! implemented here, plus Forge's own (agents, extensions, palettes, guide).

use gpui::TaskExt as _;
use gpui::{Action, App, AppContext as _, Context, Menu, MenuItem, OsAction, PromptLevel, SystemMenuType, Window, actions};
use multi_buffer::MultiBuffer;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use workspace::{OpenOptions, Workspace};

actions!(
    forge,
    [
        /// Shows Forge's version and the Zed revision it is built on.
        About,
        /// Opens the user settings file.
        OpenSettingsFile,
        /// Opens Forge's default settings (read-only).
        OpenDefaultSettings,
        /// Opens the user key bindings file.
        OpenKeymapFile,
        /// Opens the default key bindings (read-only).
        OpenDefaultKeymap,
        /// Opens the agents configuration file.
        OpenAgentsFile,
        /// Opens the project's instructions for agents (`.forge/AGENTS.md`).
        OpenProjectInstructions,
        /// Opens your instructions for agents in every project.
        OpenUserInstructions,
        /// Reveals the colour palettes folder.
        OpenPalettesFolder,
        /// Reveals the extensions folder.
        OpenExtensionsFolder,
        /// Opens the welcome page.
        ShowWelcome,
        /// Opens the Forge guide (README) inside Forge.
        OpenGuide,
        /// Hides Forge.
        Hide,
        /// Hides other applications.
        HideOthers,
        /// Shows all applications.
        ShowAll,
        /// Minimizes the window.
        Minimize,
        /// Zooms (maximizes) the window.
        Zoom,
        /// Toggles the window between full screen and windowed.
        ToggleFullScreen,
        /// Makes the editor font bigger.
        IncreaseFontSize,
        /// Makes the editor font smaller.
        DecreaseFontSize,
        /// Restores the editor font size from settings.
        ResetFontSize,
        /// Restores editor and UI font sizes from settings.
        ResetAllZoom,
        /// Chooses what Run and Debug start.
        SelectRunTarget,
        /// Brings all of Forge's windows to the front.
        BringAllToFront,
        /// Empties the recent projects lists.
        ClearRecentProjects,
    ]
);

/// Command-palette namespaces hidden in Forge: Zed's own app actions (re-exposed under
/// `forge:` where they make sense) and Zed services Forge doesn't ship (Zed account,
/// agent threads sidebar, developer tools).
pub const HIDDEN_PALETTE_NAMESPACES: &[&str] = &["zed", "client", "agents_sidebar", "multi_workspace", "dev"];

/// Opens a URL in the default browser.
#[derive(PartialEq, Clone, Deserialize, Default, JsonSchema, Action)]
#[action(namespace = forge)]
#[serde(deny_unknown_fields)]
pub struct OpenBrowser {
    pub url: String,
}

/// Opens a project of File › Open Recent, or of the Dock's menu.
#[derive(PartialEq, Clone, Deserialize, Default, JsonSchema, Action)]
#[action(namespace = forge)]
#[serde(deny_unknown_fields)]
pub struct OpenRecentProject {
    pub paths: Vec<PathBuf>,
}

/// How many recent projects the menus show.
const MAX_RECENT: usize = 10;

const GUIDE: &str = include_str!("../../../docs/GUIDE.md");
const FORGE_DEFAULTS: &str = include_str!("../assets/forge-defaults.json");
const ZED_MANIFEST: &str = include_str!("../../../vendor/zed/crates/zed/Cargo.toml");

pub fn init(cx: &mut App) {
    command_palette_hooks::CommandPaletteFilter::update_global(cx, |filter, _| {
        for namespace in HIDDEN_PALETTE_NAMESPACES {
            filter.hide_namespace(namespace);
        }
        // A command with what to open in it: from the menus only.
        filter.hide_action_types(&[std::any::TypeId::of::<OpenRecentProject>()]);
    });
    cx.bind_keys([
        gpui::KeyBinding::new("secondary-=", IncreaseFontSize, None),
        gpui::KeyBinding::new("secondary-+", IncreaseFontSize, None),
        gpui::KeyBinding::new("secondary--", DecreaseFontSize, None),
        gpui::KeyBinding::new("secondary-0", ResetFontSize, None),
    ]);
    // Only where the system has them: elsewhere GPUI ignores them, or (Windows) panics, and
    // without a handler the command palette doesn't offer them.
    if MenuPlatform::current() == MenuPlatform::Mac {
        cx.on_action(|_: &Hide, cx| cx.hide());
        cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
        cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
        cx.on_action(|_: &BringAllToFront, cx| cx.activate(true));
    }
    cx.on_action(|action: &OpenRecentProject, cx| {
        let Some(app_state) = workspace::AppState::try_global(cx) else { return };
        workspace::open_paths(&action.paths, app_state, crate::own_window(), cx).detach_and_log_err(cx);
    });
    cx.on_action(|_: &ClearRecentProjects, cx| {
        crate::welcome::clear_recent(cx);
        set_recent_projects(Vec::new(), cx);
    });
    cx.on_action(|action: &OpenBrowser, cx| cx.open_url(&action.url));
    cx.on_action(|_: &OpenPalettesFolder, cx| reveal(&paths::config_dir().join("palettes"), cx));
    cx.on_action(|_: &OpenExtensionsFolder, cx| reveal(&paths::data_dir().join("extensions"), cx));

    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|_, _: &Minimize, window, _| window.minimize_window())
            .register_action(|_, _: &Zoom, window, _| window.zoom_window())
            .register_action(|_, _: &ToggleFullScreen, window, _| window.toggle_fullscreen())
            .register_action(|_, _: &About, window, cx| about(window, cx))
            // Zed's "Install Themes" buttons and the like: Forge's extensions are in its panel.
            .register_action(|ws, _: &zed_actions::Extensions, window, cx| {
                ws.focus_panel::<forge_extension_host::ExtensionsPanel>(window, cx);
            })
            .register_action(|ws, _: &SelectRunTarget, window, cx| select_run_target(ws, window, cx))
            .register_action(|ws, _: &workspace::ReloadActiveItem, window, cx| {
                if let Some(item) = ws.active_item(cx) {
                    item.reload(ws.project().clone(), window, cx).detach_and_log_err(cx);
                }
            })
            .register_action(|ws, _: &workspace::CloseProject, window, cx| close_project(ws, window, cx))
            .register_action(|ws, _: &OpenSettingsFile, window, cx| {
                open_config_file(ws, paths::settings_file(), crate::config_files::USER_SETTINGS, window, cx)
            })
            .register_action(|ws, _: &OpenKeymapFile, window, cx| {
                open_config_file(ws, paths::keymap_file(), crate::config_files::KEYMAP, window, cx)
            })
            .register_action(|ws, _: &OpenAgentsFile, window, cx| {
                // forge-agents creates it with defaults the first time the panel loads.
                open_config_file(ws, &paths::config_dir().join("agents.json"), "{ \"agents\": [] }\n", window, cx)
            })
            .register_action(|ws, _: &OpenProjectInstructions, window, cx| {
                // The file would be created on this machine's disk, under the host's path.
                if forge_ui::is_remote(ws.project().read(cx)) {
                    return forge_ui::show_not_on_remote(ws, "Project instructions", cx);
                }
                let Some(root) = ws.visible_worktrees(cx).next().map(|t| t.read(cx).abs_path().to_path_buf()) else { return };
                let file = forge_agents::rules::project_file(&root, &forge_agents::rules::configured_files(cx));
                open_config_file(ws, &file, INSTRUCTIONS_TEMPLATE, window, cx)
            })
            .register_action(|ws, _: &OpenUserInstructions, window, cx| {
                open_config_file(ws, &forge_agents::rules::user_file(), INSTRUCTIONS_TEMPLATE, window, cx)
            })
            .register_action(|ws, _: &OpenDefaultSettings, window, cx| {
                open_text(ws, FORGE_DEFAULTS, "Forge Default Settings", "JSONC", window, cx);
            })
            .register_action(|ws, _: &OpenDefaultKeymap, window, cx| {
                open_text(ws, &settings::default_keymap(), "Default Key Bindings", "JSONC", window, cx);
            })
            .register_action(|ws, _: &OpenGuide, window, cx| open_text(ws, GUIDE, "Forge Guide", "Markdown", window, cx))
            .register_action(|ws, _: &ShowWelcome, window, cx| crate::welcome::show(ws, window, cx))
            .register_action(|ws, action: &zed_actions::OpenRecent, window, cx| {
                crate::welcome::open_recent(ws, action.create_new_window.unwrap_or(false), window, cx)
            })
            .register_action(|_, _: &IncreaseFontSize, _, cx| theme_settings::increase_buffer_font_size(cx))
            .register_action(|_, _: &DecreaseFontSize, _, cx| theme_settings::decrease_buffer_font_size(cx))
            .register_action(|_, _: &ResetFontSize, _, cx| theme_settings::reset_buffer_font_size(cx))
            .register_action(|_, _: &ResetAllZoom, _, cx| {
                theme_settings::reset_ui_font_size(cx);
                theme_settings::reset_buffer_font_size(cx);
            })
            // Zed's equivalents stay handled for bindings in user keymaps.
            .register_action(|_, _: &zed_actions::IncreaseBufferFontSize, _, cx| theme_settings::increase_buffer_font_size(cx))
            .register_action(|_, _: &zed_actions::DecreaseBufferFontSize, _, cx| theme_settings::decrease_buffer_font_size(cx))
            .register_action(|_, _: &zed_actions::ResetBufferFontSize, _, cx| theme_settings::reset_buffer_font_size(cx))
            .register_action(|_, _: &zed_actions::ResetAllZoom, _, cx| {
                theme_settings::reset_ui_font_size(cx);
                theme_settings::reset_buffer_font_size(cx);
            });
    })
    .detach();
}

/// Closes the project but keeps the window, showing the welcome page.
fn close_project(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(window_handle) = window.window_handle().downcast::<workspace::MultiWorkspace>() else { return };
    let Some(app_state) = workspace::AppState::try_global(cx) else { return };
    cx.spawn_in(window, async move |this, cx| {
        let proceed = this.update_in(cx, |ws, window, cx| ws.prepare_to_close(workspace::CloseIntent::ReplaceWindow, window, cx))?.await?;
        if proceed {
            let open = cx.update(|_, cx| {
                workspace::open_new(OpenOptions { requesting_window: Some(window_handle), ..Default::default() }, app_state, cx, |ws, window, cx| {
                    crate::welcome::show(ws, window, cx)
                })
            })?;
            open.await?;
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn about(window: &mut Window, cx: &mut Context<Workspace>) {
    let zed = ZED_MANIFEST.lines().find_map(|l| l.strip_prefix("version = ")).map(|v| v.trim_matches('"')).unwrap_or("?");
    let detail = format!(
        "Version {}\n\nForge is built with open-source libraries from the Zed editor (zed.dev, v{zed}): GPUI and its editor, terminal and language components.\n\nGPL-3.0-or-later",
        env!("CARGO_PKG_VERSION")
    );
    let answer = window.prompt(PromptLevel::Info, "Forge", Some(&detail), &["OK"], cx);
    cx.background_spawn(async move {
        let _ = answer.await;
    })
    .detach();
}

/// Opens a config file in an editor tab, creating it with `default` if missing.
/// A new instructions file: empty files are not sent, so this one isn't until written in.
const INSTRUCTIONS_TEMPLATE: &str = "";

fn open_config_file(workspace: &mut Workspace, path: &Path, default: &str, window: &mut Window, cx: &mut Context<Workspace>) {
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, default);
    }
    workspace.open_abs_path(path.to_path_buf(), OpenOptions::default(), window, cx).detach_and_log_err(cx);
}

/// Shows bundled text (guide, defaults) in a read-only editor tab.
fn open_text(workspace: &mut Workspace, text: &str, title: &str, language: &'static str, window: &mut Window, cx: &mut Context<Workspace>) {
    let project = workspace.project().clone();
    let languages = workspace.app_state().languages.clone();
    let (text, title) = (text.to_string(), title.to_string());
    cx.spawn_in(window, async move |workspace, cx| {
        let language = languages.language_for_name(language).await.ok();
        workspace.update_in(cx, |workspace, window, cx| {
            let buffer = project.update(cx, |p, cx| p.create_local_buffer(&text, language, false, cx));
            let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title));
            let editor = cx.new(|cx| {
                let mut editor = editor::Editor::for_multibuffer(multibuffer, Some(project), window, cx);
                editor.set_read_only(true);
                editor
            });
            workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
        })
    })
    .detach_and_log_err(cx);
}

fn reveal(dir: &PathBuf, cx: &mut App) {
    let _ = std::fs::create_dir_all(dir);
    cx.reveal_path(dir);
}

/// What the menus show of the extensions: built from the extension host, so the menus are
/// set again when what it offers changes (see [`extension_menus_signature`]).
struct ExtensionEntries {
    /// Extension panels for View › Panels: (title, panel id).
    panels: Vec<(String, String)>,
    /// Extensions with commands, for the Extensions menu: (name, [(title, command id)]).
    commands: Vec<(String, Vec<(String, String)>)>,
}

fn extension_entries(cx: &App) -> ExtensionEntries {
    let Some(host) = forge_extension_host::ExtensionHost::global(cx) else { return ExtensionEntries { panels: vec![], commands: vec![] } };
    let host = host.read(cx);
    let mut panels: Vec<(String, String)> = host.panels.iter().map(|p| (p.title.clone(), p.id.clone())).collect();
    panels.sort();
    let mut commands: Vec<(String, Vec<(String, String)>)> = host
        .extensions
        .iter()
        .map(|e| (e.name.clone(), host.commands.iter().filter(|c| c.extension.as_deref() == Some(e.id.as_str())).map(|c| (c.title.clone(), c.id.clone())).collect::<Vec<_>>()))
        .filter(|(_, commands)| !commands.is_empty())
        .collect();
    commands.sort();
    ExtensionEntries { panels, commands }
}

/// Changes when the extension items of the menus would: the menus are set again then.
pub fn extension_menus_signature(cx: &App) -> String {
    let entries = extension_entries(cx);
    format!("{:?}{:?}", entries.panels, entries.commands)
}

/// Whose menus: each platform shows the commands the way its apps do. A value rather than a
/// `cfg!`, so that the tests check every platform's menus on any machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuPlatform {
    Mac,
    Linux,
    Windows,
}

impl MenuPlatform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

/// What the menus show of the active window and of the projects opened lately. When it
/// changes the menus are set again.
#[derive(Clone, Default, PartialEq)]
pub struct MenuState {
    pub recent: Vec<crate::welcome::Recent>,
    pub fullscreen: bool,
    pub left_dock: bool,
    pub right_dock: bool,
    pub bottom_dock: bool,
    /// The .NET commands are offered where they apply.
    pub has_solution: bool,
}

impl gpui::Global for MenuState {}

/// Sets the menus again, for what they show has changed.
pub fn refresh(cx: &mut App) {
    cx.set_menus(app_menus(cx));
    // The title bar draws them where there is no global menu bar.
    if MenuPlatform::current() != MenuPlatform::Mac {
        cx.refresh_windows();
    }
}

fn update_state(cx: &mut App, change: impl FnOnce(&mut MenuState)) {
    let mut state = cx.try_global::<MenuState>().cloned().unwrap_or_default();
    let before = state.clone();
    change(&mut state);
    if state != before {
        cx.set_global(state);
        refresh(cx);
    }
}

/// Keeps [`MenuState`] up to date with the active window.
pub fn track_state(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        for dock in [workspace.left_dock().clone(), workspace.right_dock().clone(), workspace.bottom_dock().clone()] {
            cx.observe_in(&dock, window, |workspace, _, window, cx| note_window(workspace, window, cx)).detach();
        }
        cx.observe_window_bounds(window, |workspace, window, cx| note_window(workspace, window, cx)).detach();
        cx.observe_window_activation(window, |workspace, window, cx| {
            note_window(workspace, window, cx);
            if window.is_window_active() {
                load_recent_projects(workspace, Duration::ZERO, cx);
            }
        })
        .detach();
        // The database gets the project a moment after the window opens.
        load_recent_projects(workspace, Duration::from_secs(2), cx);
    })
    .detach();
}

/// The solution comes and goes after the window opened.
pub fn track_solution(model: &gpui::Entity<forge_dotnet::model::DotnetModel>, window: &mut Window, cx: &mut Context<Workspace>) {
    cx.observe_in(model, window, |workspace, _, window, cx| note_window(workspace, window, cx)).detach();
}

fn note_window(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !window.is_window_active() {
        return;
    }
    let fullscreen = window.is_fullscreen();
    let (left_dock, right_dock, bottom_dock) = (workspace.left_dock().read(cx).is_open(), workspace.right_dock().read(cx).is_open(), workspace.bottom_dock().read(cx).is_open());
    let has_solution = workspace.panel::<forge_dotnet::SolutionExplorer>(cx).is_some_and(|panel| panel.read(cx).model().read(cx).solution.is_some());
    update_state(cx, |state| {
        state.fullscreen = fullscreen;
        (state.left_dock, state.right_dock, state.bottom_dock) = (left_dock, right_dock, bottom_dock);
        state.has_solution = has_solution;
    });
}

fn load_recent_projects(workspace: &Workspace, after: Duration, cx: &mut Context<Workspace>) {
    let executor = cx.background_executor().clone();
    let recent = crate::welcome::load_recent(workspace, cx);
    cx.spawn(async move |_, cx| {
        executor.timer(after).await;
        let mut recent = recent.await;
        recent.truncate(MAX_RECENT);
        cx.update(|cx| set_recent_projects(recent, cx));
    })
    .detach();
}

fn set_recent_projects(recent: Vec<crate::welcome::Recent>, cx: &mut App) {
    let changed = cx.try_global::<MenuState>().is_none_or(|state| state.recent != recent);
    if changed {
        offer_outside_the_window(&recent, cx);
    }
    update_state(cx, |state| state.recent = recent);
}

/// The Dock's menu and the system's recent documents on macOS, the Jump List on Windows.
/// On Linux the desktop reads the same from the `.desktop` file (`bundle-linux.sh`).
fn offer_outside_the_window(recent: &[crate::welcome::Recent], cx: &mut App) {
    match MenuPlatform::current() {
        MenuPlatform::Mac => {
            let mut items = vec![MenuItem::action("New Window", workspace::NewWindow)];
            if !recent.is_empty() {
                items.push(MenuItem::separator());
                items.extend(recent.iter().map(|r| MenuItem::action(r.name(), OpenRecentProject { paths: r.paths.clone() })));
            }
            cx.set_dock_menu(items);
            // Oldest first: the system lists the last one added at the top.
            for path in recent.iter().rev().filter_map(|r| r.paths.first()) {
                cx.add_recent_document(path);
            }
        }
        MenuPlatform::Windows => {
            let entries = recent.iter().map(|r| r.paths.iter().cloned().collect()).collect();
            cx.update_jump_list(vec![MenuItem::action("New Window", workspace::NewWindow)], entries).detach();
        }
        MenuPlatform::Linux => {}
    }
}

/// The application menus, this platform's. Each menu holds one kind of thing.
pub fn app_menus(cx: &App) -> Vec<Menu> {
    let state = cx.try_global::<MenuState>().cloned().unwrap_or_default();
    app_menus_for(MenuPlatform::current(), &state, cx)
}

/// macOS: Forge, File, Edit, Code, View, Go, Run, Git, Agents, Tools, Window and Help, in the
/// order Mac apps have them. Linux and Windows: File, Edit, View, Go, Run, Git, Agents, Tools
/// and Help; what the application menu holds on a Mac goes in File and Help, and the window's
/// own buttons do what the Window menu does.
pub fn app_menus_for(platform: MenuPlatform, state: &MenuState, cx: &App) -> Vec<Menu> {
    let extensions = extension_entries(cx);
    let tools = tools_menu(platform, state, &extensions);
    let panels = panels_menu(&extensions);
    let shared = |menus: &mut Vec<Menu>| menus.extend([go_menu(), run_menu(state), git_menu(), agents_menu(), tools]);
    let mut menus = Vec::new();
    if platform == MenuPlatform::Mac {
        menus.extend([forge_menu(), mac_file_menu(state), mac_edit_menu(), code_menu(), mac_view_menu(state, panels)]);
        shared(&mut menus);
        menus.extend([window_menu(), mac_help_menu()]);
    } else {
        menus.extend([other_file_menu(platform, state), other_edit_menu(), other_view_menu(state, panels)]);
        shared(&mut menus);
        menus.push(other_help_menu());
    }
    menus
}

fn forge_menu() -> Menu {
    Menu::new("Forge").items([
        MenuItem::action("About Forge", About),
        MenuItem::action("Check for Updates…", forge_update::CheckForUpdates),
        MenuItem::separator(),
        MenuItem::action("Settings…", crate::settings_view::OpenSettings),
        configuration_files(),
        MenuItem::separator(),
        MenuItem::os_submenu("Services", SystemMenuType::Services),
        MenuItem::separator(),
        MenuItem::action("Hide Forge", Hide),
        MenuItem::action("Hide Others", HideOthers),
        MenuItem::action("Show All", ShowAll),
        MenuItem::separator(),
        MenuItem::action("Quit Forge", crate::Quit),
    ])
}

fn mac_file_menu(state: &MenuState) -> Menu {
    Menu::new("File").items([
        MenuItem::action("New File", workspace::NewFile),
        MenuItem::action("New Window", workspace::NewWindow),
        new_project(),
        MenuItem::separator(),
        MenuItem::action("Open…", workspace::Open::default()),
        open_recent(MenuPlatform::Mac, state),
        MenuItem::action("Open Remote…", zed_actions::OpenRemote::default()),
        MenuItem::action("Add Folder to Project…", workspace::AddFolderToProject),
        MenuItem::separator(),
        MenuItem::action("Close Editor", workspace::CloseActiveItem { save_intent: None, close_pinned: true }),
        MenuItem::action("Close All Editors", workspace::CloseAllItemsAndPanes::default()),
        MenuItem::action("Close Project", workspace::CloseProject),
        MenuItem::action("Close Window", workspace::CloseWindow),
        MenuItem::separator(),
        MenuItem::action("Save", workspace::Save { save_intent: None }),
        MenuItem::action("Save As…", workspace::SaveAs),
        MenuItem::action("Save All", workspace::SaveAll { save_intent: None }),
        MenuItem::action("Revert to Saved", workspace::ReloadActiveItem),
    ])
}

fn other_file_menu(platform: MenuPlatform, state: &MenuState) -> Menu {
    Menu::new("File").items([
        MenuItem::action("New File", workspace::NewFile),
        MenuItem::action("New Window", workspace::NewWindow),
        new_project(),
        MenuItem::separator(),
        MenuItem::action("Open…", workspace::Open::default()),
        open_recent(platform, state),
        MenuItem::action("Open Remote…", zed_actions::OpenRemote::default()),
        MenuItem::action("Add Folder to Project…", workspace::AddFolderToProject),
        MenuItem::separator(),
        MenuItem::action("Save", workspace::Save { save_intent: None }),
        MenuItem::action("Save As…", workspace::SaveAs),
        MenuItem::action("Save All", workspace::SaveAll { save_intent: None }),
        MenuItem::action("Revert File", workspace::ReloadActiveItem),
        MenuItem::separator(),
        MenuItem::action("Close Editor", workspace::CloseActiveItem { save_intent: None, close_pinned: true }),
        MenuItem::action("Close All Editors", workspace::CloseAllItemsAndPanes::default()),
        MenuItem::action("Close Project", workspace::CloseProject),
        MenuItem::action("Close Window", workspace::CloseWindow),
        MenuItem::separator(),
        MenuItem::submenu(Menu::new("Preferences").items([
            MenuItem::action("Settings…", crate::settings_view::OpenSettings),
            configuration_files(),
            MenuItem::separator(),
            MenuItem::action("Theme…", zed_actions::theme_selector::Toggle::default()),
            MenuItem::action("Icon Theme…", zed_actions::icon_theme_selector::Toggle::default()),
            MenuItem::action("Color Palettes Folder", OpenPalettesFolder),
        ])),
        MenuItem::separator(),
        MenuItem::action(if platform == MenuPlatform::Windows { "Exit" } else { "Quit" }, crate::Quit),
    ])
}

fn new_project() -> MenuItem {
    MenuItem::submenu(Menu::new("New Project").items([
        MenuItem::action(".NET Solution…", forge_dotnet::explorer::NewSolution),
        MenuItem::action(".NET Project…", forge_dotnet::explorer::NewProject),
    ]))
}

/// The projects opened lately, each a click away. The picker has them all, with their paths.
fn open_recent(platform: MenuPlatform, state: &MenuState) -> MenuItem {
    let home = std::env::home_dir();
    let mut items: Vec<MenuItem> = state.recent.iter().map(|r| MenuItem::action(format!("{} — {}", r.name(), r.location(home.as_deref())), OpenRecentProject { paths: r.paths.clone() })).collect();
    if items.is_empty() {
        items.push(MenuItem::action("No Recent Projects", ClearRecentProjects).disabled(true));
    }
    items.push(MenuItem::separator());
    items.push(MenuItem::action("More…", zed_actions::OpenRecent::default()));
    if platform == MenuPlatform::Mac {
        items.push(MenuItem::action("Clear Menu", ClearRecentProjects).disabled(state.recent.is_empty()));
    }
    MenuItem::submenu(Menu::new("Open Recent").items(items))
}

fn configuration_files() -> MenuItem {
    MenuItem::submenu(Menu::new("Configuration Files").items([
        MenuItem::action("Open Settings File", OpenSettingsFile),
        MenuItem::action("Open Default Settings", OpenDefaultSettings),
        MenuItem::separator(),
        MenuItem::action("Open Key Bindings", OpenKeymapFile),
        MenuItem::action("Open Default Key Bindings", OpenDefaultKeymap),
        MenuItem::separator(),
        MenuItem::action("Edit agents.json", OpenAgentsFile),
    ]))
}

fn clipboard_items() -> Vec<MenuItem> {
    use editor::actions as ed;
    vec![
        MenuItem::os_action("Undo", ed::Undo, OsAction::Undo),
        MenuItem::os_action("Redo", ed::Redo, OsAction::Redo),
        MenuItem::separator(),
        MenuItem::os_action("Cut", ed::Cut, OsAction::Cut),
        MenuItem::os_action("Copy", ed::Copy, OsAction::Copy),
        MenuItem::action("Copy and Trim", ed::CopyAndTrim),
        MenuItem::os_action("Paste", ed::Paste, OsAction::Paste),
        MenuItem::os_action("Select All", ed::SelectAll, OsAction::SelectAll),
    ]
}

fn mac_edit_menu() -> Menu {
    let mut items = clipboard_items();
    items.extend([
        MenuItem::separator(),
        MenuItem::submenu(Menu::new("Find").items([
            MenuItem::action("Find", search::buffer_search::Deploy::find()),
            MenuItem::action("Find and Replace", search::buffer_search::Deploy::replace()),
            MenuItem::action("Find in Project", workspace::DeploySearch::default()),
        ])),
        selection_menu(),
    ]);
    Menu::new("Edit").items(items)
}

/// Linux and Windows: everything about editing text in one menu, the selection included.
fn other_edit_menu() -> Menu {
    use editor::actions as ed;
    let mut items = clipboard_items();
    items.extend([
        MenuItem::separator(),
        MenuItem::action("Find", search::buffer_search::Deploy::find()),
        MenuItem::action("Replace", search::buffer_search::Deploy::replace()),
        MenuItem::action("Find in Project", workspace::DeploySearch::default()),
        MenuItem::separator(),
        selection_menu(),
        MenuItem::separator(),
        MenuItem::action("Toggle Line Comment", ed::ToggleComments::default()),
        MenuItem::action("Format Document", ed::Format),
        MenuItem::submenu(Menu::new("Refactor").items([
            MenuItem::action("Rename Symbol", ed::Rename),
            MenuItem::action("Code Actions", ed::ToggleCodeActions::default()),
        ])),
        lines_menu(),
        transform_menu(),
    ]);
    Menu::new("Edit").items(items)
}

fn selection_menu() -> MenuItem {
    use editor::actions as ed;
    MenuItem::submenu(Menu::new("Selection").items([
        MenuItem::action("Select Line", ed::SelectLine),
        MenuItem::action("Expand Selection", ed::SelectLargerSyntaxNode),
        MenuItem::action("Shrink Selection", ed::SelectSmallerSyntaxNode),
        MenuItem::separator(),
        MenuItem::action("Add Cursor Above", ed::AddSelectionAbove { skip_soft_wrap: true }),
        MenuItem::action("Add Cursor Below", ed::AddSelectionBelow { skip_soft_wrap: true }),
        MenuItem::action("Add Next Occurrence", ed::SelectNext { replace_newest: false }),
        MenuItem::action("Add Previous Occurrence", ed::SelectPrevious { replace_newest: false }),
        MenuItem::action("Select All Occurrences", ed::SelectAllMatches),
    ]))
}

fn lines_menu() -> MenuItem {
    use editor::actions as ed;
    MenuItem::submenu(Menu::new("Lines").items([
        MenuItem::action("Indent", ed::Indent),
        MenuItem::action("Outdent", ed::Outdent),
        MenuItem::separator(),
        MenuItem::action("Move Line Up", ed::MoveLineUp),
        MenuItem::action("Move Line Down", ed::MoveLineDown),
        MenuItem::action("Duplicate Line Up", ed::DuplicateLineUp),
        MenuItem::action("Duplicate Line Down", ed::DuplicateLineDown),
        MenuItem::separator(),
        MenuItem::action("Sort Lines", ed::SortLinesCaseSensitive),
        MenuItem::action("Reverse Lines", ed::ReverseLines),
        MenuItem::action("Join Lines", ed::JoinLines),
        MenuItem::action("Delete Line", ed::DeleteLine),
    ]))
}

fn transform_menu() -> MenuItem {
    use editor::actions as ed;
    MenuItem::submenu(Menu::new("Transform").items([
        MenuItem::action("UPPERCASE", ed::ConvertToUpperCase),
        MenuItem::action("lowercase", ed::ConvertToLowerCase),
        MenuItem::action("Title Case", ed::ConvertToTitleCase),
        MenuItem::action("snake_case", ed::ConvertToSnakeCase),
        MenuItem::action("kebab-case", ed::ConvertToKebabCase),
        MenuItem::action("UpperCamelCase", ed::ConvertToUpperCamelCase),
        MenuItem::action("lowerCamelCase", ed::ConvertToLowerCamelCase),
    ]))
}

fn code_menu() -> Menu {
    use editor::actions as ed;
    Menu::new("Code").items([
        MenuItem::action("Rename Symbol", ed::Rename),
        MenuItem::action("Code Actions", ed::ToggleCodeActions::default()),
        MenuItem::action("Format Document", ed::Format),
        MenuItem::action("Toggle Line Comment", ed::ToggleComments::default()),
        MenuItem::separator(),
        lines_menu(),
        transform_menu(),
        MenuItem::separator(),
        MenuItem::action("Restart Language Servers", forge_languages::language_servers::RestartLanguageServers),
    ])
}

/// Every panel, built in or from an extension, by name. Built-in ones are shown even when
/// hidden from the side bars; Problems is a tab, not a panel.
fn panels_menu(extensions: &ExtensionEntries) -> MenuItem {
    let mut panels: Vec<(String, MenuItem)> = crate::docks::BUILT_IN_PANELS
        .iter()
        .map(|(title, name)| (name.to_string(), MenuItem::action(*name, crate::docks::OpenPanel { title: title.to_string() })))
        .collect();
    panels.push(("Problems".into(), MenuItem::action("Problems", diagnostics::Deploy)));
    for (title, id) in &extensions.panels {
        panels.push((title.clone(), MenuItem::action(title.clone(), forge_extension_host::panel::ShowPanel { id: id.clone() })));
    }
    panels.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    MenuItem::submenu(Menu::new("Panels").items(panels.into_iter().map(|(_, item)| item)))
}

fn editor_layout_items() -> [MenuItem; 4] {
    [
        MenuItem::action("Split Up", workspace::SplitUp::default()),
        MenuItem::action("Split Down", workspace::SplitDown::default()),
        MenuItem::action("Split Left", workspace::SplitLeft::default()),
        MenuItem::action("Split Right", workspace::SplitRight::default()),
    ]
}

fn editor_options() -> MenuItem {
    use editor::actions as ed;
    MenuItem::submenu(Menu::new("Editor Options").items([
        MenuItem::action("Soft Wrap", ed::ToggleSoftWrap),
        MenuItem::action("Line Numbers", ed::ToggleLineNumbers),
        MenuItem::action("Inlay Hints", ed::ToggleInlayHints),
        MenuItem::action("Indent Guides", ed::ToggleIndentGuides),
    ]))
}

fn mac_view_menu(state: &MenuState, panels: MenuItem) -> Menu {
    let show_or_hide = |open: bool, what: &str| format!("{} {what}", if open { "Hide" } else { "Show" });
    Menu::new("View").items([
        MenuItem::action("Command Palette…", zed_actions::command_palette::Toggle),
        MenuItem::separator(),
        panels,
        MenuItem::action("Show or Hide Panels…", crate::docks::ShowOrHidePanels),
        MenuItem::action(show_or_hide(state.left_dock, "Left Sidebar"), workspace::ToggleLeftDock),
        MenuItem::action(show_or_hide(state.right_dock, "Right Sidebar"), workspace::ToggleRightDock),
        MenuItem::action(show_or_hide(state.bottom_dock, "Bottom Panel"), workspace::ToggleBottomDock),
        MenuItem::action("Maximize Panel", workspace::ToggleZoom),
        MenuItem::separator(),
        MenuItem::submenu(Menu::new("Editor Layout").items(editor_layout_items())),
        editor_options(),
        MenuItem::submenu(Menu::new("Appearance").items([
            MenuItem::action("Theme…", zed_actions::theme_selector::Toggle::default()),
            MenuItem::action("Icon Theme…", zed_actions::icon_theme_selector::Toggle::default()),
            MenuItem::action("Color Palettes Folder", OpenPalettesFolder),
        ])),
        MenuItem::separator(),
        MenuItem::action("Zoom In", IncreaseFontSize),
        MenuItem::action("Zoom Out", DecreaseFontSize),
        MenuItem::action("Actual Size", ResetFontSize),
        MenuItem::action("Reset All Zoom", ResetAllZoom),
        MenuItem::separator(),
        MenuItem::action(if state.fullscreen { "Exit Full Screen" } else { "Enter Full Screen" }, ToggleFullScreen),
    ])
}

fn other_view_menu(state: &MenuState, panels: MenuItem) -> Menu {
    let mut layout = vec![
        MenuItem::action("Left Dock", workspace::ToggleLeftDock).checked(state.left_dock),
        MenuItem::action("Right Dock", workspace::ToggleRightDock).checked(state.right_dock),
        MenuItem::action("Bottom Dock", workspace::ToggleBottomDock).checked(state.bottom_dock),
        MenuItem::action("All Docks", workspace::ToggleAllDocks),
        MenuItem::action("Maximize Panel", workspace::ToggleZoom),
        MenuItem::separator(),
    ];
    layout.extend(editor_layout_items());
    Menu::new("View").items([
        MenuItem::action("Command Palette…", zed_actions::command_palette::Toggle),
        MenuItem::separator(),
        panels,
        MenuItem::action("Show or Hide Panels…", crate::docks::ShowOrHidePanels),
        MenuItem::submenu(Menu::new("Layout").items(layout)),
        editor_options(),
        MenuItem::separator(),
        MenuItem::action("Zoom In", IncreaseFontSize),
        MenuItem::action("Zoom Out", DecreaseFontSize),
        MenuItem::action("Reset Zoom", ResetFontSize),
        MenuItem::action("Reset All Zoom", ResetAllZoom),
        MenuItem::separator(),
        MenuItem::action("Full Screen", ToggleFullScreen).checked(state.fullscreen),
    ])
}

fn go_menu() -> Menu {
    use editor::actions as ed;
    Menu::new("Go").items([
        MenuItem::action("Back", workspace::GoBack),
        MenuItem::action("Forward", workspace::GoForward),
        MenuItem::separator(),
        MenuItem::action("Go to File…", workspace::ToggleFileFinder::default()),
        MenuItem::action("Go to Symbol…", zed_actions::outline::ToggleOutline),
        MenuItem::action("Go to Line…", ed::ToggleGoToLine),
        MenuItem::separator(),
        MenuItem::action("Go to Definition", ed::GoToDefinition::default()),
        MenuItem::action("Go to Declaration", ed::GoToDeclaration::default()),
        MenuItem::action("Go to Type Definition", ed::GoToTypeDefinition::default()),
        MenuItem::action("Go to Implementation", ed::GoToImplementation::default()),
        MenuItem::action("Find All References", ed::FindAllReferences::default()),
        MenuItem::action("Type Hierarchy", forge_languages::type_hierarchy::ShowTypeHierarchy),
        MenuItem::separator(),
        MenuItem::action("Next Problem", ed::GoToDiagnostic::default()),
        MenuItem::action("Previous Problem", ed::GoToPreviousDiagnostic::default()),
    ])
}

fn run_menu(state: &MenuState) -> Menu {
    use editor::actions as ed;
    let mut items = vec![
        MenuItem::action("Run", forge_run::Run),
        MenuItem::action("Debug", forge_run::Debug),
        MenuItem::action("Run with Hot Reload", forge_run::Watch),
        MenuItem::action("Stop", forge_run::Stop),
        MenuItem::action("Select What to Run…", SelectRunTarget),
    ];
    if state.has_solution {
        items.extend([
            MenuItem::separator(),
            MenuItem::action("Build", forge_dotnet::explorer::Build),
            MenuItem::action("Rebuild", forge_dotnet::explorer::Rebuild),
            MenuItem::action("Clean", forge_dotnet::explorer::Clean),
        ]);
    }
    items.extend([
        MenuItem::separator(),
        MenuItem::action("Continue", debugger_ui::Continue),
        MenuItem::action("Step Over", debugger_ui::StepOver),
        MenuItem::action("Step Into", debugger_ui::StepInto),
        MenuItem::action("Toggle Breakpoint", ed::ToggleBreakpoint),
        MenuItem::action("Clear All Breakpoints", debugger_ui::ClearAllBreakpoints),
        MenuItem::separator(),
        MenuItem::action("Run All Tests", forge_tests::panel::RunAllTests),
        MenuItem::action("Run All Tests with Coverage", forge_tests::panel::RunAllTestsWithCoverage),
        MenuItem::action("Run Failed Tests", forge_tests::panel::RunFailedTests),
        MenuItem::action("Find Tests Again", forge_tests::panel::RefreshTests),
        MenuItem::action("Clear Coverage", forge_tests::panel::ClearCoverage),
    ]);
    Menu::new("Run").items(items)
}

fn git_menu() -> Menu {
    Menu::new("Git").items([
        MenuItem::action("Changes", zed_actions::git_panel::ToggleFocus),
        MenuItem::action("History", forge_git::history::ToggleHistory),
        MenuItem::action("Project Diff", git_ui::project_diff::Diff),
        MenuItem::separator(),
        MenuItem::action("Commit…", git::Commit),
        MenuItem::action("Write Commit Message with the Agent", forge_agents::commit_message::WriteCommitMessage),
        MenuItem::action("Pull", git::Pull),
        MenuItem::action("Push", git::Push),
        MenuItem::action("Fetch", git::Fetch),
        MenuItem::separator(),
        MenuItem::action("Switch Branch…", zed_actions::git::Branch),
        MenuItem::action("Create Tag…", forge_git::tags::CreateTag),
        MenuItem::action("Stash All", git::StashAll),
        MenuItem::action("Pop Stash", git::StashPop),
        MenuItem::separator(),
        MenuItem::action("Blame Current File", git::Blame),
        MenuItem::action("History of Current File", git::FileHistory),
        MenuItem::submenu(Menu::new("Merge Conflicts").items([
            MenuItem::action("Next Conflict", forge_git::merge_editor::NextConflict),
            MenuItem::action("Previous Conflict", forge_git::merge_editor::PreviousConflict),
            MenuItem::separator(),
            MenuItem::action("Open Merge Editor…", forge_ui::OpenMergeEditor::default()),
            MenuItem::action("Resolve with the Agent", zed_actions::agent::ResolveConflictedFilesWithAgent { conflicted_file_paths: vec![] }),
        ])),
        MenuItem::submenu(Menu::new("GitHub").items([
            MenuItem::action("Pull Requests…", forge_github::PullRequests),
            MenuItem::action("Review This Pull Request with an Agent", forge_github::ReviewPullRequest),
            MenuItem::action("Show Review Comments", forge_github::ShowReviewComments),
        ])),
        MenuItem::separator(),
        MenuItem::action("Initialize Repository", forge_git::init::InitRepository),
        MenuItem::action("Sign Off Commits in This Project", forge_git::signoff::ToggleSignOff),
    ])
}

fn agents_menu() -> Menu {
    Menu::new("Agents").items([
        MenuItem::action("New Thread", forge_agents::NewThread),
        MenuItem::action("New Thread in Worktree", forge_agents::NewThreadInWorktree),
        MenuItem::action("Go to Thread…", forge_agents::OpenThreads),
        MenuItem::action("Worktrees…", forge_agents::ManageWorktrees),
        MenuItem::separator(),
        MenuItem::action("Ask About This Code", forge_agents::AskHere),
        MenuItem::action("Ask About the Terminal Output", forge_agents::context::AskAboutTerminal),
        MenuItem::action("Fix the Problem at the Cursor", forge_agents::FixProblemAtCursor),
        MenuItem::action("Fix the Problems in This File", forge_agents::FixProblemsInFile),
        MenuItem::action("Fix the Problems in the Project", forge_agents::FixProblemsInProject),
        MenuItem::separator(),
        MenuItem::action("Keep Change", forge_agents::agent_review::KeepAgentChange),
        MenuItem::action("Undo Change", forge_agents::agent_review::UndoAgentChange),
        MenuItem::action("Stop the Agent", forge_agents::Cancel),
        MenuItem::separator(),
        MenuItem::action("Agent Settings…", forge_agents::OpenAgentSettings),
        MenuItem::submenu(Menu::new("Instructions").items([
            MenuItem::action("Edit Project Instructions", OpenProjectInstructions),
            MenuItem::action("Edit Your Instructions", OpenUserInstructions),
        ])),
        MenuItem::action("Set Up Forge with an Agent…", forge_agents::ConfigureForge),
    ])
}

fn tools_menu(platform: MenuPlatform, state: &MenuState, extensions: &ExtensionEntries) -> Menu {
    let mut items = vec![
        MenuItem::action("New Terminal", workspace::NewTerminal::default()),
        MenuItem::action("Terminal Panel", terminal_view::terminal_panel::ToggleFocus),
        MenuItem::separator(),
        MenuItem::submenu(Menu::new("HTTP Requests").items([
            MenuItem::action("Send Request", forge_http::SendRequest::default()),
            MenuItem::action("Cancel Request", forge_http::CancelRequest),
            MenuItem::action("Request History…", forge_http::RequestHistory),
            MenuItem::action("Environment…", forge_http::SelectEnvironment),
        ])),
    ];
    if state.has_solution {
        items.push(MenuItem::submenu(Menu::new(".NET").items([
            MenuItem::action("Manage NuGet Packages", forge_dotnet::explorer::ManagePackages),
            MenuItem::action("Code Analysis", forge_dotnet::code_analysis::OpenCodeAnalysis),
            MenuItem::action("Open Aspire Dashboard", forge_run::OpenDashboard),
            MenuItem::separator(),
            MenuItem::action("Restore", forge_dotnet::explorer::Restore),
            MenuItem::action("Pack", forge_dotnet::explorer::Pack),
            MenuItem::action("Publish", forge_dotnet::explorer::Publish),
        ])));
    }
    items.extend([
        MenuItem::separator(),
        MenuItem::submenu(Menu::new("Extensions").items([
            MenuItem::action("Extensions Panel", forge_extension_host::panel::ToggleFocus),
            MenuItem::separator(),
            MenuItem::action("Install from Package…", forge_extension_host::install::InstallFromPackage),
            MenuItem::action("Install from Folder…", forge_extension_host::install::InstallFromFolder),
            MenuItem::action("Reveal Extensions Folder", OpenExtensionsFolder),
        ])),
    ]);
    for (name, commands) in &extensions.commands {
        let commands: Vec<MenuItem> = commands.iter().map(|(title, id)| MenuItem::action(title.clone(), forge_extension_host::commands::RunCommand { id: id.clone() })).collect();
        items.push(MenuItem::submenu(Menu::new(name.clone()).items(commands)));
    }
    items.extend([MenuItem::separator(), MenuItem::action("Check Tools…", forge_toolchain::CheckTools)]);
    if platform != MenuPlatform::Mac {
        items.push(MenuItem::action("Restart Language Servers", forge_languages::language_servers::RestartLanguageServers));
    }
    Menu::new("Tools").items(items)
}

fn window_menu() -> Menu {
    Menu::new("Window").items([
        MenuItem::action("Minimize", Minimize),
        MenuItem::action("Zoom", Zoom),
        MenuItem::separator(),
        MenuItem::action("Bring All to Front", BringAllToFront),
    ])
}

fn help_references() -> [MenuItem; 2] {
    [
        MenuItem::action("Agent Client Protocol", OpenBrowser { url: "https://agentclientprotocol.com".into() }),
        MenuItem::action("Editor Features Reference", OpenBrowser { url: "https://zed.dev/docs".into() }),
    ]
}

fn mac_help_menu() -> Menu {
    let mut items = vec![
        MenuItem::action("Forge Guide", OpenGuide),
        MenuItem::action("Welcome", ShowWelcome),
        MenuItem::action("Key Bindings Reference", OpenDefaultKeymap),
        MenuItem::separator(),
    ];
    items.extend(help_references());
    Menu::new("Help").items(items)
}

fn other_help_menu() -> Menu {
    let mut items = vec![
        MenuItem::action("Welcome", ShowWelcome),
        MenuItem::action("Forge Guide", OpenGuide),
        MenuItem::action("Key Bindings Reference", OpenDefaultKeymap),
        MenuItem::separator(),
    ];
    items.extend(help_references());
    items.extend([
        MenuItem::separator(),
        MenuItem::action("Check for Updates…", forge_update::CheckForUpdates),
        MenuItem::action("About Forge", About),
    ]);
    Menu::new("Help").items(items)
}

/// Lets you choose what Run and Debug start (like the picker in the title bar).
fn select_run_target(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(controller) = forge_run::RunController::for_workspace(&cx.entity(), cx) else { return };
    let targets = controller.read(cx).targets().to_vec();
    if targets.is_empty() {
        return;
    }
    let choices = targets.iter().map(|t| forge_ui::pick::Choice::new(t.name.clone()).detail(t.kind.label())).collect();
    forge_ui::pick::pick(workspace, "What Run and Debug start", choices, window, cx, move |index, _, cx| {
        if let Some(target) = targets.get(index) {
            controller.update(cx, |c, cx| c.select(target.id(), cx));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    fn actions_in(items: &[MenuItem], out: &mut Vec<(String, Box<dyn Action>)>) {
        for item in items {
            match item {
                MenuItem::Action { name, action, .. } => out.push((name.to_string(), action.boxed_clone())),
                MenuItem::Submenu(menu) => actions_in(&menu.items, out),
                _ => {}
            }
        }
    }

    fn titles(items: &[MenuItem]) -> Vec<String> {
        items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action { name, .. } => Some(name.to_string()),
                MenuItem::Submenu(menu) => Some(format!("{} ›", menu.name)),
                _ => None,
            })
            .collect()
    }

    fn submenu<'a>(items: &'a [MenuItem], name: &str) -> &'a [MenuItem] {
        items.iter().find_map(|item| match item {
            MenuItem::Submenu(menu) if menu.name == name => Some(menu.items.as_slice()),
            _ => None,
        }).unwrap_or_else(|| panic!("no submenu {name}"))
    }

    /// An extension's panels are in View › Panels and its commands in Tools › <its
    /// name>, as soon as it registers them; the items show the panel and run the command.
    #[gpui::test]
    async fn extensions_have_their_panels_and_commands_in_the_menus(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            workspace::init(params.clone(), cx);
            forge_extension_host::panel::init(cx);
            forge_extension_host::commands::init(cx);
        });
        let tmp = tempfile::tempdir().unwrap();
        let ext = tmp.path().join("menu-ext");
        std::fs::create_dir_all(ext.join("dist")).unwrap();
        std::fs::write(ext.join("package.json"), r#"{"name":"menu-ext","displayName":"Menu Test","forge":{}}"#).unwrap();
        std::fs::write(
            ext.join("dist/extension.js"),
            "var __forgeExtension = { activate() { const f = __forge.modules['@forge-ide/api'].forge; \
             f.panels.register({ id: 'menu-ext.panel', title: 'Gadgets', render: () => null }); \
             f.commands.register('menu-ext.hello', 'Say Hello', () => f.commands.register('menu-ext.said', 'Said', () => {})); } };",
        )
        .unwrap();
        let host = cx.update(|cx| forge_extension_host::ExtensionHost::init(vec![tmp.path().to_path_buf()], cx)).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while cx.update(|cx| !extension_menus_signature(cx).contains("Say Hello") || !extension_menus_signature(cx).contains("Gadgets")) {
            assert!(std::time::Instant::now() < deadline, "the extension never registered");
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let menus = cx.update(|cx| app_menus(cx));
        let view = &menus.iter().find(|m| m.name == "View").unwrap().items;
        let panels = titles(submenu(view, "Panels"));
        assert!(panels.contains(&"Gadgets".to_string()), "{panels:?}");
        let mut sorted = panels.clone();
        sorted.sort_by_key(|t| t.to_lowercase());
        assert_eq!(panels, sorted, "panels are listed by name");
        let tools = &menus.iter().find(|m| m.name == "Tools").unwrap().items;
        assert_eq!(titles(submenu(tools, "Menu Test")), ["Say Hello"]);
        assert!(titles(submenu(tools, "Extensions")).contains(&"Install from Package…".to_string()));

        // The items work: the command runs, and the panel's slot shows it.
        let project = project::Project::test(params.fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        workspace.update_in(cx, |ws, window, cx| forge_extension_host::panel::add_panels(host.clone(), ws, window, cx));
        cx.dispatch_action(forge_extension_host::commands::RunCommand { id: "menu-ext.hello".into() });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !host.read_with(cx, |h, _| h.commands.iter().any(|c| c.id == "menu-ext.said")) {
            assert!(std::time::Instant::now() < deadline, "the command never ran");
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        cx.dispatch_action(forge_extension_host::panel::ShowPanel { id: "menu-ext.panel".into() });
        cx.run_until_parked();
        let slot = host.read_with(cx, |h, _| h.slot_of("menu-ext.panel")).unwrap();
        let open = workspace.read_with(cx, |ws, cx| {
            [ws.left_dock(), ws.right_dock(), ws.bottom_dock()].iter().any(|dock| {
                let dock = dock.read(cx);
                dock.is_open() && dock.visible_panel().is_some_and(|p| p.persistent_name() == forge_extension_host::panel::SLOT_KEYS[slot])
            })
        });
        assert!(open, "Show Panel opens the panel's slot");
    }

    fn menu<'a>(menus: &'a [Menu], name: &str) -> &'a [MenuItem] {
        &menus.iter().find(|m| m.name == name).unwrap_or_else(|| panic!("no menu {name}")).items
    }

    /// Each platform has the menus its apps have.
    #[gpui::test]
    async fn each_platform_has_its_own_menus(cx: &mut TestAppContext) {
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            workspace::init(params.clone(), cx);
        });
        let names = |menus: &[Menu]| menus.iter().map(|m| m.name.to_string()).collect::<Vec<_>>();
        let state = MenuState::default();

        let mac = cx.update(|cx| app_menus_for(MenuPlatform::Mac, &state, cx));
        assert_eq!(names(&mac), ["Forge", "File", "Edit", "Code", "View", "Go", "Run", "Git", "Agents", "Tools", "Window", "Help"]);
        let about = titles(menu(&mac, "Forge"));
        for item in ["About Forge", "Check for Updates…", "Settings…", "Hide Forge", "Quit Forge"] {
            assert!(about.contains(&item.to_string()), "{item} in {about:?}");
        }
        assert!(titles(menu(&mac, "Edit")).contains(&"Select All".to_string()));
        assert!(!titles(menu(&mac, "Help")).contains(&"About Forge".to_string()), "About is in the Forge menu");
        assert!(titles(menu(&mac, "View")).contains(&"Enter Full Screen".to_string()));

        let linux = cx.update(|cx| app_menus_for(MenuPlatform::Linux, &state, cx));
        assert_eq!(names(&linux), ["File", "Edit", "View", "Go", "Run", "Git", "Agents", "Tools", "Help"]);
        let edit = titles(menu(&linux, "Edit"));
        assert!(edit.contains(&"Select All".to_string()) && edit.contains(&"Selection ›".to_string()), "Edit holds the selection: {edit:?}");
        assert!(titles(menu(&linux, "File")).contains(&"Quit".to_string()));
        assert!(titles(submenu(menu(&linux, "File"), "Preferences")).contains(&"Settings…".to_string()));
        assert!(titles(menu(&linux, "Help")).contains(&"About Forge".to_string()));

        let windows = cx.update(|cx| app_menus_for(MenuPlatform::Windows, &state, cx));
        assert!(titles(menu(&windows, "File")).contains(&"Exit".to_string()));

        // What only applies with a solution open, and what shows the state.
        assert!(!titles(menu(&linux, "Run")).contains(&"Build".to_string()));
        assert!(!titles(menu(&linux, "Tools")).contains(&".NET ›".to_string()));
        let solution = MenuState { has_solution: true, fullscreen: true, left_dock: true, ..Default::default() };
        let mac = cx.update(|cx| app_menus_for(MenuPlatform::Mac, &solution, cx));
        assert!(titles(menu(&mac, "Run")).contains(&"Build".to_string()));
        assert!(titles(menu(&mac, "Tools")).contains(&".NET ›".to_string()));
        assert!(titles(menu(&mac, "View")).contains(&"Exit Full Screen".to_string()));
        assert!(titles(menu(&mac, "View")).contains(&"Hide Left Sidebar".to_string()));
        assert!(titles(menu(&mac, "View")).contains(&"Show Right Sidebar".to_string()));
    }

    /// File › Open Recent lists the projects, with the way to the rest.
    #[gpui::test]
    async fn open_recent_lists_the_projects(cx: &mut TestAppContext) {
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            workspace::init(params.clone(), cx);
        });
        let state = MenuState { recent: vec![crate::welcome::Recent { paths: vec![PathBuf::from("/code/forge")], opened_at: 1 }], ..Default::default() };
        let mac = cx.update(|cx| app_menus_for(MenuPlatform::Mac, &state, cx));
        let recent = titles(submenu(menu(&mac, "File"), "Open Recent"));
        assert!(recent[0].starts_with("forge — "), "{recent:?}");
        assert_eq!(recent[1..], ["More…", "Clear Menu"]);
        let linux = cx.update(|cx| app_menus_for(MenuPlatform::Linux, &MenuState::default(), cx));
        assert_eq!(titles(submenu(menu(&linux, "File"), "Open Recent")), ["No Recent Projects", "More…"]);
    }

    /// Every menu item must be backed by a handler: with an editor focused in a workspace
    /// window, no item may come out disabled.
    #[gpui::test]
    async fn every_menu_item_has_a_handler(cx: &mut TestAppContext) {
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            workspace::init(params.clone(), cx);
            command_palette::init(cx);
            search::init(cx);
            file_finder::init(cx);
            go_to_line::init(cx);
            project_panel::init(cx);
            terminal_view::init(cx);
            theme_selector::init(cx);
            diagnostics::init(cx);
            outline::init(cx);
            forge_agents::init(cx);
            forge_extension_host::panel::init(cx);
            forge_extension_host::install::init(cx);
            forge_http::init(cx);
            forge_github::init(cx);
            forge_update::init(cx);
            forge_git::init(cx);
            forge_dotnet::init(cx);
            forge_toolchain::init(cx);
            // The app runs it from `forge_languages::init`, which needs the language registry.
            forge_languages::type_hierarchy::init(cx);
            forge_languages::language_servers::init(cx);
            init(cx);
            crate::open_editors::init(cx);
            crate::docks::init(cx);
            crate::settings_view::init(cx);
            forge_agents::commit_message::init(cx);
            forge_agents::context::init(cx);
            forge_tests::panel::init(cx);
            forge_run::init(cx);
            forge_output::init(cx);
            debugger_ui::init(cx);
            crate::titlebar::init(cx);
            crate::windows::init(cx);
            crate::remote::init(cx);
        });
        let project = project::Project::test(params.fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.dispatch_action(workspace::NewFile);
        cx.run_until_parked();

        let state = MenuState { has_solution: true, ..Default::default() };
        let mut actions = Vec::new();
        for platform in [MenuPlatform::Mac, MenuPlatform::Linux, MenuPlatform::Windows] {
            for menu in cx.update(|_, cx| app_menus_for(platform, &state, cx)) {
                actions_in(&menu.items, &mut actions);
            }
        }
        assert!(actions.len() > 80, "the menus are extensive ({} items)", actions.len());
        // Handled only while their target exists: File History needs a file of a repository
        // open; Clear All Breakpoints, the debug panel (loaded with the app's panels); stepping,
        // a debug session stopped; keeping or undoing, an agent's change at the cursor; the
        // conflicts, a merge editor.
        let needs_context = [
            "git::FileHistory",
            "debugger::ClearAllBreakpoints",
            "debugger::Continue",
            "debugger::StepOver",
            "debugger::StepInto",
            "agents::KeepAgentChange",
            "agents::UndoAgentChange",
            "git::NextConflict",
            "git::PreviousConflict",
        ];
        // Only registered on macOS, whose menus are the only ones with them.
        let mac_only = ["forge::Hide", "forge::HideOthers", "forge::ShowAll", "forge::BringAllToFront"];
        let unavailable: Vec<String> = actions
            .iter()
            .filter(|(_, action)| !needs_context.contains(&action.name()) && !(mac_only.contains(&action.name()) && !cfg!(target_os = "macos")))
            .filter(|(_, action)| !cx.update(|window, cx| window.is_action_available(&**action, cx) || cx.is_action_available(&**action)))
            .map(|(name, action)| format!("{name} ({})", action.name()))
            .collect();
        assert!(unavailable.is_empty(), "menu items without a handler: {unavailable:#?}");

        // Help → Welcome opens Forge's page (once), not Zed's.
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();

        // The workspace draws Forge's title bar (room for the traffic lights).
        let has_title_bar = workspace.read_with(cx, |ws, _| ws.titlebar_item().is_some_and(|item| item.downcast::<crate::titlebar::ForgeTitleBar>().is_ok()));
        assert!(has_title_bar);
        for _ in 0..2 {
            cx.dispatch_action(ShowWelcome);
            cx.run_until_parked();
        }
        let pages = workspace.read_with(cx, |ws, cx| ws.active_pane().read(cx).items_of_type::<crate::welcome::ForgeWelcome>().count());
        assert_eq!(pages, 1);

        // Nothing Zed-branded is offered in the command palette.
        let visible: Vec<String> = cx.update(|window, cx| {
            let filter = command_palette_hooks::CommandPaletteFilter::try_global(cx);
            window
                .available_actions(cx)
                .into_iter()
                .filter(|a| !filter.is_some_and(|f| f.is_hidden(&**a)))
                .map(|a| command_palette::humanize_action_name(a.name()))
                .collect()
        });
        let zed_branded: Vec<&String> = visible.iter().filter(|n| n.to_lowercase().split(|c: char| !c.is_alphanumeric()).any(|w| w == "zed")).collect();
        assert!(zed_branded.is_empty(), "Zed-branded commands in the palette: {zed_branded:?}");
        assert!(visible.iter().any(|n| n == "forge: increase font size"), "Forge equivalents are offered");
        assert!(visible.iter().any(|n| n == "languages: restart language servers"), "language servers can be restarted");
        assert!(!visible.iter().any(|n| n.starts_with("forge ")), "commands are named by what they're about: {visible:?}");
        if !cfg!(target_os = "macos") {
            for system_only in ["forge: hide", "forge: hide others", "forge: show all"] {
                assert!(!visible.iter().any(|n| n == system_only), "{system_only} can't work on this platform");
            }
        }
        for hidden in ["client: sign in", "multi workspace: next thread"] {
            assert!(!visible.iter().any(|n| n == hidden), "{hidden} should be hidden");
        }
    }
}
