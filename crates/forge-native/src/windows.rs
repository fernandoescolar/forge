//! Opening and closing windows: File › Open asks for folders or files first and opens them
//! in a new window; File › New Window opens one with the welcome page, laid out like the
//! window it was opened from. A closed window leaves the session (it doesn't come back
//! when Forge starts again); quitting keeps every window open then for next time.

use gpui::TaskExt as _;
use gpui::{App, Global, PathPromptOptions, Window, WindowHandle};
use workspace::{
    AppState, MultiWorkspace, OpenMode, OpenOptions, Workspace,
    dock::{DockPosition, PanelSizeState},
};

pub fn init(cx: &mut App) {
    // Zed opens a window before asking when no window shows a project, and adds what is
    // chosen to the window's sidebar; Zed's editor opens new windows on an untitled file.
    // Forge's handlers go first and stop there. They run right after the action, which
    // arrives through the active window: they read that window, busy until then.
    cx.on_action(|_: &workspace::Open, cx| {
        cx.stop_propagation();
        cx.defer(open);
    });
    cx.on_action(|_: &workspace::NewWindow, cx| {
        cx.stop_propagation();
        cx.defer(new_window);
    });
    // The close button closes the window the way Close Window does (as Zed's own setup
    // does): unsaved changes are asked about, and the window leaves the session. Without
    // this, the window just goes and comes back the next time Forge starts.
    cx.observe_new(|_: &mut MultiWorkspace, window, cx| {
        let Some(window) = window else { return };
        let handle = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            handle
                .update(cx, |mw, cx| {
                    // It closes once ready; not now.
                    mw.close_window(&workspace::CloseWindow, window, cx);
                    false
                })
                .unwrap_or(true)
        });
    })
    .detach();
    cx.on_action(|_: &crate::Quit, cx| quit(cx));
}

static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Quit: each window is saved as it is (unsaved changes asked about, or kept for next time)
/// and stays in the session, so they all open again next time; then Forge ends.
fn quit(cx: &mut App) {
    use std::sync::atomic::Ordering;
    if QUITTING.swap(true, Ordering::AcqRel) {
        return;
    }
    let windows: Vec<WindowHandle<MultiWorkspace>> = cx.windows().into_iter().filter_map(|w| w.downcast::<MultiWorkspace>()).collect();
    cx.spawn(async move |cx| {
        if workspace::prepare_windows_to_quit(&windows, cx).await {
            cx.update(|cx| cx.quit());
        }
        QUITTING.store(false, Ordering::Release);
    })
    .detach();
}

/// The frontmost Forge window.
fn active_window(cx: &App) -> Option<WindowHandle<MultiWorkspace>> {
    cx.active_window().and_then(|w| w.downcast::<MultiWorkspace>())
}

