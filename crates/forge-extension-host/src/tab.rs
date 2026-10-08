//! Extension tabs: React surfaces opened in the editor area (`forge.tabs.open`), for UIs
//! that need room, like a table's rows or a query and its results.

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement as _, Render, SharedString, Styled as _, Window, div};
use std::str::FromStr as _;
use theme::ActiveTheme as _;
use ui::{Icon, IconName};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

use crate::{host::ExtensionHost, js::ToJs, surface::Surface};

pub struct ExtensionTab {
    pub id: String,
    pub title: String,
    pub icon: String,
    pub tooltip: Option<String>,
    surface: Entity<Surface>,
}

impl ExtensionTab {
    #[cfg(test)]
    pub(crate) fn surface(&self) -> &Entity<Surface> {
        &self.surface
    }

    fn new(host: Entity<ExtensionHost>, id: String, title: String, icon: String, cx: &mut Context<Self>) -> Self {
        let surface = cx.new(|cx| Surface::new(host.clone(), id.clone(), true, cx));
        // Closing the tab (dropping it, not moving it to another pane) unmounts its React tree.
        cx.on_release({
            let (host, id) = (host.clone(), id.clone());
            move |_, cx| {
                host.update(cx, |host, _| {
                    host.trees.remove(&id);
                    host.js.send(ToJs::Event { name: "tabs.closed".into(), json: serde_json::json!({ "id": id }).to_string() });
                })
            }
        })
        .detach();
        Self { id, title, icon, tooltip: None, surface }
    }
}

/// Opens tab `id` in the active pane, or shows it if it is already open.
pub fn open(host: Entity<ExtensionHost>, workspace: &mut Workspace, id: String, title: String, icon: String, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace.items_of_type::<ExtensionTab>(cx).find(|t| t.read(cx).id == id);
    if let Some(existing) = existing {
        existing.update(cx, |tab, cx| {
            tab.title = title;
            tab.icon = icon;
            cx.emit(ItemEvent::UpdateTab);
        });
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let tab = cx.new(|cx| ExtensionTab::new(host, id, title, icon, cx));
    workspace.add_item_to_active_pane(Box::new(tab), None, true, window, cx);
}

/// Closes tab `id` wherever it is.
pub fn close(workspace: &mut Workspace, id: &str, window: &mut Window, cx: &mut Context<Workspace>) {
    let tab = workspace.items_of_type::<ExtensionTab>(cx).find(|t| t.read(cx).id == id);
    let Some(tab) = tab else { return };
    let item_id = tab.entity_id();
    for pane in workspace.panes().to_vec() {
        if pane.read(cx).items().any(|item| item.item_id() == item_id) {
            pane.update(cx, |pane, cx| pane.close_item_by_id(item_id, workspace::SaveIntent::Skip, window, cx).detach());
        }
    }
}

/// Changes tab `id`'s title (and icon, tooltip).
pub fn update(workspace: &mut Workspace, id: &str, title: Option<String>, icon: Option<String>, tooltip: Option<String>, cx: &mut Context<Workspace>) {
    let tabs: Vec<_> = workspace.items_of_type::<ExtensionTab>(cx).filter(|t| t.read(cx).id == id).collect();
    for tab in tabs {
        tab.update(cx, |tab, cx| {
            if let Some(title) = title.clone() {
                tab.title = title;
            }
            if let Some(icon) = icon.clone() {
                tab.icon = icon;
            }
            if tooltip.is_some() {
                tab.tooltip = tooltip.clone();
            }
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        });
    }
}

impl Render for ExtensionTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(cx.theme().colors().editor_background).child(self.surface.clone())
    }
}

impl Focusable for ExtensionTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.surface.focus_handle(cx)
    }
}

impl EventEmitter<ItemEvent> for ExtensionTab {}

