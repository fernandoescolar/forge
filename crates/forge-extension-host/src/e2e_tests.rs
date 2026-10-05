//! Database Explorer (extensions/db-explorer) end to end: the real extension bundle in
//! QuickJS, its real `forge-sql` sidecar and a SQLite file, driven through the UI events a
//! user's clicks and typing send. Skipped when the extension or its sidecar isn't built
//! (`npm run build && npm run sidecar -- --host-only` in extensions/db-explorer).

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use gpui::VisualTestContext;
use serde_json::{Value, json};

use crate::{
    host::ExtensionHost,
    surface::{collect, text_content},
    tree::{NodeId, NodeKind},
};

struct Ui<'a> {
    host: gpui::Entity<ExtensionHost>,
    cx: &'a mut VisualTestContext,
}

impl Ui<'_> {
    /// Waits for `f` to find something, running the app and the JS thread meanwhile.
    fn wait<T>(&mut self, what: &str, mut f: impl FnMut(&ExtensionHost) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            self.cx.run_until_parked();
            if let Some(found) = self.host.read_with(self.cx, |h, _| f(h)) {
                return found;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}; errors: {:?}\n{}", self.host.read_with(self.cx, |h, _| h.errors.clone()), self.host.read_with(self.cx, |h, _| dump(h)));
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The first `kind` node in panel or tab `panel` whose props match `pred`.
    fn find(&mut self, panel: &str, kind: &str, what: &str, pred: impl Fn(&serde_json::Map<String, Value>) -> bool) -> NodeId {
        let panel = panel.to_string();
        self.wait(what, |h| {
            let tree = h.trees.get(&panel)?;
            collect(tree, |n| matches!(&n.kind, NodeKind::Element { kind: k, props, .. } if k == kind && pred(props))).into_iter().next()
        })
    }

    fn by_label(&mut self, panel: &str, kind: &str, label: &str) -> NodeId {
        self.find(panel, kind, &format!("{kind} “{label}” in {panel}"), |p| p.get("label").and_then(Value::as_str) == Some(label))
    }

    fn send(&mut self, node: NodeId, event: &str, payload: Value) {
        self.host.read_with(self.cx, |h, _| h.dispatch(node, event, payload));
        self.cx.run_until_parked();
    }

    fn prop(&mut self, panel: &str, node: NodeId, key: &str) -> Value {
        self.host.read_with(self.cx, |h, _| h.trees.get(panel).and_then(|t| t.get(node)).and_then(|n| n.prop(key).cloned()).unwrap_or(Value::Null))
    }

    /// Waits for text containing `needle` in `panel`.
    fn text(&mut self, panel: &str, needle: &str) {
        let (panel, needle) = (panel.to_string(), needle.to_string());
        self.wait(&format!("text “{needle}” in {panel}"), |h| {
            let tree = h.trees.get(&panel)?;
            collect(tree, |n| matches!(&n.kind, NodeKind::Element { kind, .. } if kind == "text")).into_iter().any(|id| text_content(tree, id).contains(&needle)).then_some(())
        })
    }

    /// The id of the open tab whose id starts with `prefix`.
    fn tab(&mut self, prefix: &str) -> String {
        let prefix = prefix.to_string();
        self.wait(&format!("a tab {prefix}…"), |h| h.trees.keys().find(|k| k.starts_with(&prefix) && h.trees[*k].len() > 3).cloned())
    }

    /// Runs a query in a new query tab and returns that tab's id.
    fn query(&mut self, sql: &str) -> String {
        let before: Vec<String> = self.host.read_with(self.cx, |h, _| h.trees.keys().cloned().collect());
        self.host.read_with(self.cx, |h, _| h.run_command("db-explorer.newQuery"));
        let tab = self.wait("a new query tab", |h| h.trees.keys().find(|k| k.starts_with("db-explorer.query:") && !before.contains(k) && h.trees[*k].len() > 3).cloned());
        let input = self.find(&tab, "input", "the SQL input", |p| p.get("multiline") == Some(&json!(true)));
        self.send(input, "onChange", json!(sql));
        self.send(input, "onSubmit", json!(sql));
        tab
    }
}

/// What each panel shows, for failure messages: labels and texts.
fn dump(h: &ExtensionHost) -> String {
    let mut out = String::new();
    for (panel, tree) in &h.trees {
        out.push_str(&format!("[{panel}]"));
        for id in collect(tree, |n| matches!(&n.kind, NodeKind::Element { kind, .. } if kind == "text" || kind == "treeItem" || kind == "button")) {
            let n = tree.get(id).unwrap();
            let label = n.str_prop("label").map(str::to_string).unwrap_or_else(|| text_content(tree, id));
            let extra = n.str_prop("description").map(|d| format!(" ({d})")).unwrap_or_default();
            out.push_str(&format!(" «{label}{extra}»"));
        }
        out.push('\n');
    }
    out
}

