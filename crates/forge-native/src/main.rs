//! Forge native shell: a GPUI application built on Zed's workspace, editor and terminal.
//!
//! This is deliberately a *slim* subset of Zed's own `main`: no collaboration, Zed AI,
//! telemetry, auto-update or extension marketplace. Forge-specific surfaces (ACP agent
//! panel, React extension host) are added as workspace panels.

mod branding;
mod config_files;
mod docks;
mod external_changes;
mod menus;
mod settings_view;
mod open_editors;
mod statusbar;
mod theme;
mod titlebar;
mod welcome;

use ::theme::ActiveTheme as _;
use anyhow::Context as _;
use assets::Assets;
use client::{Client, UserStore};
use db::kvp::KeyValueStore;
use fs::{Fs, RealFs};
use gpui::TaskExt as _;
use gpui::{App, AppContext as _, Application, QuitMode, UpdateGlobal as _, actions};
use gpui_tokio::Tokio;
use language::LanguageRegistry;
use node_runtime::{NodeBinaryOptions, NodeRuntime};
use project_panel::ProjectPanel;
use reqwest_client::ReqwestClient;
use session::{AppSession, Session};
use settings::{KeybindSource, KeymapFile, SettingsStore};
use std::{path::PathBuf, sync::Arc};
use terminal_view::terminal_panel::TerminalPanel;
use util::ResultExt as _;
use workspace::{AppState, OpenOptions, Workspace, WorkspaceStore};

actions!(forge, [Quit]);

