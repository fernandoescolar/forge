//! Database Explorer (extensions/db-explorer) end to end: the real extension bundle in
//! QuickJS, its real `forge-sql` sidecar and a SQLite file, driven through the UI events a
//! user's clicks and typing send. Skipped when the extension or its sidecar isn't built
//! (`npm run build && npm run sidecar -- --host-only` in extensions/db-explorer). Containers
//! (extensions/containers) too, against the real Docker CLI when Docker is running.

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
        // And 25 in another collection, for paging.
        let many = json!({ "connectionId": "seed", "database": "forge_e2e", "collection": "many", "limit": 1000 });
        let old = call(6, "find", many.clone());
        for (i, d) in old["result"]["documents"].as_array().unwrap().iter().enumerate() {
            let mut v = many.clone();
            v["id"] = d["id"].clone();
            call(100 + i as u64, "deleteDocument", v);
        }
        for i in 0..25 {
            let mut v = many.clone();
            v["document"] = json!(format!("{{ \"n\": {i} }}"));
            call(200 + i, "insertDocument", v);
        }
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
    // Drawn, the table has room for its rows (a row of views doesn't stretch its children
    // unless asked: the table once got no height at all).
    ui.cx.update(|window, _| window.refresh());
    ui.cx.run_until_parked();
    let selector: &'static str = Box::leak(format!("forge-grid-{tab}-{grid}").into_boxed_str());
    let bounds = ui.cx.debug_bounds(selector).expect("the documents table is drawn");
    assert!(bounds.size.height > gpui::px(100.), "the documents table is {:?} high", bounds.size.height);

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

    // Opening a collection shows its first page at once; the page size can change.
    let many = ui.by_label("db-explorer", "treeItem", "many");
    ui.send(many, "onDoubleClick", Value::Null);
    let many_tab = ui.wait("the many tab", |h| h.trees.keys().find(|k| k.ends_with(":many") && h.trees[*k].len() > 3).cloned());
    ui.text(&many_tab, "1–25 of 25");
    let size = ui.find(&many_tab, "select", "the page size", |p| p.get("value") == Some(&json!(200)));
    ui.send(size, "onChange", json!(10));
    ui.text(&many_tab, "1–10 of 25");
    let next = ui.find(&many_tab, "button", "the next page button", |p| p.get("tooltip").and_then(Value::as_str) == Some("Next page"));
    ui.send(next, "onClick", Value::Null);
    ui.text(&many_tab, "11–20 of 25");
}