impl Item for ExtensionTab {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _: &App) -> SharedString {
        self.title.clone().into()
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::from_str(&self.icon).unwrap_or(IconName::Sparkle)))
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        self.tooltip.clone().map(Into::into)
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Typing into an empty input keeps every key, even when the screen redraws before the
    /// extension has heard of them (it runs on its own thread); a value the extension sets
    /// itself, like clearing the input, still replaces the text.
    #[gpui::test]
    async fn inputs_keep_what_is_typed(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init_for_tests(cx);
        });
        let tmp = tempfile::tempdir().unwrap();
        let ext = tmp.path().join("ext-typing");
        std::fs::create_dir_all(ext.join("dist")).unwrap();
        std::fs::write(ext.join("package.json"), r#"{"name":"ext-typing","forge":{}}"#).unwrap();
        let code = r#"var __forgeExtension = { activate(ctx) {
            const f = __forge.modules['@forge-ide/api'].forge;
            const R = __forge.modules.react;
            const h = R.createElement;
            const api = __forge.modules['@forge-ide/api'];
            function Form() {
                const [v, setV] = R.useState('');
                return h(api.View, {},
                    h(api.Input, { value: v, placeholder: 'Name', onChange: setV }),
                    h(api.Text, {}, 'value=' + v),
                    h(api.Button, { label: 'Clear', onClick: () => setV('') }));
            }
            f.commands.register('open', 'Open', () => f.tabs.open({ id: 'ext-typing.form', title: 'Form', render: () => h(Form) }));
            f.commands.register('ready', 'Ready', () => {});
        } };"#;
        std::fs::write(ext.join("dist/extension.js"), code).unwrap();
        let host = cx.update(|cx| ExtensionHost::init(vec![tmp.path().to_path_buf()], cx)).unwrap();
        params.fs.as_fake().insert_tree("/root", serde_json::json!({})).await;
        let project = project::Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        let weak = workspace.downgrade();
        cx.update(|window, cx| host.update(cx, |h, cx| h.set_workspace(weak, window.window_handle(), cx)));
        let wait = |what: &str, cx: &mut gpui::VisualTestContext, f: &dyn Fn(&mut gpui::VisualTestContext) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !f(cx) {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                cx.update(|window, _| window.refresh());
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait("the extension", cx, &|cx| host.read_with(cx, |h, _| h.commands.iter().any(|c| c.id == "ready")));
        host.read_with(cx, |h, _| h.run_command("open"));
        let surface = |cx: &mut gpui::VisualTestContext| workspace.read_with(cx, |ws, cx| ws.items_of_type::<ExtensionTab>(cx).next().map(|t| t.read(cx).surface().clone()));
        wait("the form", cx, &|cx| surface(cx).is_some_and(|s| s.read_with(cx, |s, _| !s.input_editors().is_empty())));
        let surface = surface(cx).unwrap();
        let editor = surface.read_with(cx, |s, _| s.input_editors()[0].clone());
        let shown = |cx: &mut gpui::VisualTestContext| {
            host.read_with(cx, |h, _| {
                h.trees
                    .get("ext-typing.form")
                    .map(|t| crate::surface::collect(t, |n| matches!(&n.kind, crate::tree::NodeKind::Element { kind, .. } if kind == "text")).into_iter().map(|id| crate::surface::text_content(t, id)).collect::<String>())
                    .unwrap_or_default()
            })
        };
        cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
        // A key, and a redraw before the extension answers: the key stays.
        let type_key = |key: &str, cx: &mut gpui::VisualTestContext| {
            editor.update_in(cx, |e, window, cx| e.insert(key, window, cx));
            surface.update_in(cx, |s, window, cx| s.sync(window, cx));
        };
        type_key("a", cx);
        assert_eq!(editor.read_with(cx, |e, cx| e.text(cx)), "a", "the first key is kept");
        type_key("d", cx);
        type_key("a", cx);
        assert_eq!(editor.read_with(cx, |e, cx| e.text(cx)), "ada");
        wait("the extension to have it", cx, &|cx| shown(cx) == "value=ada");
        assert_eq!(editor.read_with(cx, |e, cx| e.text(cx)), "ada", "its echo doesn't undo anything");

        // The extension clears it: that does replace the text, and typing starts over.
        let clear = host.read_with(cx, |h, _| {
            crate::surface::collect(&h.trees["ext-typing.form"], |n| matches!(&n.kind, crate::tree::NodeKind::Element { kind, props, .. } if kind == "button" && props.get("label") == Some(&serde_json::json!("Clear"))))[0]
        });
        host.read_with(cx, |h, _| h.dispatch(clear, "onClick", serde_json::Value::Null));
        wait("the input cleared", cx, &|cx| editor.read_with(cx, |e, cx| e.text(cx)).is_empty() && shown(cx) == "value=");
        type_key("x", cx);
        assert_eq!(editor.read_with(cx, |e, cx| e.text(cx)), "x", "typing after a clear is kept too");
        wait("the extension to have it", cx, &|cx| shown(cx) == "value=x");
    }

    /// An input with a `language` highlights its text as that language.
    #[gpui::test]
    async fn inputs_highlight_their_language(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init_for_tests(cx);
        });
        // As the app does: inputs find languages through the global app state.
        cx.update(|cx| workspace::AppState::set_global(params.clone(), cx));
        params.languages.add(std::sync::Arc::new(language::Language::new(language::LanguageConfig { name: "JSON".into(), ..Default::default() }, None)));
        let tmp = tempfile::tempdir().unwrap();
        let ext = tmp.path().join("ext-lang");
        std::fs::create_dir_all(ext.join("dist")).unwrap();
        std::fs::write(ext.join("package.json"), r#"{"name":"ext-lang","forge":{}}"#).unwrap();
        let code = r#"var __forgeExtension = { activate(ctx) {
            const f = __forge.modules['@forge-ide/api'].forge;
            const h = __forge.modules.react.createElement;
            const api = __forge.modules['@forge-ide/api'];
            f.commands.register('open', 'Open', () => {
                f.tabs.open({ id: 'ext-lang.doc', title: 'Doc', render: () => h(api.View, {},
                    h(api.Input, { value: '{ "a": 1 }', multiline: true, language: 'JSON' }),
                    h(api.Input, { value: 'plain' })) });
            });
            f.commands.register('ready', 'Ready', () => {});
        } };"#;
        std::fs::write(ext.join("dist/extension.js"), code).unwrap();
        let host = cx.update(|cx| ExtensionHost::init(vec![tmp.path().to_path_buf()], cx)).unwrap();
        params.fs.as_fake().insert_tree("/root", serde_json::json!({})).await;
        let project = project::Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        let weak = workspace.downgrade();
        cx.update(|window, cx| host.update(cx, |h, cx| h.set_workspace(weak, window.window_handle(), cx)));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !host.read_with(cx, |h, _| h.commands.iter().any(|c| c.id == "ready")) {
            assert!(Instant::now() < deadline, "the extension never loaded");
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
        }
        host.read_with(cx, |h, _| h.run_command("open"));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            let languages = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ExtensionTab>(cx).next().map(|t| t.read(cx).surface().read(cx).input_languages(cx)));
            if let Some(mut languages) = languages.filter(|l| l.len() == 2 && l.contains(&Some("JSON".to_string()))) {
                languages.sort();
                assert_eq!(languages, [None, Some("JSON".to_string())], "only the input that asked for it");
                break;
            }
            assert!(Instant::now() < deadline, "the input never got its language");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// With two windows, what an extension opens goes to the window the user is in (the one
    /// activated last), not to the one the host was first given.
    #[gpui::test]
    async fn tabs_open_in_the_active_window(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init_for_tests(cx);
        });
        let tmp = tempfile::tempdir().unwrap();
        let ext = tmp.path().join("ext-windows");
        std::fs::create_dir_all(ext.join("dist")).unwrap();
        std::fs::write(ext.join("package.json"), r#"{"name":"ext-windows","forge":{}}"#).unwrap();
        let code = r#"var __forgeExtension = { activate(ctx) {
            const f = __forge.modules['@forge-ide/api'].forge;
            const h = __forge.modules.react.createElement;
            const api = __forge.modules['@forge-ide/api'];
            let n = 0;
            f.commands.register('ready', 'Ready', () => {});
            f.commands.register('open', 'Open', () => {
                n += 1;
                f.tabs.open({ id: 'ext-windows.form' + n, title: 'Form ' + n, render: () => h(api.View, {}) });
            });
        } };"#;
        std::fs::write(ext.join("dist/extension.js"), code).unwrap();
        let host = cx.update(|cx| ExtensionHost::init(vec![tmp.path().to_path_buf()], cx)).unwrap();

        params.fs.as_fake().insert_tree("/one", serde_json::json!({})).await;
        params.fs.as_fake().insert_tree("/two", serde_json::json!({})).await;
        let mut windows = Vec::new();
        for root in ["/one", "/two"] {
            let project = project::Project::test(params.fs.clone(), [root.as_ref()], cx).await;
            let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
            let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
            let vcx = gpui::VisualTestContext::from_window(window.into(), cx);
            windows.push((workspace, vcx));
        }
        // As when the panels are added: the host is given each window as it opens, and
        // follows activations from then on.
        for (workspace, vcx) in &mut windows {
            let host = host.clone();
            workspace.update_in(vcx, |ws, window, cx| {
                crate::panel::follow_active_window(host.clone(), window, cx);
                let weak = ws.weak_handle();
                host.update(cx, |h, cx| h.set_workspace(weak, window.window_handle(), cx));
            });
        }
        let ready = |vcx: &mut gpui::VisualTestContext| host.read_with(vcx, |h, _| h.commands.iter().any(|c| c.id == "ready"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready(&mut windows[0].1) {
            assert!(Instant::now() < deadline, "the extension never loaded");
            windows[0].1.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
        }
        let tabs = |workspace: &Entity<Workspace>, vcx: &mut gpui::VisualTestContext| workspace.read_with(vcx, |ws, cx| ws.items_of_type::<ExtensionTab>(cx).map(|t| t.read(cx).title.to_string()).collect::<Vec<_>>());
        // Activates window `ix`, runs the extension's command there, waits for `total` tabs.
        let open_from = |ix: usize, total: usize, windows: &mut Vec<(Entity<Workspace>, gpui::VisualTestContext)>| {
            windows[ix].1.update(|window, _| window.activate_window());
            windows[ix].1.run_until_parked();
            host.read_with(&windows[ix].1, |h, _| h.run_command("open"));
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let mut opened = 0;
                for (ws, vcx) in windows.iter_mut() {
                    vcx.run_until_parked();
                    opened += tabs(ws, vcx).len();
                }
                if opened == total {
                    break;
                }
                assert!(Instant::now() < deadline, "the tab never opened");
                std::thread::sleep(Duration::from_millis(20));
            }
        };

        // The second window opened last; the user goes back to the first and clicks there.
        open_from(0, 1, &mut windows);
        let (one, two) = (windows[0].0.clone(), windows[1].0.clone());
        assert_eq!(tabs(&one, &mut windows[0].1), ["Form 1"], "in the window the user is in");
        assert!(tabs(&two, &mut windows[1].1).is_empty());

        open_from(1, 2, &mut windows);
        assert_eq!(tabs(&two, &mut windows[1].1), ["Form 2"]);
        assert_eq!(tabs(&one, &mut windows[0].1), ["Form 1"]);
    }

    /// An extension spawns a program and its own sidecar and talks to them, and opens a tab
    /// in the editor area with a data grid, a tree and a select; closing the tab tells it.
    #[gpui::test]
    async fn processes_sidecars_and_tabs(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init_for_tests(cx);
        });
        let tmp = tempfile::tempdir().unwrap();
        let ext = tmp.path().join("ext-tab");
        std::fs::create_dir_all(ext.join("dist")).unwrap();
        std::fs::write(ext.join("package.json"), r#"{"name":"ext-tab","forge":{"sidecars":["echoer"]}}"#).unwrap();
        let sidecar = crate::process::sidecar_path(&ext, "echoer");
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        // Not executable on purpose: spawning a sidecar fixes that.
        std::fs::write(&sidecar, "#!/bin/sh\nwhile read line; do echo \"sidecar $line\"; done\n").unwrap();
        let code = r#"var __forgeExtension = { activate(ctx) {
            const f = __forge.modules['@forge-ide/api'].forge;
            const api = __forge.modules['@forge-ide/api'];
            const h = __forge.modules.react.createElement;
            const mark = (id) => f.commands.register(id, id, () => {});
            f.commands.register('go', 'Go', async () => {
                try {
                    const p = await f.process.spawn('sh', { args: ['-c', 'while read l; do echo "got $l"; done'] });
                    const lines = [];
                    p.onLine((l) => lines.push(l));
                    p.write('one\ntwo\n');
                    p.end();
                    const code = await p.exited;
                    const s = await f.process.sidecar('echoer');
                    const first = new Promise((r) => s.onLine(r));
                    s.write('hi\n');
                    const reply = await first;
                    s.kill();
                    await s.exited;
                    mark('done ' + JSON.stringify({ lines, code, reply }));
                } catch (e) { mark('failed ' + e); }
            });
            f.commands.register('tab', 'Tab', () => {
                f.tabs.open({ id: 'ext-tab.grid', title: 'Rows', icon: 'table', onClose: () => mark('closed'), render: () =>
                    h(api.View, { style: { grow: true } },
                        h(api.TreeItem, { label: 'users', icon: 'table', depth: 1, expanded: false, contextMenu: [{ id: 'open', label: 'Open' }] }),
                        h(api.Select, { value: 'a', options: [{ value: 'a', label: 'A' }, { value: 'b', label: 'B' }] }),
                        h(api.Input, { value: 'select 1', multiline: true }),
                        h(api.Input, { value: 'secret', password: true }),
                        h(api.Spinner, {}),
                        h(api.DataGrid, { style: { grow: true }, editable: true, columns: [{ name: 'id', type: 'int', primaryKey: true }, { name: 'name' }],
                            rows: [[1, 'Ada'], [2, null]], selectedRows: [1], rowStates: { 0: 'modified' }, editedCells: ['0:1'] })) });
                mark('opened');
            });
        } };"#;
        std::fs::write(ext.join("dist/extension.js"), code).unwrap();
        let host = cx.update(|cx| ExtensionHost::init(vec![tmp.path().to_path_buf()], cx)).unwrap();

        params.fs.as_fake().insert_tree("/root", serde_json::json!({ "a.txt": "" })).await;
        let project = project::Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        let weak = workspace.downgrade();
        cx.update(|window, cx| host.update(cx, |h, cx| h.set_workspace(weak, window.window_handle(), cx)));

        let commands = |cx: &mut gpui::VisualTestContext| host.read_with(cx, |h, _| h.commands.iter().map(|c| c.id.clone()).collect::<Vec<_>>());
        let wait_for = |prefix: &str, cx: &mut gpui::VisualTestContext| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(id) = commands(cx).into_iter().find(|id| id.starts_with(prefix)) {
                    return id;
                }
                assert!(Instant::now() < deadline, "no command {prefix}…; have {:?}, errors {:?}", commands(cx), host.read_with(cx, |h, _| h.errors.clone()));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait_for("go", cx);
        host.read_with(cx, |h, _| h.run_command("go"));
        let done = wait_for("done ", cx);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(done.strip_prefix("done ").unwrap()).unwrap(),
            serde_json::json!({ "lines": ["got one", "got two"], "code": 0, "reply": "sidecar hi" })
        );
        assert!(host.read_with(cx, |h, _| h.processes.is_empty()), "ended processes are forgotten");

        host.read_with(cx, |h, _| h.run_command("tab"));
        wait_for("opened", cx);
        let deadline = Instant::now() + Duration::from_secs(10);
        let tab = loop {
            cx.run_until_parked();
            let tab = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ExtensionTab>(cx).next());
            if let Some(tab) = tab.filter(|_| host.read_with(cx, |h, _| h.trees.get("ext-tab.grid").is_some_and(|t| t.len() > 5))) {
                break tab;
            }
            assert!(Instant::now() < deadline, "the tab never opened");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(tab.read_with(cx, |t, _| t.title.clone()), "Rows");
        // Draw it: the grid, tree item, select, inputs and spinner render natively.
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        workspace.update_in(cx, |ws, window, cx| close(ws, "ext-tab.grid", window, cx));
        drop(tab);
        wait_for("closed", cx);
        assert!(host.read_with(cx, |h, _| !h.trees.contains_key("ext-tab.grid")), "the closed tab's tree is dropped");
    }
}
