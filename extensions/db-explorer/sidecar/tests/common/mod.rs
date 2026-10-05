#![allow(dead_code)]
//! Test client: spawns the built `forge-sql` binary and talks JSON lines to it.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

pub struct Sidecar {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: mpsc::Receiver<Value>,
    next_id: u64,
    pending: HashMap<u64, Value>,
}

impl Sidecar {
    pub fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_forge-sql"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn forge-sql");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad response line {line:?}: {e}"));
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        Self { child, stdin, rx, next_id: 1, pending: HashMap::new() }
    }

    pub fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    pub fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send_raw(&json!({ "id": id, "method": method, "params": params }).to_string());
        id
    }

    pub fn next_response(&mut self) -> Value {
        self.rx.recv_timeout(Duration::from_secs(90)).expect("timed out waiting for a response")
    }

    /// Wait for the response with the given id (buffering others).
    pub fn wait(&mut self, id: u64) -> Value {
        if let Some(v) = self.pending.remove(&id) {
            return v;
        }
        loop {
            let v = self.next_response();
            let rid = v["id"].as_u64().expect("numeric id");
            if rid == id {
                return v;
            }
            self.pending.insert(rid, v);
        }
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.send(method, params);
        let mut v = self.wait(id);
        match v.get_mut("error") {
            Some(e) => Err(e.take()),
            None => Ok(v["result"].take()),
        }
    }

    pub fn ok(&mut self, method: &str, params: Value) -> Value {
        self.call(method, params.clone()).unwrap_or_else(|e| panic!("{method} {params} failed: {e}"))
    }

    pub fn err(&mut self, method: &str, params: Value) -> Value {
        match self.call(method, params.clone()) {
            Ok(v) => panic!("{method} {params} unexpectedly succeeded: {v}"),
            Err(e) => e,
        }
    }

    /// Close stdin and wait for a clean exit.
    pub fn shutdown(mut self) {
        self.stdin.take();
        for _ in 0..100 {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "sidecar exited with {status}");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        panic!("sidecar did not exit after stdin closed");
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// Per-engine SQL used by the shared scenario.
pub struct EngineSql {
    pub engine: &'static str,
    pub setup: Vec<&'static str>,
    /// Expected `price` cell for the first row (decimal -> string, SQLite stores REAL).
    pub price: Value,
    /// Expected `created` cell for the first row.
    pub created: Value,
    /// A query that runs for a long time (used for cancellation).
    pub slow: &'static str,
    /// Schema to pass for object-level calls (None where databases act as schemas).
    pub schema: Option<&'static str>,
    pub db_in_list: &'static str,
    /// Whether DML statements report rowsAffected in `query`.
    pub reports_rows_affected: bool,
}

/// Exercise every method end-to-end against one connection.
pub fn run_scenario(sc: &mut Sidecar, mut connect: Value, e: &EngineSql) {
    connect["connectionId"] = json!("c1");
    let info = sc.ok("connect", connect.clone());
    assert!(!info["serverVersion"].as_str().unwrap().is_empty(), "{info}");
    let engine = info["engine"].as_str().unwrap();
    assert!(engine == e.engine || (e.engine == "mysql" && engine == "mariadb"), "{info}");

    for sql in &e.setup {
        sc.ok("query", json!({ "connectionId": "c1", "sql": sql }));
    }

    // ---- metadata
    let dbs = sc.ok("listDatabases", json!({ "connectionId": "c1" }));
    assert!(dbs.as_array().unwrap().iter().any(|d| d == e.db_in_list), "{dbs}");
    let schemas = sc.ok("listSchemas", json!({ "connectionId": "c1" }));
    match e.schema {
        Some(s) => assert!(schemas.as_array().unwrap().iter().any(|x| x == s), "{schemas}"),
        None => assert_eq!(schemas, json!([])),
    }
    let objs = sc.ok("listObjects", json!({ "connectionId": "c1", "schema": e.schema }));
    let objs = objs.as_array().unwrap();
    let find = |name: &str| objs.iter().find(|o| o["name"] == name).cloned();
    assert_eq!(find("forge_items").expect("table listed")["kind"], "table");
    assert_eq!(find("forge_view").expect("view listed")["kind"], "view");
    let kinds: Vec<&str> = objs.iter().map(|o| o["kind"].as_str().unwrap()).collect();
    let mut sorted = kinds.clone();
    sorted.sort();
    assert_eq!(kinds, sorted, "objects sorted by kind");

    let desc = sc.ok("describe", json!({ "connectionId": "c1", "schema": e.schema, "table": "forge_items" }));
    let cols = desc["columns"].as_array().unwrap();
    let names: Vec<&str> = cols.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["id", "name", "price", "created", "data", "flag", "big"]);
    assert_eq!(desc["primaryKey"], json!(["id"]));
    assert_eq!(cols[0]["primaryKey"], true);
    assert_eq!(cols[0]["autoIncrement"], true, "{desc}");
    assert_eq!(cols[1]["nullable"], false);
    assert_eq!(cols[2]["nullable"], true);
    assert!(sc.call("describe", json!({ "connectionId": "c1", "schema": e.schema, "table": "nope" })).is_err());

    // ---- query + cell encoding
    let r = sc.ok(
        "query",
        json!({ "connectionId": "c1", "sql": "SELECT id, name, price, created, data, flag, big FROM forge_items ORDER BY id" }),
    );
    assert!(r["elapsedMs"].is_u64());
    let set = &r["resultSets"][0];
    assert_eq!(set["columns"].as_array().unwrap().len(), 7);
    assert_eq!(set["truncated"], false);
    assert!(set["columns"][0]["type"].is_string());
    let row = &set["rows"][0];
    assert_eq!(row[0], 1, "{set}");
    assert_eq!(row[1], "alpha");
    assert_eq!(row[2], e.price, "{set}");
    assert_eq!(row[3], e.created, "{set}");
    assert_eq!(row[4], "0xdeadbeef");
    assert_eq!(row[5], true, "{set}");
    assert_eq!(row[6], "9007199254740993");
    assert_eq!(set["rows"][1][2], Value::Null);
    assert_eq!(set["rows"][1][6], 1);

    // truncation
    let r = sc.ok("query", json!({ "connectionId": "c1", "sql": "SELECT name FROM forge_items ORDER BY id", "maxRows": 2 }));
    assert_eq!(r["resultSets"][0]["rows"].as_array().unwrap().len(), 2);
    assert_eq!(r["resultSets"][0]["truncated"], true);
    // the connection is still usable afterwards
    sc.ok("query", json!({ "connectionId": "c1", "sql": "SELECT 1" }));

    // empty result keeps its columns
    let r = sc.ok("query", json!({ "connectionId": "c1", "sql": "SELECT id, name FROM forge_items WHERE 1 = 0" }));
    let set = &r["resultSets"][0];
    assert_eq!(set["rows"], json!([]));
    assert_eq!(set["columns"].as_array().unwrap().len(), 2, "{set}");
    assert_eq!(set["rowsAffected"], Value::Null);

    // multiple statements / result sets
    let r = sc.ok("query", json!({ "connectionId": "c1", "sql": "SELECT 1 AS a; SELECT 'x' AS b" }));
    let sets = r["resultSets"].as_array().unwrap();
    assert_eq!(sets.len(), 2, "{r}");
    assert_eq!(sets[0]["rows"][0][0], 1);
    assert_eq!(sets[1]["rows"][0][0], "x");
    assert_eq!(sets[1]["columns"][0]["name"], "b");

    if e.reports_rows_affected {
        let r = sc.ok("query", json!({ "connectionId": "c1", "sql": "UPDATE forge_items SET name = name WHERE id <= 2" }));
        assert_eq!(r["resultSets"][0]["rowsAffected"], 2, "{r}");
        assert_eq!(r["resultSets"][0]["columns"], json!([]));
    }

    let err = sc.err("query", json!({ "connectionId": "c1", "sql": "SELECT * FROM no_such_table_xyz" }));
    assert!(err["message"].as_str().unwrap().to_lowercase().contains("no_such_table_xyz"), "{err}");

    // ---- fetchTable
    let page = sc.ok(
        "fetchTable",
        json!({ "connectionId": "c1", "schema": e.schema, "table": "forge_items", "offset": 1, "limit": 1,
                "orderBy": [{ "column": "id", "desc": true }] }),
    );
    assert_eq!(page["total"], 3, "{page}");
    assert_eq!(page["rows"].as_array().unwrap().len(), 1);
    assert_eq!(page["rows"][0][1], "beta");
    assert_eq!(page["columns"][1]["name"], "name");
    let page = sc.ok(
        "fetchTable",
        json!({ "connectionId": "c1", "schema": e.schema, "table": "forge_items", "offset": 0, "limit": 50, "where": "name <> 'beta'" }),
    );
    assert_eq!(page["total"], 2);
    assert_eq!(page["rows"].as_array().unwrap().len(), 2);
    let page = sc.ok(
        "fetchTable",
        json!({ "connectionId": "c1", "schema": e.schema, "table": "forge_items", "offset": 0, "limit": 10, "where": "1 = 0" }),
    );
    assert_eq!(page["total"], 0);
    assert_eq!(page["columns"].as_array().unwrap().len(), 7, "empty page keeps columns: {page}");

    // ---- applyChanges
    let base = json!({ "connectionId": "c1", "schema": e.schema, "table": "forge_items" });
    let with = |changes: Value| {
        let mut p = base.clone();
        p["changes"] = changes;
        p
    };
    let r = sc.ok(
        "applyChanges",
        with(json!([
            { "kind": "update", "key": { "id": "1" }, "values": { "name": "ALPHA", "price": "9.99", "flag": false } },
            { "kind": "insert", "values": { "name": "delta", "price": 4, "big": "42" } },
            { "kind": "delete", "key": { "id": 2 } },
        ])),
    );
    assert_eq!(r["applied"], 3);
    let r = sc.ok("query", json!({ "connectionId": "c1", "sql": "SELECT name, big FROM forge_items ORDER BY id" }));
    let rows = &r["resultSets"][0]["rows"];
    assert_eq!(rows.as_array().unwrap().len(), 3, "{rows}");
    assert_eq!(rows[0][0], "ALPHA");
    assert_eq!(rows[1][0], "gamma");
    assert_eq!(rows[2], json!(["delta", 42]));

    // a failing change rolls back the whole batch and reports its index
    let err = sc.err(
        "applyChanges",
        with(json!([
            { "kind": "insert", "values": { "name": "epsilon" } },
            { "kind": "update", "key": { "id": 12345 }, "values": { "name": "ghost" } },
        ])),
    );
    assert_eq!(err["index"], 1, "{err}");
    assert!(err["message"].as_str().unwrap().contains("changed or deleted by someone else"), "{err}");
    let err = sc.err("applyChanges", with(json!([{ "kind": "insert", "values": { "name": null } }])));
    assert_eq!(err["index"], 0, "{err}");
    let r = sc.ok("query", json!({ "connectionId": "c1", "sql": "SELECT COUNT(*) FROM forge_items" }));
    assert_eq!(r["resultSets"][0]["rows"][0][0], 3, "rolled back");

    // ---- cancel: a slow query must not block other requests, and is aborted on request
    let qid = sc.send("query", json!({ "connectionId": "c1", "sql": e.slow, "requestId": "slow-1" }));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(sc.ok("ping", json!({})), "pong");
    let r = sc.ok("cancel", json!({ "requestId": "slow-1" }));
    assert_eq!(r["cancelled"], true);
    let resp = sc.wait(qid);
    assert_eq!(resp["error"]["code"], "cancelled", "{resp}");
    assert_eq!(sc.ok("cancel", json!({ "requestId": "slow-1" }))["cancelled"], false);
    let r = sc.ok("query", json!({ "connectionId": "c1", "sql": "SELECT COUNT(*) FROM forge_items" }));
    assert_eq!(r["resultSets"][0]["rows"][0][0], 3);

    // ---- reconnect replaces, disconnect removes
    sc.ok("connect", connect);
    assert_eq!(sc.ok("disconnect", json!({ "connectionId": "c1" })), Value::Null);
    let err = sc.err("listDatabases", json!({ "connectionId": "c1" }));
    assert_eq!(err["code"], "not_connected");
}

/// Connection JSON from an env var, or None (test skipped).
pub fn env_connection(var: &str) -> Option<Value> {
    let raw = std::env::var(var).ok().filter(|s| !s.trim().is_empty())?;
    Some(serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{var} is not valid JSON: {e}")))
}