fn main() {
    // Zed captures the login-shell environment by re-running the current executable with
    // `--printenv` from inside that shell; answer like Zed does instead of opening a window.
    if std::env::args().nth(1).as_deref() == Some("--printenv") {
        util::shell_env::print_env();
        return;
    }

    // Same filtering and stderr output as before; the Output panel also gets a copy.
    forge_output::app_log::install(env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")));

    // Keep Forge's settings, database and caches apart from an installed Zed.
    let data_dir = dirs_data_dir().join("Forge");
    paths::set_custom_data_dir(&data_dir.to_string_lossy());

    let paths = startup_paths(std::env::args().skip(1), std::env::current_dir().ok());

    let app = Application::with_platform(gpui_platform::current_platform(false)).with_assets(branding::ForgeAssets);
    let app_db = db::AppDatabase::new();
    let session = app
        .background_executor()
        .spawn(Session::new(uuid::Uuid::new_v4().to_string(), KeyValueStore::from_app_db(&app_db)));
    let fs: Arc<dyn Fs> = RealFs::new(None, app.background_executor());
    // Files/folders dropped on the Dock icon or opened with "Open With → Forge".
    let (open_tx, mut open_rx) = futures::channel::mpsc::unbounded::<Vec<PathBuf>>();
    app.on_open_urls(move |urls| {
        let paths: Vec<PathBuf> = urls.iter().filter_map(|u| url::Url::parse(u).ok()?.to_file_path().ok()).collect();
        if !paths.is_empty() {
            let _ = open_tx.unbounded_send(paths);
        }
    });

    app.run(move |cx| {
        cx.set_global(app_db);
        menu::init();
        zed_actions::init();
        release_channel::init(semver(env!("CARGO_PKG_VERSION")), cx);
        gpui_tokio::init(cx);
        settings::init(cx);
        apply_forge_defaults(cx);
        config_files::ensure();
        watch_settings(fs.clone(), cx);

        let http = {
            let _guard = Tokio::handle(cx).enter();
            ReqwestClient::user_agent(&format!("Forge/{} ({})", env!("CARGO_PKG_VERSION"), std::env::consts::OS))
                .expect("could not start HTTP client")
        };
        cx.set_http_client(Arc::new(http));
        <dyn Fs>::set_global(fs.clone(), cx);

        let client = Client::production(cx);
        let mut languages = LanguageRegistry::new(cx.background_executor().clone());
        languages.set_language_server_download_dir(paths::languages_dir().clone());
        let languages = Arc::new(languages);
        let (mut node_tx, node_rx) = watch::channel(None);
        node_tx.send(Some(NodeBinaryOptions { allow_path_lookup: true, allow_binary_download: true, use_paths: None })).log_err();
        let node_runtime = NodeRuntime::new(client.http_client(), None, node_rx);
        languages::init(languages.clone(), fs.clone(), node_runtime.clone(), cx);
        forge_languages::init(languages.clone(), cx);
        let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
        let workspace_store = cx.new(|cx| WorkspaceStore::new(client.clone(), cx));
        Client::set_global(client.clone(), cx);
        project::Project::init(&client, cx);
        client::init(&client, cx);

        let session = cx.foreground_executor().block_on(session);
        let app_state = Arc::new(AppState {
            languages,
            client,
            user_store,
            workspace_store,
            fs: fs.clone(),
            build_window_options,
            node_runtime,
            session: cx.new(|cx| AppSession::new(session, cx)),
        });
        AppState::set_global(app_state.clone(), cx);

        theme_settings::init(::theme::LoadThemes::All(Box::new(Assets)), cx);
        theme::init(fs.clone(), cx);
        theme::connect_syntax_highlighting(app_state.languages.clone(), cx);
        editor::init(cx);
        // An empty model registry: Zed's git panel looks it up (commit messages), but
        // Forge adds no models — its agents come through ACP.
        language_model::init(cx);
        workspace::init(app_state.clone(), cx);
        command_palette::init(cx);
        go_to_line::init(cx);
        file_finder::init(cx);
        search::init(cx);
        project_panel::init(cx);
        terminal_view::init(cx);
        dap_adapters::init(cx);
        debugger_ui::init(cx);
        theme_selector::init(cx);
        diagnostics::init(cx);
        outline::init(cx);
        forge_extension_host::init(cx);
        forge_agents::init(cx);
        forge_tests::init(cx);
        forge_run::init(cx);
        forge_http::init(cx);
        forge_github::init(cx);
        forge_update::init(cx);
        forge_output::init(cx);
        forge_git::init(cx);
        forge_dotnet::init(cx);
        menus::init(cx);
        open_editors::init(cx);
        docks::init(cx);
        settings_view::init(cx);
        external_changes::init(cx);
        titlebar::init(cx);
        cx.set_menus(menus::app_menus(cx));
        // Extension panels and commands appear in the menus: set them again when they change.
        if let Some(host) = forge_extension_host::ExtensionHost::global(cx) {
            let mut signature = menus::extension_menus_signature(cx);
            cx.observe(&host, move |_, cx| {
                let now = menus::extension_menus_signature(cx);
                if now != signature {
                    signature = now;
                    cx.set_menus(menus::app_menus(cx));
                }
            })
            .detach();
        }

        load_keymap(cx);
        cx.set_quit_mode(QuitMode::LastWindowClosed);
        add_panels_to_new_workspaces(cx);

        if paths.is_empty() {
            // Launched from Finder/Dock without a project: the windows of the last session,
            // as they were; otherwise an empty window (⌘O opens a folder).
            let app_state = app_state.clone();
            cx.spawn(async move |cx| {
                if !restore_last_session(&app_state, cx).await {
                    let open = cx.update(|cx| {
                        workspace::open_new(OpenOptions::default(), app_state.clone(), cx, |ws, window, cx| welcome::show(ws, window, cx))
                    });
                    open.await.context("failed to open the initial window").log_err();
                }
            })
            .detach();
        } else {
            let open = workspace::open_paths(&paths, app_state.clone(), OpenOptions::default(), cx);
            cx.spawn(async move |_| {
                open.await.context("failed to open the initial window").log_err();
            })
            .detach();
        }
        cx.spawn(async move |cx| {
            use futures::StreamExt as _;
            while let Some(paths) = open_rx.next().await {
                let open = cx.update(|cx| workspace::open_paths(&paths, app_state.clone(), OpenOptions::default(), cx));
                open.await.with_context(|| format!("failed to open {paths:?}")).log_err();
            }
        })
        .detach();
        cx.activate(true);
    });
}

/// Reopens the windows that were open when Forge last ended (quit, or the last window
/// closed), each with its tabs, splits and docks; `restore_on_startup` in settings.json
/// chooses that, only the last project ("last_workspace") or nothing ("empty_tab"). Returns
/// whether any window opened.
async fn restore_last_session(app_state: &Arc<AppState>, cx: &mut gpui::AsyncApp) -> bool {
    use settings::Settings as _;
    use workspace::{RestoreOnStartupBehavior, WorkspaceSettings};
    let (behavior, db, last_session) = cx.update(|cx| {
        let session = app_state.session.read(cx);
        (
            WorkspaceSettings::get_global(cx).restore_on_startup,
            workspace::WorkspaceDb::global(cx),
            session.last_session_id().map(|id| (id.to_string(), session.last_session_window_stack())) as Option<(String, Option<Vec<gpui::WindowId>>)>,
        )
    });
    let locations = match (behavior, last_session) {
        (RestoreOnStartupBehavior::LastSession, Some((id, window_stack))) => {
            let ordered = window_stack.is_some();
            let mut locations = workspace::last_session_workspace_locations(&db, &id, window_stack, app_state.fs.as_ref()).await.unwrap_or_default();
            // Front-to-back: open the frontmost window last, so it ends up in front.
            if ordered {
                locations.reverse();
            }
            locations
        }
        (RestoreOnStartupBehavior::EmptyTab | RestoreOnStartupBehavior::Launchpad, _) => Vec::new(),
        _ => workspace::last_opened_workspace_location(&db, app_state.fs.as_ref())
            .await
            .map(|(workspace_id, location, paths)| vec![workspace::SessionWorkspace { workspace_id, location, paths, window_id: None }])
            .unwrap_or_default(),
    };
    let locations: Vec<_> = locations.into_iter().filter(|l| matches!(l.location, workspace::SerializedWorkspaceLocation::Local)).collect();
    if locations.is_empty() {
        return false;
    }
    let multi_workspaces = cx.update(|cx| workspace::read_serialized_multi_workspaces(locations, cx));
    let mut restored = false;
    for multi_workspace in multi_workspaces {
        match workspace::restore_multiworkspace(multi_workspace, app_state.clone(), cx).await {
            Ok(_) => restored = true,
            Err(error) => log::error!("failed to restore a window: {error:#}"),
        }
    }
    restored
}

/// `forge [PATH...]`: folders become worktrees, files open in editors. With no paths, a
/// terminal launch opens the current directory; a Finder launch (cwd `/`) opens an empty
/// window. macOS may add a `-psn_…` argument to GUI launches; it is ignored.
fn startup_paths(args: impl Iterator<Item = String>, cwd: Option<PathBuf>) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = args.filter(|a| !a.starts_with("-psn_")).map(PathBuf::from).collect();
    if paths.is_empty() {
        paths.extend(cwd.filter(|d| d.as_os_str() != "/"));
    }
    paths.into_iter().map(|p| p.canonicalize().unwrap_or(p)).collect()
}

