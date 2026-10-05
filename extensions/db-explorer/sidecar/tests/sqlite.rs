//! End-to-end tests over the JSON-lines protocol against a temporary SQLite database.

mod common;

use common::{EngineSql, Sidecar, run_scenario};
use serde_json::{Value, json};

const SETUP: &[&str] = &[
    "DROP VIEW IF EXISTS forge_view; DROP TABLE IF EXISTS forge_items",
    "CREATE TABLE forge_items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, price NUMERIC, created DATETIME,
        data BLOB, flag BOOLEAN, big INTEGER)",
    "CREATE VIEW forge_view AS SELECT id, name FROM forge_items",
    "INSERT INTO forge_items (name, price, created, data, flag, big) VALUES
        ('alpha', 1.50, '2024-01-02 03:04:05', X'DEADBEEF', 1, 9007199254740993),
        ('beta', NULL, NULL, NULL, 0, 1),
        ('gamma', 3.25, '2024-05-06 07:08:09', NULL, NULL, NULL)",
];

fn temp_db() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    // create an empty database file (the sidecar refuses to create missing files)
    std::fs::File::create(&path).unwrap();
    (dir, path.to_string_lossy().into_owned())
}

#[test]
fn sqlite_scenario() {
    let (_dir, path) = temp_db();
    let mut sc = Sidecar::spawn();
    let e = EngineSql {
        engine: "sqlite",
        setup: SETUP.to_vec(),
        price: json!(1.5),
        created: json!("2024-01-02 03:04:05"),
        slow: "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c",
        schema: None,
        db_in_list: "main",
        reports_rows_affected: true,
    };
    run_scenario(&mut sc, json!({ "engine": "sqlite", "file": path }), &e);
    sc.shutdown();
}

#[test]
fn sqlite_specifics() {
    let (dir, path) = temp_db();
    let mut sc = Sidecar::spawn();

    // missing files are an error (create_if_missing = false)
    let missing = dir.path().join("missing.db");
    let err = sc.err("connect", json!({ "connectionId": "x", "engine": "sqlite", "file": missing }));
    assert!(!err["message"].as_str().unwrap().is_empty());

    let info = sc.ok("connect", json!({ "connectionId": "s", "engine": "sqlite", "file": path }));
    assert_eq!(info["engine"], "sqlite");
    assert_eq!(info["defaultDatabase"], "main");

    // attached databases show up and their objects are qualified by schema
    let other = dir.path().join("other.db");
    std::fs::File::create(&other).unwrap();
    let attach = format!("ATTACH DATABASE '{}' AS other", other.display());
    sc.ok("query", json!({ "connectionId": "s", "sql": attach }));
    sc.ok("query", json!({ "connectionId": "s", "sql": "CREATE TABLE other.\"we\"\"ird\" (a INTEGER, b TEXT, PRIMARY KEY (b, a))" }));
    let dbs = sc.ok("listDatabases", json!({ "connectionId": "s" }));
    assert_eq!(dbs, json!(["main", "other"]));
    let objs = sc.ok("listObjects", json!({ "connectionId": "s", "database": "other" }));
    assert_eq!(objs, json!([{ "name": "we\"ird", "schema": "other", "kind": "table" }]));
    let desc = sc.ok("describe", json!({ "connectionId": "s", "database": "other", "table": "we\"ird" }));
    assert_eq!(desc["primaryKey"], json!(["b", "a"]));
    assert_eq!(desc["columns"][0]["autoIncrement"], false);
    let r = sc.ok(
        "applyChanges",
        json!({ "connectionId": "s", "database": "other", "table": "we\"ird",
                "changes": [{ "kind": "insert", "values": { "a": 1, "b": "x" } }, { "kind": "insert", "values": {} }] }),
    );
    assert_eq!(r["applied"], 2);
    let page = sc.ok("fetchTable", json!({ "connectionId": "s", "database": "other", "table": "we\"ird", "offset": 0, "limit": 10 }));
    assert_eq!(page["total"], 2);

    // floats, NaN-free reals, text that looks like numbers, and big blobs
    let r = sc.ok(
        "query",
        json!({ "connectionId": "s", "sql": "SELECT 1.25, '007', zeroblob(300), NULL, 1e308 * 10, -9223372036854775808" }),
    );
    let row = &r["resultSets"][0]["rows"][0];
    assert_eq!(row[0], 1.25);
    assert_eq!(row[1], "007");
    let blob = row[2].as_str().unwrap();
    assert!(blob.starts_with("0x0000") && blob.ends_with('…') && blob.chars().count() == 2 + 512 + 1);
    assert_eq!(row[3], Value::Null);
    assert_eq!(row[4], "Infinity");
    assert_eq!(row[5], "-9223372036854775808");

    // statements without rows report rowsAffected
    let r = sc.ok("query", json!({ "connectionId": "s", "sql": "CREATE TABLE t2 (x); INSERT INTO t2 VALUES (1), (2); SELECT * FROM t2" }));
    let sets = r["resultSets"].as_array().unwrap();
    assert_eq!(sets.len(), 3, "{r}");
    assert_eq!(sets[1]["rowsAffected"], 2);
    assert_eq!(sets[2]["rows"], json!([[1], [2]]));
    sc.shutdown();
}

