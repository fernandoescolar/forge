use super::*;
use gpui::{Bounds, Modifiers, MouseButton, Pixels, Point, TestAppContext, VisualTestContext};

/// Each test gets its own database: hidden panels, groups and their sizes are saved
/// there, and tests running in parallel must not see each other's.
fn test_app_state(cx: &mut App) -> Arc<workspace::AppState> {
    cx.set_global(db::AppDatabase::test_new());
    workspace::AppState::test(cx)
}

fn center(b: Bounds<Pixels>) -> Point<Pixels> {
    b.center()
}

/// Clicking the button of the panel that is showing hides its dock; clicking it again
/// brings the panel back.
#[gpui::test]
async fn clicking_the_open_panel_button_hides_its_dock(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let params = cx.update(test_app_state);
    cx.update(|cx| {
        theme_settings::init(::theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        forge_output::panel::init(cx);
    });
    let project = project::Project::test(params.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);

    workspace.update_in(cx, |ws, window, cx| {
        let panel = cx.new(|cx| forge_output::OutputPanel::new(ws, window, cx));
        ws.add_panel(panel.clone(), window, cx);
        let entries = vec![DockEntry { handle: Arc::new(panel), title: Rc::new(|_| "Output".into()) }];
        install(ws, entries, None, window, cx);
        ws.focus_panel::<forge_output::OutputPanel>(window, cx);
    });
    cx.run_until_parked();
    let bottom_open = |cx: &mut VisualTestContext| workspace.read_with(cx, |ws, cx| ws.bottom_dock().read(cx).is_open());
    assert!(bottom_open(cx), "the Output panel is showing");

    let icon = cx.debug_bounds("forge-dock-button-ForgeOutputPanel").expect("Output icon in the status bar");
    cx.simulate_click(center(icon), Modifiers::none());
    cx.run_until_parked();
    assert!(!bottom_open(cx), "clicking the open panel's button hides the dock");

    let icon = cx.debug_bounds("forge-dock-button-ForgeOutputPanel").expect("Output icon in the status bar");
    cx.simulate_click(center(icon), Modifiers::none());
    cx.run_until_parked();
    assert!(bottom_open(cx), "clicking it again shows the panel");
}

/// A panel hidden from the side bars loses its button (and its dock closes if it was
/// showing); shown again, it comes back.
#[gpui::test]
async fn hides_and_shows_a_panel(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let params = cx.update(test_app_state);
    cx.update(|cx| {
        theme_settings::init(::theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        forge_output::panel::init(cx);
    });
    let project = project::Project::test(params.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    workspace.update_in(cx, |ws, window, cx| {
        let panel = cx.new(|cx| forge_output::OutputPanel::new(ws, window, cx));
        ws.add_panel(panel.clone(), window, cx);
        let entries = vec![DockEntry { handle: Arc::new(panel), title: Rc::new(|_| "Output".into()) }];
        install(ws, entries, None, window, cx);
        ws.focus_panel::<forge_output::OutputPanel>(window, cx);
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("forge-dock-button-ForgeOutputPanel").is_some());

    let weak = workspace.downgrade();
    cx.update(|window, cx| set_panel_hidden("ForgeOutputPanel", true, &weak, window, cx));
    cx.run_until_parked();
    assert!(cx.debug_bounds("forge-dock-button-ForgeOutputPanel").is_none(), "no button once hidden");
    assert!(!workspace.read_with(cx, |ws, cx| ws.bottom_dock().read(cx).is_open()), "its dock closed");

    cx.update(|window, cx| set_panel_hidden("ForgeOutputPanel", false, &weak, window, cx));
    cx.run_until_parked();
    assert!(cx.debug_bounds("forge-dock-button-ForgeOutputPanel").is_some(), "back after showing it");
}

/// Two panels shown together: the second moves to the first's dock, both are on
/// screen at once and both buttons light up; "Show Alone" splits them again.
#[gpui::test]
async fn shows_panels_together(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let params = cx.update(test_app_state);
    cx.update(|cx| {
        theme_settings::init(::theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        forge_output::panel::init(cx);
        forge_tests::init(cx);
    });
    let project = project::Project::test(params.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let (output, tests) = workspace.update_in(cx, |ws, window, cx| {
        let output = cx.new(|cx| forge_output::OutputPanel::new(ws, window, cx));
        ws.add_panel(output.clone(), window, cx);
        let tests = cx.new(|cx| forge_tests::TestPanel::new(ws, window, cx));
        ws.add_panel(tests.clone(), window, cx);
        let entries = vec![
            DockEntry { handle: Arc::new(output.clone()), title: Rc::new(|_| "Output".into()) },
            DockEntry { handle: Arc::new(tests.clone()), title: Rc::new(|_| "Tests".into()) },
        ];
        install(ws, entries, None, window, cx);
        ws.focus_panel::<forge_output::OutputPanel>(window, cx);
        (output, tests)
    });
    cx.run_until_parked();
    let shown = |cx: &mut VisualTestContext| workspace.read_with(cx, |ws, cx| ws.bottom_dock().read(cx).shown_panels().iter().map(|p| p.persistent_name()).collect::<Vec<_>>());
    assert_eq!(shown(cx), ["ForgeOutputPanel"]);

    let weak = workspace.downgrade();
    let (with, panel): (Arc<dyn PanelHandle>, Arc<dyn PanelHandle>) = (Arc::new(output), Arc::new(tests));
    cx.update(|window, cx| show_together("ForgeTestPanel", with, panel, &weak, window, cx));
    cx.run_until_parked();
    assert_eq!(shown(cx), ["ForgeOutputPanel", "ForgeTestPanel"], "both on screen, in the bottom dock");
    assert!(cx.debug_bounds("forge-dock-button-ForgeTestPanel").is_some());

    // Dragging the divider between them resizes them, and the sizes are remembered.
    let divider = cx.debug_bounds("dock-group-divider-1").expect("a divider between the two");
    let start = divider.center();
    cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::none());
    for step in 1..=10 {
        cx.simulate_mouse_move(start - point(px(15. * step as f32), px(0.)), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
    }
    cx.simulate_mouse_up(start - point(px(150.), px(0.)), gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.run_until_parked();
    let (output_share, tests_share) = cx.update(|_, cx| {
        let groups = PanelGroups::global(cx);
        (groups.read(cx).weight("ForgeOutputPanel").unwrap_or(1.), groups.read(cx).weight("ForgeTestPanel").unwrap_or(1.))
    });
    assert!(output_share < tests_share, "the left panel got smaller: {output_share} vs {tests_share}");

    cx.update(|_, cx| PanelGroups::global(cx).update(cx, |g, cx| g.leave("ForgeTestPanel", cx)));
    cx.run_until_parked();
    assert_eq!(shown(cx), ["ForgeTestPanel"], "alone again: the dock shows just its active panel");
}

/// Dropping one panel's button on another's (in another dock) shows them together.
#[gpui::test]
async fn dropping_a_button_on_another_groups_them(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let params = cx.update(test_app_state);
    cx.update(|cx| {
        theme_settings::init(::theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        forge_output::panel::init(cx);
        forge_tests::init(cx);
    });
    let project = project::Project::test(params.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    workspace.update_in(cx, |ws, window, cx| {
        let output = cx.new(|cx| forge_output::OutputPanel::new(ws, window, cx));
        ws.add_panel(output.clone(), window, cx);
        let tests = cx.new(|cx| forge_tests::TestPanel::new(ws, window, cx));
        ws.add_panel(tests.clone(), window, cx);
        let entries = vec![
            DockEntry { handle: Arc::new(output), title: Rc::new(|_| "Output".into()) },
            DockEntry { handle: Arc::new(tests), title: Rc::new(|_| "Tests".into()) },
        ];
        install(ws, entries, None, window, cx);
    });
    cx.run_until_parked();
    let output_button = cx.debug_bounds("forge-dock-button-ForgeOutputPanel").unwrap();
    let tests_button = cx.debug_bounds("forge-dock-button-ForgeTestPanel").unwrap();
    cx.simulate_mouse_down(center(output_button), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(center(output_button) + gpui::point(gpui::px(20.), gpui::px(-20.)), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    // The drop zones leave the button strips free (in the app the left zone sits level
    // with the buttons).
    let left_zone = cx.debug_bounds("forge-drop-left").expect("drop zones while dragging");
    assert!(left_zone.origin.x >= tests_button.origin.x + tests_button.size.width, "the left zone clears the strip: {left_zone:?} vs {tests_button:?}");
    cx.simulate_mouse_move(center(tests_button), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(center(tests_button), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    let shown = workspace.read_with(cx, |ws, cx| ws.left_dock().read(cx).shown_panels().iter().map(|p| p.persistent_name()).collect::<Vec<_>>());
    assert_eq!(shown, ["ForgeTestPanel", "ForgeOutputPanel"], "Output joined Tests in the left dock");
    cx.update(|_, cx| PanelGroups::global(cx).update(cx, |g, cx| g.leave("ForgeOutputPanel", cx)));
}

/// Dropping a button on another's trailing edge puts it after that one, in the same
/// strip, without grouping them; the order is remembered.
#[gpui::test]
async fn dropping_a_button_between_others_reorders_them(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let params = cx.update(test_app_state);
    cx.update(|cx| {
        theme_settings::init(::theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        forge_output::panel::init(cx);
        forge_tests::init(cx);
    });
    let project = project::Project::test(params.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    workspace.update_in(cx, |ws, window, cx| {
        let output = cx.new(|cx| forge_output::OutputPanel::new(ws, window, cx));
        ws.add_panel(output.clone(), window, cx);
        let tests = cx.new(|cx| forge_tests::TestPanel::new(ws, window, cx));
        ws.add_panel(tests.clone(), window, cx);
        let entries = vec![
            DockEntry { handle: Arc::new(output), title: Rc::new(|_| "Output".into()) },
            DockEntry { handle: Arc::new(tests), title: Rc::new(|_| "Tests".into()) },
        ];
        install(ws, entries, None, window, cx);
    });
    cx.run_until_parked();
    // Put Output in the left strip, below Tests.
    let output_button = cx.debug_bounds("forge-dock-button-ForgeOutputPanel").unwrap();
    let tests_button = cx.debug_bounds("forge-dock-button-ForgeTestPanel").unwrap();
    let below_tests = point(tests_button.center().x, tests_button.bottom() - px(2.));
    cx.simulate_mouse_down(center(output_button), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(center(output_button) + point(px(20.), px(-20.)), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    cx.simulate_mouse_move(below_tests, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    cx.simulate_mouse_up(below_tests, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    let in_left = workspace.read_with(cx, |ws, cx| ws.left_dock().read(cx).panel::<forge_output::OutputPanel>().is_some());
    assert!(in_left, "Output moved to the left dock");
    assert!(cx.update(|_, cx| PanelGroups::global(cx).read(cx).group_of("ForgeOutputPanel").is_none()), "not shown together");
    let (output_button, tests_button) =
        (cx.debug_bounds("forge-dock-button-ForgeOutputPanel").unwrap(), cx.debug_bounds("forge-dock-button-ForgeTestPanel").unwrap());
    assert!(output_button.origin.y > tests_button.origin.y, "Output's button is below Tests'");

    // Now drop Output on Tests' leading edge: it goes first.
    let above_tests = point(tests_button.center().x, tests_button.top() + px(2.));
    cx.simulate_mouse_down(center(output_button), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(center(output_button) + point(px(20.), px(-20.)), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    cx.simulate_mouse_move(above_tests, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    cx.simulate_mouse_up(above_tests, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    let (output_button, tests_button) =
        (cx.debug_bounds("forge-dock-button-ForgeOutputPanel").unwrap(), cx.debug_bounds("forge-dock-button-ForgeTestPanel").unwrap());
    assert!(output_button.origin.y < tests_button.origin.y, "Output's button is above Tests' now");
    let order = cx.update(|_, cx| PanelOrder::global(cx).read(cx).0.clone());
    assert_eq!(order, ["ForgeOutputPanel", "ForgeTestPanel"]);
}

/// Drags the Output panel's status-bar icon onto the left drop zone with real mouse
/// events and checks the workspace moved it.
#[gpui::test]
async fn dragging_a_dock_icon_moves_the_panel(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let params = cx.update(test_app_state);
    cx.update(|cx| {
        theme_settings::init(::theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
    });
    let project = project::Project::test(params.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);

    let output = workspace.update_in(cx, |ws, window, cx| {
        let panel = cx.new(|cx| forge_output::OutputPanel::new(ws, window, cx));
        ws.add_panel(panel.clone(), window, cx);
        let entries = vec![DockEntry { handle: Arc::new(panel.clone()), title: Rc::new(|_| "Output".into()) }];
        install(ws, entries, None, window, cx);
        panel
    });
    cx.run_until_parked();
    let still_zed_buttons = workspace.read_with(cx, |ws, cx| ws.status_bar().read(cx).item_of_type::<PanelButtons>().is_some());
    assert!(!still_zed_buttons, "Zed's panel buttons were replaced");

    let icon = cx.debug_bounds("forge-dock-button-ForgeOutputPanel").expect("Output icon in the status bar");
    assert!(cx.debug_bounds("forge-drop-left").is_none(), "no drop zones before dragging");

    // Press, move past the drag threshold, then over the left zone, and release.
    cx.simulate_mouse_down(center(icon), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(center(icon) + gpui::point(gpui::px(20.), gpui::px(-20.)), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    let left_zone = cx.debug_bounds("forge-drop-left").expect("drop zones appear while dragging");
    cx.simulate_mouse_move(center(left_zone), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(center(left_zone), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();

    let (in_left, open) = workspace.read_with(cx, |ws, cx| {
        let dock = ws.left_dock().read(cx);
        (dock.panel::<forge_output::OutputPanel>().is_some(), dock.is_open())
    });
    assert!(in_left, "the Output panel now lives in the left dock");
    assert!(open, "and the left dock was opened on it");
    assert!(cx.debug_bounds("forge-drop-left").is_none(), "drop zones are gone after the drop");
    drop(output);
}