fn dirs_data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    if cfg!(target_os = "macos") { home.join("Library/Application Support") } else { home.join(".local/share") }
}

fn semver(v: &str) -> semver::Version {
    v.parse().expect("CARGO_PKG_VERSION is semver")
}

/// Layers `assets/forge-defaults.json` over Zed's defaults, so Forge ships its own look and
/// opinions while users still override everything in their settings.json.
fn apply_forge_defaults(cx: &mut App) {
    let result = default_settings_value().and_then(|defaults| SettingsStore::update_global(cx, |store, cx| store.set_default_settings(&defaults.to_string(), cx)));
    result.context("failed to apply Forge default settings").log_err();
}

/// The built-in defaults with Forge's on top: what a setting is when settings.json
/// doesn't set it.
pub fn default_settings_value() -> anyhow::Result<serde_json::Value> {
    fn merge(base: &mut serde_json::Value, overlay: &serde_json::Value) {
        match (base, overlay) {
            (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
                for (k, v) in o {
                    merge(b.entry(k.clone()).or_insert(serde_json::Value::Null), v);
                }
            }
            (b, o) => *b = o.clone(),
        }
    }
    let mut defaults: serde_json::Value = serde_json_lenient::from_str(&settings::default_settings())?;
    let forge: serde_json::Value = serde_json_lenient::from_str(include_str!("../assets/forge-defaults.json"))?;
    merge(&mut defaults, &forge);
    Ok(defaults)
}

fn watch_settings(fs: Arc<dyn Fs>, cx: &mut App) {
    SettingsStore::update_global(cx, move |store, cx| {
        store.watch_settings_files(fs, cx, |_, result, _| {
            if let settings::ParseStatus::Failed { error } = result.parse_status {
                log::error!("settings error: {error}");
            }
        });
    });
}

fn load_keymap(cx: &mut App) {
    let default = KeymapFile::load_asset_allow_partial_failure(settings::DEFAULT_KEYMAP_PATH, cx).unwrap_or_default();
    let mut bindings = default;
    for b in &mut bindings {
        b.set_meta(KeybindSource::Default.meta());
    }
    cx.bind_keys(bindings);
    cx.bind_keys([gpui::KeyBinding::new("cmd-q", Quit, None), gpui::KeyBinding::new("cmd-,", settings_view::OpenSettings, None)]);
    forge_agents::bind_keys(cx);
    // ⌘. in a project file (no language server there): the package's other versions.
    for extension in ["csproj", "fsproj", "vbproj", "props", "targets", "proj"] {
        cx.bind_keys([gpui::KeyBinding::new("cmd-.", forge_dotnet::project_files::ChangePackageVersion, Some(&format!("Editor && extension == {extension}")))]);
    }
}