/// File › Open: the system's open dialog, then a new window for what was chosen. The window
/// it was asked from closes when it has nothing open (just the welcome page), so opening a
/// project from a fresh window doesn't leave an empty one behind.
fn open(cx: &mut App) {
    let Some(app_state) = AppState::try_global(cx) else { return };
    let from = active_window(cx);
    let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: true, multiple: true, prompt: None });
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = paths.await else { return anyhow::Ok(()) };
        let options = OpenOptions { open_mode: OpenMode::NewWindow, add_dirs_to_sidebar: false, ..OpenOptions::default() };
        cx.update(|cx| workspace::open_paths(&paths, app_state, options, cx)).await?;
        if let Some(from) = from {
            let empty = from.read_with(cx, |mw, cx| is_empty(mw.workspace().read(cx), cx)).unwrap_or(false);
            if empty {
                // Closed, not just removed: it leaves the session too.
                from.update(cx, |mw, window, cx| mw.close_window(&workspace::CloseWindow, window, cx)).ok();
            }
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// No folders, and nothing unsaved.
fn is_empty(workspace: &Workspace, cx: &App) -> bool {
    workspace.project().read(cx).worktrees(cx).next().is_none() && !workspace.items(cx).any(|item| item.is_dirty(cx))
}

/// File › New Window: an empty window on the welcome page, with the docks of the window
/// it was opened from (which are open, what they show, how big).
fn new_window(cx: &mut App) {
    let Some(app_state) = AppState::try_global(cx) else { return };
    let layout = active_window(cx).and_then(|w| w.read_with(cx, |mw, cx| DockLayout::capture(mw.workspace().read(cx), cx)).ok());
    cx.set_global(PendingLayout(layout));
    workspace::open_new(OpenOptions::default(), app_state, cx, |workspace, window, cx| {
        cx.activate(true);
        crate::welcome::show(workspace, window, cx);
    })
    .detach_and_log_err(cx);
}

/// The layout for the next window to finish setting up its panels (see
/// [`apply_pending_layout`]): Forge adds them once the window exists.
#[derive(Default)]
struct PendingLayout(Option<DockLayout>);
impl Global for PendingLayout {}

/// Called once a new window's panels are in place.
pub fn apply_pending_layout(workspace: &mut Workspace, window: &mut Window, cx: &mut gpui::Context<Workspace>) {
    let Some(layout) = cx.try_global::<PendingLayout>().and_then(|p| p.0.clone()) else { return };
    cx.set_global(PendingLayout(None));
    layout.apply(workspace, window, cx);
}

/// Each dock: whether it is open, the panel it shows and that panel's size.
#[derive(Clone, Debug, PartialEq)]
pub struct DockLayout(Vec<(DockPosition, bool, Option<String>, Option<PanelSizeState>)>);

impl DockLayout {
    pub fn capture(workspace: &Workspace, cx: &App) -> Self {
        let docks = [DockPosition::Left, DockPosition::Right, DockPosition::Bottom].map(|position| {
            let dock = workspace.dock_at_position(position).read(cx);
            let active = dock.active_panel();
            let size = active.and_then(|panel| dock.stored_panel_size_state(panel.as_ref()));
            (position, dock.is_open(), active.map(|panel| panel.persistent_name().to_string()), size)
        });
        Self(docks.into())
    }

    pub fn apply(&self, workspace: &mut Workspace, window: &mut Window, cx: &mut gpui::Context<Workspace>) {
        for (position, open, active, size) in &self.0 {
            let dock = workspace.dock_at_position(*position).clone();
            dock.update(cx, |dock, cx| {
                if let Some(ix) = active.as_deref().and_then(|name| dock.panel_index_for_persistent_name(name, cx)) {
                    dock.activate_panel(ix, window, cx);
                    if let (Some(size), Some(panel)) = (size, dock.active_panel().cloned()) {
                        dock.set_panel_size_state(panel.as_ref(), *size, cx);
                    }
                }
                dock.set_open(*open, window, cx);
            });
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    /// Folders opened from outside (command line, Dock): the window that shows them, or a
    /// window of their own, never another window's sidebar.
    #[gpui::test]
    async fn folders_from_outside_get_their_own_window(cx: &mut TestAppContext) {
        let params = crate::welcome::tests::init_forge(cx);
        params.fs.as_fake().insert_tree("/proj", serde_json::json!({ "a.txt": "hi" })).await;
        params.fs.as_fake().insert_tree("/other", serde_json::json!({ "b.txt": "hi" })).await;
        let open = |path: &'static str, cx: &mut TestAppContext| {
            let params = params.clone();
            cx.update(|cx| workspace::open_paths(&[path.into()], params, crate::own_window(), cx))
        };
        open("/proj", cx).await.unwrap();
        cx.run_until_parked();
        open("/proj", cx).await.unwrap();
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| cx.windows().len()), 1, "the window that shows it");
        open("/other", cx).await.unwrap();
        cx.run_until_parked();
        let shown: Vec<Vec<std::path::PathBuf>> = cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter_map(|w| w.downcast::<MultiWorkspace>())
                .filter_map(|w| w.read(cx).ok().map(|mw| mw.workspaces().map(|ws| ws.read(cx).visible_worktrees(cx).count()).sum::<usize>()))
                .map(|count| vec![std::path::PathBuf::new(); count])
                .collect()
        });
        assert_eq!(shown.len(), 2, "a window of its own");
        assert!(shown.iter().all(|w| w.len() == 1), "one project per window: {shown:?}");
    }

    /// File › Open from an empty window asks first: no window opens before the dialog.
    #[gpui::test]
    async fn open_asks_before_opening_a_window(cx: &mut TestAppContext) {
        let params = crate::welcome::tests::init_forge(cx);
        cx.update(init);
        let project = project::Project::test(params.fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let vcx = &mut VisualTestContext::from_window(window.into(), cx);
        vcx.run_until_parked();
        vcx.dispatch_action(workspace::Open::default());
        vcx.run_until_parked();
        assert!(cx.did_prompt_for_paths(), "the open dialog");
        assert_eq!(cx.update(|cx| cx.windows().len()), 1, "no window before choosing");
    }

    /// New Window: the welcome page, and the docks of the window it came from.
    #[gpui::test]
    async fn new_window_shows_welcome_with_the_same_docks(cx: &mut TestAppContext) {
        let params = crate::welcome::tests::init_forge(cx);
        cx.update(init);
        params.fs.as_fake().insert_tree("/proj", serde_json::json!({ "a.txt": "hi" })).await;
        let project = project::Project::test(params.fs.clone(), ["/proj".as_ref()], cx).await;
        let first = cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let vcx = &mut VisualTestContext::from_window(first.into(), cx);
        vcx.run_until_parked();
        let workspace = first.read_with(vcx, |mw, _| mw.workspace().clone()).unwrap();
        // A layout of one's own: the bottom dock open on the Output panel, the left one closed.
        workspace.update_in(vcx, |ws, window, cx| {
            ws.toggle_panel_focus::<forge_output::OutputPanel>(window, cx);
            ws.left_dock().update(cx, |dock, cx| dock.set_open(false, window, cx));
        });
        vcx.run_until_parked();
        let before = workspace.read_with(vcx, |ws, cx| DockLayout::capture(ws, cx));
        assert!(before.0.iter().any(|(_, open, active, _)| *open && active.as_deref() == Some("ForgeOutputPanel")), "{before:?}");

        vcx.update(|window, _| window.activate_window());
        vcx.dispatch_action(workspace::NewWindow);
        vcx.run_until_parked();
        let windows: Vec<_> = cx.update(|cx| cx.windows().into_iter().filter_map(|w| w.downcast::<MultiWorkspace>()).collect());
        assert_eq!(windows.len(), 2, "a second window");
        let second = windows.into_iter().find(|w| *w != first).unwrap();
        let (layout, welcome, untitled) = second
            .read_with(cx, |mw, cx| {
                let ws = mw.workspace().read(cx);
                let pane = ws.active_pane().read(cx);
                (DockLayout::capture(ws, cx), pane.items_of_type::<crate::welcome::ForgeWelcome>().count(), pane.items_len() - pane.items_of_type::<crate::welcome::ForgeWelcome>().count())
            })
            .unwrap();
        assert_eq!((welcome, untitled), (1, 0), "the welcome page, not an untitled file");
        assert_eq!(layout, before, "laid out like the window it came from");
    }
}