#[test]
fn protocol_errors_and_concurrency() {
    let mut sc = Sidecar::spawn();
    assert_eq!(sc.ok("ping", Value::Null), "pong");

    let err = sc.err("frobnicate", json!({}));
    assert_eq!(err["code"], "unknown_method");
    let err = sc.err("listDatabases", json!({ "connectionId": "nope" }));
    assert_eq!(err["code"], "not_connected");
    let err = sc.err("describe", json!({ "connectionId": "nope" }));
    assert_eq!(err["code"], "invalid_params");
    let err = sc.err("connect", json!({ "connectionId": "x", "engine": "oracle" }));
    assert!(err["message"].as_str().unwrap().contains("oracle"));

    sc.send_raw("this is not json");
    let resp = sc.next_response();
    assert_eq!(resp["id"], Value::Null);
    assert_eq!(resp["error"]["code"], "parse_error");

    // responses may arrive out of order: a slow query does not block a ping
    let (_dir, path) = temp_db();
    sc.ok("connect", json!({ "connectionId": "s", "engine": "sqlite", "file": path }));
    let slow = sc.send(
        "query",
        json!({ "connectionId": "s", "requestId": 99,
                "sql": "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c" }),
    );
    let ping = sc.send("ping", json!({}));
    let first = sc.next_response();
    assert_eq!(first["id"], ping);
    assert_eq!(sc.ok("cancel", json!({ "requestId": 99 }))["cancelled"], true);
    assert_eq!(sc.wait(slow)["error"]["code"], "cancelled");
    // the interrupted connection is healthy again
    let objs = sc.ok("listObjects", json!({ "connectionId": "s" }));
    assert_eq!(objs, json!([]));
    sc.shutdown();
}

#[test]
fn requests_wait_for_inflight_connect() {
    let (dir, path) = temp_db();
    let mut sc = Sidecar::spawn();

    // requests pipelined right behind connect (no waiting) are served once it completes
    let c = sc.send("connect", json!({ "connectionId": "w", "engine": "sqlite", "file": path }));
    let q = sc.send("query", json!({ "connectionId": "w", "sql": "SELECT 41 + 1" }));
    let l = sc.send("listDatabases", json!({ "connectionId": "w" }));
    assert_eq!(sc.wait(q)["result"]["resultSets"][0]["rows"], json!([[42]]));
    assert_eq!(sc.wait(l)["result"], json!(["main"]));
    assert_eq!(sc.wait(c)["result"]["engine"], "sqlite");

    // ...and fail with the connect's own error when it fails
    let missing = dir.path().join("missing.db");
    let c = sc.send("connect", json!({ "connectionId": "bad", "engine": "sqlite", "file": missing }));
    let q = sc.send("listObjects", json!({ "connectionId": "bad" }));
    let connect_err = sc.wait(c)["error"].clone();
    assert!(connect_err.is_object(), "connect should fail");
    assert_eq!(sc.wait(q)["error"], connect_err);

    // ids that never had a connect still fail immediately
    assert_eq!(sc.err("listDatabases", json!({ "connectionId": "never" }))["code"], "not_connected");
    // after the failed connect finished, the id is simply not connected
    assert_eq!(sc.err("listDatabases", json!({ "connectionId": "bad" }))["code"], "not_connected");
    sc.shutdown();
}
