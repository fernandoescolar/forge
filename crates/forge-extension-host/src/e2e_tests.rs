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

/// MongoDB through the same UI: a connection by host and port, a database and its
/// collection in the tree, its documents in a table, and one edited whole, one inserted and
/// one deleted. Needs a server: FORGE_SQL_TEST_MONGO='{"host":"127.0.0.1","port":27099}'
/// (e.g. `docker run -d --rm -p 127.0.0.1:27099:27017 mongo`).
#[gpui::test]
async fn database_explorer_mongodb_end_to_end(cx: &mut gpui::TestAppContext) {
    let extension = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../extensions/db-explorer");
    let sidecar = crate::process::sidecar_path(&extension, "forge-sql");
    let Some(server) = std::env::var("FORGE_SQL_TEST_MONGO").ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) else {
        eprintln!("skipping: FORGE_SQL_TEST_MONGO not set");
        return;
    };
    if !extension.join("dist/extension.js").is_file() || !sidecar.is_file() {
        eprintln!("skipping: build extensions/db-explorer and its sidecar first");
        return;
    }
    let (host_name, port) = (server["host"].as_str().unwrap_or("127.0.0.1").to_string(), server["port"].as_u64().unwrap_or(27017));

    // Seed a collection with the sidecar itself (dropping what an earlier run left).
    {
        use std::io::{BufRead, Write};
        let mut seed = std::process::Command::new(&sidecar).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        let mut stdin = seed.stdin.take().unwrap();
        let mut lines = std::io::BufReader::new(seed.stdout.take().unwrap()).lines();
        let mut call = |id: u64, method: &str, params: Value| -> Value {
            writeln!(stdin, "{}", json!({ "id": id, "method": method, "params": params })).unwrap();
            let line = lines.next().unwrap().unwrap();
            serde_json::from_str(&line).unwrap()
        };
        let c = json!({ "connectionId": "seed", "database": "forge_e2e", "collection": "people" });
        let with = |doc: &str| {
            let mut v = c.clone();
            v["document"] = json!(doc);
            v
        };
        assert!(call(1, "connect", json!({ "connectionId": "seed", "engine": "mongodb", "host": host_name, "port": port })).get("result").is_some());
        let old = call(2, "find", c.clone());
        for (i, d) in old["result"]["documents"].as_array().unwrap().iter().enumerate() {
            let mut v = c.clone();
            v["id"] = d["id"].clone();
            call(10 + i as u64, "deleteDocument", v);
        }
        call(3, "insertDocument", with(r#"{ "_id": ObjectId("65f1c0ffee0000000000aaaa"), "name": "Ada", "born": ISODate("1815-12-10") }"#));
        call(4, "insertDocument", with(r#"{ "name": "Grace", "langs": ["COBOL"] }"#));
        call(5, "insertDocument", with(r#"{ "name": "Linus", "score": NumberLong("7") }"#));
        drop(stdin);
        seed.wait().unwrap();
    }

    cx.executor().allow_parking();
    let params = cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        crate::init_for_tests(cx);
    });
    let host = cx.update(|cx| ExtensionHost::init(vec![extension.clone()], cx)).unwrap();
    params.fs.as_fake().insert_tree("/root", json!({})).await;
    let project = project::Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let weak = workspace.downgrade();
    cx.update(|window, cx| host.update(cx, |h, cx| h.set_workspace(weak, window.window_handle(), cx)));
    let mut ui = Ui { host: host.clone(), cx };

    // A MongoDB connection by host and port, without a user.
    let add = ui.by_label("db-explorer", "button", "Add Connection…");
    ui.send(add, "onClick", Value::Null);
    let form = ui.tab("db-explorer.connection:new");
    let engine = ui.find(&form, "select", "the engine select", |p| p.get("value") == Some(&json!("postgres")));
    ui.send(engine, "onChange", json!("mongodb"));
    let host_input = ui.find(&form, "input", "the host input", |p| p.get("placeholder").and_then(Value::as_str) == Some("localhost"));
    ui.send(host_input, "onChange", json!(host_name));
    let port_input = ui.find(&form, "input", "the port input", |p| p.get("placeholder").and_then(Value::as_str) == Some("27017"));
    ui.send(port_input, "onChange", json!(port.to_string()));
    let save = ui.by_label(&form, "button", "Save and Connect");
    ui.send(save, "onClick", Value::Null);

    // Databases, then the collection; double-click opens its documents.
    let database = ui.by_label("db-explorer", "treeItem", "forge_e2e");
    ui.send(database, "onClick", Value::Null);
    let people = ui.by_label("db-explorer", "treeItem", "people");
    assert_eq!(ui.prop("db-explorer", people, "icon"), json!("json"));
    ui.send(people, "onDoubleClick", Value::Null);
    let tab = ui.tab("db-explorer.collection:");
    ui.text(&tab, "1–3 of 3");
    let grid = ui.find(&tab, "grid", "the documents", |p| p.get("rows").and_then(Value::as_array).is_some_and(|r| r.len() == 3));
    let columns: Vec<Value> = ui.prop(&tab, grid, "columns").as_array().unwrap().iter().map(|c| c["name"].clone()).collect();
    assert_eq!(columns, [json!("_id"), json!("name"), json!("born"), json!("langs"), json!("score")], "_id first, then fields as they appear");
    assert_eq!(ui.prop(&tab, grid, "rows")[1][3], json!("[ 1 item ]"));

    // Select Ada: her document, as text, in the editor; edit it whole and save.
    ui.send(grid, "onSelect", json!({ "rows": [0] }));
    let editor = ui.find(&tab, "input", "the document editor", |p| p.get("multiline") == Some(&json!(true)) && p.get("value").and_then(Value::as_str).is_some_and(|v| v.contains("\"Ada\"")));
    let text = ui.prop(&tab, editor, "value").as_str().unwrap().to_string();
    assert!(text.contains("ObjectId(\"65f1c0ffee0000000000aaaa\")") && text.contains("ISODate(\"1815-12-10T00:00:00Z\")"), "{text}");
    let edited = text.replace("\"Ada\"", "\"Ada Lovelace\"");
    ui.send(editor, "onChange", json!(edited));
    let save = ui.by_label(&tab, "button", "Save");
    ui.send(save, "onClick", Value::Null);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ui.cx.has_pending_prompt() {
        assert!(Instant::now() < deadline, "no confirmation before saving");
        ui.cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
    ui.cx.simulate_prompt_answer("Save");
    ui.wait("the saved name in the table", |h| (h.trees.get(&tab)?.get(grid)?.prop("rows")?[0][1] == json!("Ada Lovelace")).then_some(()));

    // Find with a filter that uses a helper.
    let filter = ui.find(&tab, "input", "the filter", |p| p.get("placeholder").and_then(Value::as_str).is_some_and(|s| s.starts_with("Filter:")));
    let by_id = r#"{ "_id": ObjectId("65f1c0ffee0000000000aaaa") }"#;
    ui.send(filter, "onChange", json!(by_id));
    ui.send(filter, "onSubmit", json!(by_id));
    ui.text(&tab, "1–1 of 1");

    // Insert a document, then find everything again.
    let insert = ui.by_label(&tab, "button", "Insert Document");
    ui.send(insert, "onClick", Value::Null);
    let editor = ui.find(&tab, "input", "the new document's editor", |p| p.get("multiline") == Some(&json!(true)) && p.get("value").and_then(Value::as_str) == Some("{\n  \n}"));
    ui.send(editor, "onChange", json!(r#"{ "name": "Margaret", "missions": NumberInt(1) }"#));
    let insert = ui.by_label(&tab, "button", "Insert");
    ui.send(insert, "onClick", Value::Null);
    ui.send(filter, "onChange", json!(""));
    ui.send(filter, "onSubmit", json!(""));
    ui.text(&tab, "1–4 of 4");

    // Delete Linus (confirmed).
    let rows = ui.prop(&tab, grid, "rows");
    let linus = rows.as_array().unwrap().iter().position(|r| r[1] == json!("Linus")).unwrap();
    ui.send(grid, "onSelect", json!({ "rows": [linus] }));
    let delete = ui.find(&tab, "button", "the delete button", |p| p.get("tooltip").and_then(Value::as_str) == Some("Delete Document"));
    ui.send(delete, "onClick", Value::Null);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ui.cx.has_pending_prompt() {
        assert!(Instant::now() < deadline, "no confirmation before deleting");
        ui.cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
    ui.cx.simulate_prompt_answer("Delete");
    ui.text(&tab, "1–3 of 3");

    // A filter that isn't JSON says so.
    ui.send(filter, "onChange", json!("{ name: 1 }"));
    ui.send(filter, "onSubmit", json!("{ name: 1 }"));
    ui.text(&tab, "not valid JSON");
}