/// Mirrors Zed's `initialize_panels`, restricted to the panels Forge ships.
fn add_panels_to_new_workspaces(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        // Forge shows its own welcome page instead of Zed's.
        workspace.active_pane().update(cx, |pane, _| pane.set_should_display_welcome_page(false));
        cx.spawn_in(window, async move |workspace, cx| {
            let project_panel = ProjectPanel::load(workspace.clone(), cx.clone()).await?;
            let terminal_panel = TerminalPanel::load(workspace.clone(), cx.clone()).await?;
            let debug_panel = debugger_ui::debugger_panel::DebugPanel::load(workspace.clone(), &mut cx.clone()).await?;
            let git_panel = git_ui::git_panel::GitPanel::load(workspace.clone(), cx.clone()).await?;
            workspace.update_in(cx, |workspace, window, cx| {
                use std::{rc::Rc, sync::Arc};
                workspace.add_panel(project_panel.clone(), window, cx);
                workspace.add_panel(terminal_panel.clone(), window, cx);
                workspace.add_panel(debug_panel.clone(), window, cx);
                let solution_explorer = cx.new(|cx| forge_dotnet::SolutionExplorer::new(workspace, window, cx));
                workspace.add_panel(solution_explorer.clone(), window, cx);
                let threads_panel = cx.new(|cx| forge_agents::ThreadsPanel::new(workspace, window, cx));
                workspace.add_panel(threads_panel.clone(), window, cx);
                let test_panel = cx.new(|cx| forge_tests::TestPanel::new(workspace, window, cx));
                workspace.add_panel(test_panel.clone(), window, cx);
                let output_panel = cx.new(|cx| forge_output::OutputPanel::new(workspace, window, cx));
                workspace.add_panel(output_panel.clone(), window, cx);
                workspace.add_panel(git_panel.clone(), window, cx);
                let history_panel = cx.new(|cx| forge_git::HistoryPanel::new(workspace, window, cx));
                workspace.add_panel(history_panel.clone(), window, cx);
                let open_editors = cx.new(|cx| open_editors::OpenEditorsPanel::new(workspace, window, cx));
                workspace.add_panel(open_editors.clone(), window, cx);
                let mut entries = vec![
                    docks::DockEntry { handle: Arc::new(project_panel), title: Rc::new(|_| "Project".into()) },
                    docks::DockEntry { handle: Arc::new(open_editors), title: Rc::new(|_| "Open Editors".into()) },
                    docks::DockEntry { handle: Arc::new(solution_explorer), title: Rc::new(|_| "Solution".into()) },
                    docks::DockEntry { handle: Arc::new(terminal_panel), title: Rc::new(|_| "Terminal".into()) },
                    docks::DockEntry { handle: Arc::new(debug_panel), title: Rc::new(|_| "Debug".into()) },
                    docks::DockEntry { handle: Arc::new(threads_panel), title: Rc::new(|_| "Threads".into()) },
                    docks::DockEntry { handle: Arc::new(test_panel), title: Rc::new(|_| "Tests".into()) },
                    docks::DockEntry { handle: Arc::new(output_panel), title: Rc::new(|_| "Output".into()) },
                    docks::DockEntry { handle: Arc::new(git_panel), title: Rc::new(|_| "Git".into()) },
                    docks::DockEntry { handle: Arc::new(history_panel), title: Rc::new(|_| "History".into()) },
                ];
                let host = forge_extension_host::ExtensionHost::global(cx);
                if let Some(host) = host.clone() {
                    entries.extend(forge_extension_host::panel::add_panels(host, workspace, window, cx));
                }
                docks::install(workspace, entries, host, window, cx);
                statusbar::install(workspace, window, cx);
                let empty = workspace.visible_worktrees(cx).next().is_none() && workspace.active_pane().read(cx).items_len() == 0;
                if empty {
                    welcome::show(workspace, window, cx);
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    })
    .detach();
}

fn build_window_options(display_uuid: Option<uuid::Uuid>, cx: &mut App) -> gpui::WindowOptions {
    use gpui::{TitlebarOptions, point, px};
    let display = display_uuid.and_then(|uuid| cx.displays().into_iter().find(|d| d.uuid().ok() == Some(uuid)));
    gpui::WindowOptions {
        titlebar: Some(TitlebarOptions { title: Some("Forge".into()), appears_transparent: true, traffic_light_position: Some(point(px(9.0), px(9.0))) }),
        focus: false,
        show: false,
        display_id: display.map(|d| d.id()),
        window_background: cx.theme().window_background_appearance(),
        app_id: Some("dev.forge.ide".into()),
        window_min_size: Some(gpui::Size { width: px(360.0), height: px(240.0) }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_paths_handle_terminal_and_finder_launches() {
        let args = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter();
        assert_eq!(startup_paths(args(&[]), Some("/".into())), Vec::<PathBuf>::new(), "Finder launch");
        assert_eq!(startup_paths(args(&["-psn_0_12345"]), Some("/".into())), Vec::<PathBuf>::new());
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(startup_paths(args(&[]), Some(tmp.clone())), vec![tmp.clone()], "terminal launch");
        assert_eq!(startup_paths(args(&["/nonexistent/x"]), None), vec![PathBuf::from("/nonexistent/x")]);
    }
}