#[gpui::test]
async fn database_explorer_end_to_end(cx: &mut gpui::TestAppContext) {
    let extension = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../extensions/db-explorer");
    if !extension.join("dist/extension.js").is_file() || !crate::process::sidecar_path(&extension, "forge-sql").is_file() {
        eprintln!("skipping: build extensions/db-explorer and its sidecar first");
        return;
    }
    cx.executor().allow_parking();
    let params = cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        crate::init_for_tests(cx);
    });
    let tmp = tempfile::tempdir().unwrap();
    let database = tmp.path().join("shop.sqlite");
    std::fs::write(&database, b"").unwrap();

    let host = cx.update(|cx| ExtensionHost::init(vec![extension.clone()], cx)).unwrap();
    params.fs.as_fake().insert_tree("/root", json!({ "a.sql": "" })).await;
    let project = project::Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let weak = workspace.downgrade();
    cx.update(|window, cx| host.update(cx, |h, cx| h.set_workspace(weak, window.window_handle(), cx)));
    let mut ui = Ui { host: host.clone(), cx };

    // The panel starts empty; add a SQLite connection through the form.
    let add = ui.by_label("db-explorer", "button", "Add Connection…");
    ui.send(add, "onClick", Value::Null);
    let form = ui.tab("db-explorer.connection:new");
    let engine = ui.find(&form, "select", "the engine select", |_| true);
    ui.send(engine, "onChange", json!("sqlite"));
    let file = ui.find(&form, "input", "the file input", |p| p.get("placeholder").and_then(Value::as_str) == Some("/path/to/database.sqlite"));
    ui.send(file, "onChange", json!(database.to_string_lossy()));
    let save = ui.by_label(&form, "button", "Save and Connect");
    ui.send(save, "onClick", Value::Null);

    // It connects and shows its Tables and Views.
    let tables = ui.by_label("db-explorer", "treeItem", "Tables");
    ui.by_label("db-explorer", "treeItem", "Views");
    assert!(ui.host.read_with(ui.cx, |h, _| !h.trees.contains_key(&form)), "the form closed");

    // A multi-statement script creates and fills tables.
    let script = "create table customers(id integer primary key, name text not null, email text);\n\
        with recursive n(i) as (select 1 union all select i + 1 from n where i < 1500) insert into customers(name, email) select 'Customer ' || i, 'c' || i || '@example.com' from n;\n\
        create view firsts as select * from customers where id <= 10;";
    let script_tab = ui.query(script);
    ui.text(&script_tab, "Finished in");

    // Refresh the Tables group: the new table is there; double-click opens its rows.
    ui.send(tables, "onContextMenu", json!({ "id": "refresh" }));
    if ui.prop("db-explorer", tables, "expanded") != json!(true) {
        ui.send(tables, "onClick", Value::Null);
    }
    let customers = ui.by_label("db-explorer", "treeItem", "customers");
    ui.send(customers, "onDoubleClick", Value::Null);
    let table = ui.tab("db-explorer.table:");
    ui.text(&table, "of 1500");
    let grid = ui.find(&table, "grid", "the rows", |p| p.get("rows").and_then(Value::as_array).is_some_and(|r| r.len() == 200));
    assert_eq!(ui.prop(&table, grid, "editable"), json!(true), "a table with a primary key is editable");
    assert_eq!(ui.prop(&table, grid, "rows")[0], json!([1, "Customer 1", "c1@example.com"]));

    // Sorting by a header reloads the page in that order.
    ui.send(grid, "onSort", json!({ "column": "id", "index": 0 }));
    ui.wait("sorted by id", |h| (h.trees.get(&table)?.get(grid)?.prop("sort")? == &json!({ "column": "id", "desc": false })).then_some(()));
    ui.send(grid, "onSort", json!({ "column": "id", "index": 0 }));
    ui.wait("rows sorted by id, descending", |h| (h.trees.get(&table)?.get(grid)?.prop("rows")?[0][0] == json!(1500)).then_some(()));

    // Edit a cell, set one to NULL, delete a row, add one: nothing is written until Save.
    ui.send(grid, "onCellEdit", json!({ "row": 0, "column": 1, "value": "Ada" }));
    ui.send(grid, "onContextMenu", json!({ "id": "null", "row": 1, "column": 2, "rows": [1] }));
    ui.send(grid, "onDeleteRows", json!({ "rows": [2] }));
    let add_row = ui.find(&table, "button", "Add Row", |p| p.get("tooltip").and_then(Value::as_str) == Some("Add Row"));
    ui.send(add_row, "onClick", Value::Null);
    ui.send(grid, "onCellEdit", json!({ "row": 200, "column": 1, "value": "Grace" }));
    let expected = json!({ "0": "modified", "1": "modified", "2": "deleted", "200": "new" });
    ui.wait("the changes marked in the grid", |h| (h.trees.get(&table)?.get(grid)?.prop("rowStates")? == &expected).then_some(()));
    let save = ui.by_label(&table, "button", "Save 4");
    ui.send(save, "onClick", Value::Null);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ui.cx.has_pending_prompt() {
        assert!(Instant::now() < deadline, "no confirmation before saving");
        ui.cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
    ui.cx.simulate_prompt_answer("Save");
    ui.text(&table, "Saved 4 changes.");

    // The database has them.
    let check = ui.query("select (select name from customers where id = 1500) as edited, (select email from customers where id = 1499) as nulled, (select count(*) from customers where id = 1498) as deleted, (select id from customers where name = 'Grace') as added, (select count(*) from firsts) as view_rows");
    let result = ui.find(&check, "grid", "the check's result", |p| p.get("rows").and_then(Value::as_array).is_some_and(|r| r.len() == 1));
    assert_eq!(ui.prop(&check, result, "rows"), json!([["Ada", null, 0, 1501, 10]]));

    // Errors come back as messages, not crashes.
    let failing = ui.query("select * from missing_table");
    ui.text(&failing, "no such table");
}