/// Redis through the same UI: a connection with a password on database 9; its keys as a tree
/// split at `:`, filtered by a pattern and loaded in batches; a hash edited and saved; a key
/// created; the console. Needs a server: FORGE_SQL_TEST_REDIS='{"host":"127.0.0.1","port":6390,"password":"pw"}'
/// (e.g. `docker run -d --rm -p 127.0.0.1:6390:6379 redis redis-server --requirepass pw`).
#[gpui::test]
async fn database_explorer_redis_end_to_end(cx: &mut gpui::TestAppContext) {
    let extension = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../extensions/db-explorer");
    let sidecar = crate::process::sidecar_path(&extension, "forge-sql");
    let Some(server) = std::env::var("FORGE_SQL_TEST_REDIS").ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) else {
        eprintln!("skipping: FORGE_SQL_TEST_REDIS not set");
        return;
    };
    if !extension.join("dist/extension.js").is_file() || !sidecar.is_file() {
        eprintln!("skipping: build extensions/db-explorer and its sidecar first");
        return;
    }
    let host_name = server["host"].as_str().unwrap_or("127.0.0.1").to_string();
    let port = server["port"].as_u64().unwrap_or(6379);
    let password = server["password"].as_str().unwrap_or_default().to_string();

    // Seed database 9 with the sidecar: 2,500 session keys (more than a batch) and 3 others.
    {
        use std::io::{BufRead, Write};
        let mut seed = std::process::Command::new(&sidecar).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        let mut stdin = seed.stdin.take().unwrap();
        let mut lines = std::io::BufReader::new(seed.stdout.take().unwrap()).lines();
        let mut id = 0;
        let mut call = |method: &str, params: Value| -> Value {
            id += 1;
            writeln!(stdin, "{}", json!({ "id": id, "method": method, "params": params })).unwrap();
            serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap()
        };
        let connected = call("connect", json!({ "connectionId": "seed", "engine": "redis", "host": host_name, "port": port, "password": password, "database": "9" }));
        assert!(connected.get("result").is_some(), "{connected}");
        let mut run = |line: String| call("redisCommand", json!({ "connectionId": "seed", "db": 9, "line": line }));
        run("FLUSHDB".into());
        run("HSET user:1 name Ada lang Analytical".into());
        run("HSET user:2 name Grace".into());
        run("SET config:theme dark".into());
        for chunk in (0..2500).collect::<Vec<_>>().chunks(500) {
            let args: Vec<String> = chunk.iter().map(|i| format!("session:{i:04} x")).collect();
            run(format!("MSET {}", args.join(" ")));
        }
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
    let confirm = |ui: &mut Ui, answer: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ui.cx.has_pending_prompt() {
            assert!(Instant::now() < deadline, "no confirmation");
            ui.cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
        }
        ui.cx.simulate_prompt_answer(answer);
    };

    // A Redis connection: host, port, password, database 9.
    let add = ui.by_label("db-explorer", "button", "Add Connection…");
    ui.send(add, "onClick", Value::Null);
    let form = ui.tab("db-explorer.connection:new");
    let engine = ui.find(&form, "select", "the engine select", |p| p.get("value") == Some(&json!("postgres")));
    ui.send(engine, "onChange", json!("redis"));
    let by_placeholder = |ui: &mut Ui, tab: &str, placeholder: &str| ui.find(tab, "input", placeholder, |p| p.get("placeholder").and_then(Value::as_str) == Some(placeholder));
    let input = by_placeholder(&mut ui, &form, "localhost");
    ui.send(input, "onChange", json!(host_name));
    let input = by_placeholder(&mut ui, &form, "6379");
    ui.send(input, "onChange", json!(port.to_string()));
    let input = ui.find(&form, "input", "the password", |p| p.get("password") == Some(&json!(true)));
    ui.send(input, "onChange", json!(password));
    let input = by_placeholder(&mut ui, &form, "0");
    ui.send(input, "onChange", json!("9"));
    let save = ui.by_label(&form, "button", "Save and Connect");
    ui.send(save, "onClick", Value::Null);

    // db9 with its key count; expanded, the first batch of keys as a tree.
    let db9 = ui.by_label("db-explorer", "treeItem", "db9");
    ui.wait("db9's key count", |h| (h.trees.get("db-explorer")?.get(db9)?.prop("description")? == &json!("2503 keys")).then_some(()));
    ui.send(db9, "onClick", Value::Null);
    let more = ui.by_label("db-explorer", "treeItem", "Load more keys…");
    ui.by_label("db-explorer", "treeItem", "config");
    let user = ui.by_label("db-explorer", "treeItem", "user");
    let session = ui.by_label("db-explorer", "treeItem", "session");
    let count = ui.prop("db-explorer", session, "description");
    assert!(count.as_str().unwrap().ends_with('+'), "a batch, not all of them: {count}");
    ui.send(more, "onClick", Value::Null);
    ui.wait("all the session keys", |h| {
        let tree = h.trees.get("db-explorer")?;
        let found = collect(tree, |n| matches!(&n.kind, NodeKind::Element { kind, props, .. } if kind == "treeItem" && props.get("label") == Some(&json!("session")) && props.get("description") == Some(&json!("2500"))));
        (!found.is_empty()).then_some(())
    });
    assert!(ui.host.read_with(ui.cx, |h, _| collect(&h.trees["db-explorer"], |n| matches!(&n.kind, NodeKind::Element { props, .. } if props.get("label") == Some(&json!("Load more keys…")))).is_empty()), "nothing more to load");

    // A filter: only the user keys. (The tree was redrawn: find the row again.)
    let _ = user;
    let user = ui.by_label("db-explorer", "treeItem", "user");
    ui.send(user, "onClick", Value::Null);
    let filter = ui.find("db-explorer", "input", "the key filter", |p| p.get("placeholder").and_then(Value::as_str).is_some_and(|s| s.starts_with("Filter keys in db9")));
    ui.send(filter, "onChange", json!("user:*"));
    ui.send(filter, "onSubmit", json!("user:*"));
    ui.wait("only the user keys", |h| {
        let tree = h.trees.get("db-explorer")?;
        let labels: Vec<String> = collect(tree, |n| matches!(&n.kind, NodeKind::Element { kind, .. } if kind == "treeItem")).into_iter().filter_map(|id| tree.get(id)?.str_prop("label").map(str::to_string)).collect();
        (labels.contains(&"user".to_string()) && !labels.contains(&"session".to_string()) && !labels.contains(&"config".to_string())).then_some(())
    });

    // Open user:1 (the namespace stays open across the filter), edit a value, save.
    let user = ui.by_label("db-explorer", "treeItem", "user");
    if ui.prop("db-explorer", user, "expanded") != json!(true) {
        ui.send(user, "onClick", Value::Null);
    }
    let user1 = ui.by_label("db-explorer", "treeItem", "1");
    assert_eq!(ui.prop("db-explorer", user1, "icon"), json!("hash"));
    ui.send(user1, "onDoubleClick", Value::Null);
    let tab = ui.tab("db-explorer.key:");
    let grid = ui.find(&tab, "grid", "the hash's fields", |p| p.get("rows").and_then(Value::as_array).is_some_and(|r| r.len() == 2));
    ui.cx.update(|window, _| window.refresh());
    ui.cx.run_until_parked();
    let selector: &'static str = Box::leak(format!("forge-grid-{tab}-{grid}").into_boxed_str());
    let bounds = ui.cx.debug_bounds(selector).expect("the fields table is drawn");
    assert!(bounds.size.height > gpui::px(100.), "the fields table is {:?} high", bounds.size.height);
    let rows = ui.prop(&tab, grid, "rows");
    let name_row = rows.as_array().unwrap().iter().position(|r| r[0] == json!("name")).unwrap();
    ui.send(grid, "onCellEdit", json!({ "row": name_row, "column": 1, "value": "Ada Lovelace" }));
    let save = ui.by_label(&tab, "button", "Save 1");
    ui.send(save, "onClick", Value::Null);
    confirm(&mut ui, "Save");
    ui.wait("the saved value", |h| {
        let rows = h.trees.get(&tab)?.get(grid)?.prop("rows")?.as_array()?.clone();
        rows.iter().any(|r| r[1] == json!("Ada Lovelace")).then_some(())
    });

    // A new list key.
    let db9 = ui.by_label("db-explorer", "treeItem", "db9");
    ui.send(db9, "onContextMenu", json!({ "id": "new-key" }));
    let new_key = ui.tab("db-explorer.newkey:");
    let name = by_placeholder(&mut ui, &new_key, "user:42:profile");
    ui.send(name, "onChange", json!("user:queue"));
    let kind = ui.find(&new_key, "select", "the type select", |p| p.get("value") == Some(&json!("string")));
    ui.send(kind, "onChange", json!("list"));
    let value = ui.find(&new_key, "input", "the first item", |p| p.get("multiline") != Some(&json!(true)) && p.get("placeholder").is_none_or(|v| v.is_null() || v == "") && p.get("value") == Some(&json!("")));
    ui.send(value, "onChange", json!("job-1"));
    let create = ui.by_label(&new_key, "button", "Create");
    ui.send(create, "onClick", Value::Null);
    let list_tab = ui.wait("the new key's tab", |h| h.trees.keys().find(|k| k.ends_with(":9:user:queue") && h.trees[*k].len() > 3).cloned());
    ui.find(&list_tab, "grid", "the list", |p| p.get("rows") == Some(&json!([[0, "job-1"]])));

    // The console.
    let db9 = ui.by_label("db-explorer", "treeItem", "db9");
    ui.send(db9, "onContextMenu", json!({ "id": "console" }));
    let console = ui.tab("db-explorer.console:");
    let line = ui.find(&console, "input", "the command line", |p| p.get("placeholder").and_then(Value::as_str).is_some_and(|s| s.starts_with("db9>")));
    ui.send(line, "onChange", json!("HGET user:1 name"));
    ui.send(line, "onSubmit", json!("HGET user:1 name"));
    ui.text(&console, "\"Ada Lovelace\"");
    ui.send(line, "onChange", json!("LLEN user:queue"));
    ui.send(line, "onSubmit", json!("LLEN user:queue"));
    ui.text(&console, "(integer) 1");
    ui.send(line, "onChange", json!("HGET user:queue x"));
    ui.send(line, "onSubmit", json!("HGET user:queue x"));
    ui.text(&console, "(error) WRONGTYPE");
}

