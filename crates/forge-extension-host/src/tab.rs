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
            const f = __forge.modules['@forge/api'].forge;
            const api = __forge.modules['@forge/api'];
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