/// Runs the Docker CLI for the containers test; `None` when it fails.
fn docker(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("docker").args(args).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Containers (extensions/containers) end to end with the real Docker CLI: a container that
/// Compose would have made shows under its project, agents can list it, and it can be removed.
/// Skipped when the extension isn't built or Docker isn't running or has no image to use.
#[gpui::test]
async fn containers_end_to_end(cx: &mut gpui::TestAppContext) {
    let extension = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../extensions/containers");
    if !extension.join("dist/extension.js").is_file() {
        eprintln!("skipping: build extensions/containers first");
        return;
    }
    let Some(image) = docker(&["images", "--format", "{{.ID}}"]).and_then(|ids| ids.lines().next().map(str::to_string)) else {
        eprintln!("skipping: Docker isn't running or has no images");
        return;
    };
    // A stopped container with Compose's labels (it is never started).
    let project = format!("forge-e2e-{}", std::process::id());
    let name = format!("{project}-web-1");
    let project_label = format!("com.docker.compose.project={project}");
    let Some(id) = docker(&["create", "--name", &name, "--label", &project_label, "--label", "com.docker.compose.service=web", &image]) else {
        eprintln!("skipping: couldn't create a container");
        return;
    };
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            docker(&["rm", "-f", &self.0]);
        }
    }
    let _cleanup = Cleanup(id.clone());

    cx.executor().allow_parking();
    let params = cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        crate::init_for_tests(cx);
    });
    let tmp = tempfile::tempdir().unwrap();
    let host = cx.update(|cx| ExtensionHost::init(vec![extension.clone()], cx)).unwrap();
    params.fs.as_fake().insert_tree(tmp.path(), json!({})).await;
    let workspace_project = project::Project::test(params.fs.clone(), [tmp.path()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(workspace_project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let weak = workspace.downgrade();
    cx.update(|window, cx| host.update(cx, |h, cx| h.set_workspace(weak, window.window_handle(), cx)));
    let mut ui = Ui { host: host.clone(), cx };

    // The project, with its one stopped container under it.
    let project_row = ui.by_label("containers", "treeItem", &project);
    assert_eq!(ui.prop("containers", project_row, "description"), json!("0/1 running"));
    let web = ui.by_label("containers", "treeItem", "web");
    assert!(ui.prop("containers", web, "description").as_str().is_some_and(|d| d.starts_with("Created")), "{:?}", ui.prop("containers", web, "description"));

    // Agents see it too.
    let tools = forge_ui::agent_tools::agent_tools();
    let listed = ui.cx.background_executor.spawn(tools.run("containers__containers", json!({}), tmp.path().to_path_buf()));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !listed.is_ready() {
        assert!(Instant::now() < deadline, "the containers tool didn't answer");
        ui.cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
    let listed = listed.await.unwrap();
    assert!(listed.contains(&format!("Compose project {project}")) && listed.contains(&format!("- web: {name}")), "{listed}");

    // Remove it from its menu (after confirming): it leaves the panel.
    ui.send(web, "onContextMenu", json!({ "id": "rm" }));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ui.cx.has_pending_prompt() {
        assert!(Instant::now() < deadline, "no confirmation before removing");
        ui.cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
    ui.cx.simulate_prompt_answer("Remove");
    ui.wait("the project gone", |h| {
        let tree = h.trees.get("containers")?;
        let left = collect(tree, |n| matches!(&n.kind, NodeKind::Element { kind, props, .. } if kind == "treeItem" && props.get("label").and_then(Value::as_str) == Some(project.as_str())));
        left.is_empty().then_some(())
    });
    assert!(docker(&["inspect", &id]).is_none(), "the container was removed");
}
