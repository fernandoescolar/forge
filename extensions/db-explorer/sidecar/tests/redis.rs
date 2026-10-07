//! Redis, gated on env vars holding connect JSON:
//!   FORGE_SQL_TEST_REDIS          a server, e.g. '{"host":"localhost","port":6379,"password":"pw"}'
//!   FORGE_SQL_TEST_REDIS_SENTINEL Sentinel, e.g. '{"sentinels":"localhost:26379","masterName":"mymaster","password":"pw"}'
//! Both run the same scenario on database 9, which they empty first.

mod common;

use common::{Sidecar, env_connection};
use serde_json::{Value, json};

#[test]
fn redis_server() {
    let Some(conn) = env_connection("FORGE_SQL_TEST_REDIS") else {
        eprintln!("FORGE_SQL_TEST_REDIS not set; skipping");
        return;
    };
    scenario(conn);
}

#[test]
fn redis_sentinel() {
    let Some(conn) = env_connection("FORGE_SQL_TEST_REDIS_SENTINEL") else {
        eprintln!("FORGE_SQL_TEST_REDIS_SENTINEL not set; skipping");
        return;
    };
    let info = scenario(conn);
    assert!(info["serverVersion"].as_str().unwrap().contains("Redis"), "the master Sentinel named: {info}");
}

fn scenario(mut conn: Value) -> Value {
    conn["engine"] = json!("redis");
    conn["connectionId"] = json!("r");
    conn["database"] = json!("9");
    let mut sc = Sidecar::spawn();
    let info = sc.ok("connect", conn.clone());
    assert_eq!(info["engine"], "redis");
    assert_eq!(info["defaultDatabase"], "9");
    let db = |extra: Value| {
        let mut v = json!({ "connectionId": "r", "db": 9 });
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    };
    let out = |sc: &mut Sidecar, line: &str| sc.ok("redisCommand", db(json!({ "line": line })))["output"].as_str().unwrap().to_string();
    assert_eq!(out(&mut sc, "FLUSHDB"), "OK");

    // The console, as redis-cli shows it.
    assert_eq!(out(&mut sc, r#"SET greeting "hello world""#), "OK");
    assert_eq!(out(&mut sc, "GET greeting"), "\"hello world\"");
    assert_eq!(out(&mut sc, "INCR visits"), "(integer) 1");
    assert_eq!(out(&mut sc, "GET missing"), "(nil)");
    assert!(out(&mut sc, "HGET greeting x").starts_with("(error) WRONGTYPE"), "server errors are replies");
    assert!(sc.err("redisCommand", db(json!({ "line": "SUBSCRIBE news" })))["message"].as_str().unwrap().contains("can't run in the console"));
    assert!(sc.err("redisCommand", db(json!({ "line": "SELECT 2" })))["message"].as_str().unwrap().contains("database above"));

    // One key of each type, created as the explorer does.
    for (key, kind, field, value, score) in [
        ("user:1", "hash", Some("name"), "Ada", None),
        ("user:2", "hash", Some("name"), "Grace", None),
        ("queue:jobs", "list", None, "first", None),
        ("tags", "set", None, "rust", None),
        ("board", "zset", None, "ada", Some(10.0)),
        ("events", "stream", Some("type"), "login", None),
        ("bin", "string", None, r"\x00\xffok", None),
    ] {
        let escaped = key == "bin";
        sc.ok("editKey", db(json!({ "key": key, "op": "create", "type": kind, "field": field, "value": value, "score": score, "escaped": escaped })));
    }
    let e = sc.err("editKey", db(json!({ "key": "user:1", "op": "create", "type": "string", "value": "x" })));
    assert!(e["message"].as_str().unwrap().contains("already exists"), "{e}");

    // Databases and SCAN.
    let dbs = sc.ok("redisDatabases", json!({ "connectionId": "r" }));
    assert_eq!(dbs.as_array().unwrap().iter().find(|d| d["db"] == 9).unwrap()["keys"], 9);
    let scanned = scan_all(&mut sc, "user:*");
    assert_eq!(scanned, [("user:1".to_string(), "hash".to_string()), ("user:2".to_string(), "hash".to_string())]);
    assert_eq!(scan_all(&mut sc, "*").len(), 9);

    // Hash: page, set, delete.
    sc.ok("editKey", db(json!({ "key": "user:1", "op": "hashSet", "field": "lang", "value": "Analytical Engine" })));
    let user = sc.ok("getKey", db(json!({ "key": "user:1" })));
    assert_eq!((user["type"].as_str(), user["length"].as_u64(), user["ttl"].as_i64()), (Some("hash"), Some(2), Some(-1)));
    let mut rows: Vec<Value> = user["rows"].as_array().unwrap().clone();
    rows.sort_by_key(|r| r[0].as_str().unwrap().to_string());
    assert_eq!(rows, [json!(["lang", "Analytical Engine"]), json!(["name", "Ada"])]);
    sc.ok("editKey", db(json!({ "key": "user:1", "op": "hashDelete", "fields": ["lang"] })));
    assert_eq!(sc.ok("getKey", db(json!({ "key": "user:1" })))["length"], 1);

    // List: push, set by index, delete by index, page.
    for v in ["second", "third", "fourth"] {
        sc.ok("editKey", db(json!({ "key": "queue:jobs", "op": "listPush", "value": v })));
    }
    sc.ok("editKey", db(json!({ "key": "queue:jobs", "op": "listPush", "value": "zeroth", "head": true })));
    sc.ok("editKey", db(json!({ "key": "queue:jobs", "op": "listSet", "index": 2, "value": "SECOND" })));
    sc.ok("editKey", db(json!({ "key": "queue:jobs", "op": "listDelete", "indexes": [0, 4] })));
    let list = sc.ok("getKey", db(json!({ "key": "queue:jobs", "limit": 2 })));
    assert_eq!(list["rows"], json!([[0, "first"], [1, "SECOND"]]));
    assert_eq!(list["next"], 2);
    let rest = sc.ok("getKey", db(json!({ "key": "queue:jobs", "offset": 2, "limit": 2 })));
    assert_eq!(rest["rows"], json!([[2, "third"]]));
    assert_eq!(rest["next"], Value::Null);

    // Set and sorted set: add, rename a member, delete.
    sc.ok("editKey", db(json!({ "key": "tags", "op": "setAdd", "member": "go" })));
    sc.ok("editKey", db(json!({ "key": "tags", "op": "setRename", "member": "go", "to": "zig" })));
    sc.ok("editKey", db(json!({ "key": "tags", "op": "setDelete", "members": ["rust"] })));
    assert_eq!(sc.ok("getKey", db(json!({ "key": "tags" })))["rows"], json!([["zig"]]));
    sc.ok("editKey", db(json!({ "key": "board", "op": "zSetAdd", "member": "grace", "score": 20.5 })));
    sc.ok("editKey", db(json!({ "key": "board", "op": "zSetAdd", "member": "ada", "score": 30 })));
    sc.ok("editKey", db(json!({ "key": "board", "op": "zSetRename", "member": "grace", "to": "hopper" })));
    assert_eq!(sc.ok("getKey", db(json!({ "key": "board" })))["rows"], json!([["hopper", 20.5], ["ada", 30.0]]));

    // Stream: add, page by id, delete.
    for i in 0..3 {
        sc.ok("editKey", db(json!({ "key": "events", "op": "streamAdd", "fields": [["type", format!("click{i}")], ["user", "1"]] })));
    }
    let first = sc.ok("getKey", db(json!({ "key": "events", "limit": 2 })));
    assert_eq!(first["length"], 4);
    assert_eq!(first["rows"][0][1], r#"{"type":"login"}"#);
    let after = first["next"].as_str().unwrap().to_string();
    let rest = sc.ok("getKey", db(json!({ "key": "events", "cursor": after, "limit": 2 })));
    assert_eq!(rest["rows"][0][1], r#"{"type":"click1","user":"1"}"#);
    let id = rest["rows"][0][0].as_str().unwrap().to_string();
    sc.ok("editKey", db(json!({ "key": "events", "op": "streamDelete", "ids": [id] })));
    assert_eq!(sc.ok("getKey", db(json!({ "key": "events" })))["length"], 3);

    // A binary string: shown escaped, written back as the same bytes.
    let bin = sc.ok("getKey", db(json!({ "key": "bin" })));
    assert_eq!((bin["text"].as_str(), bin["escaped"].as_bool(), bin["length"].as_u64()), (Some(r"\x00\xffok"), Some(true), Some(4)));
    sc.ok("editKey", db(json!({ "key": "bin", "op": "setString", "value": r"\x00\xffOK", "escaped": true })));
    assert_eq!(out(&mut sc, "STRLEN bin"), "(integer) 4");
    assert_eq!(out(&mut sc, "GET bin"), r#""\x00\xffOK""#);

    // TTL, rename, delete.
    sc.ok("expireKey", db(json!({ "key": "greeting", "ttl": 100 })));
    let ttl = sc.ok("getKey", db(json!({ "key": "greeting" })))["ttl"].as_i64().unwrap();
    assert!((95..=100).contains(&ttl), "{ttl}");
    sc.ok("editKey", db(json!({ "key": "greeting", "op": "setString", "value": "hi" })));
    assert!(sc.ok("getKey", db(json!({ "key": "greeting" })))["ttl"].as_i64().unwrap() > 0, "editing keeps the TTL");
    sc.ok("expireKey", db(json!({ "key": "greeting", "ttl": null })));
    assert_eq!(sc.ok("getKey", db(json!({ "key": "greeting" })))["ttl"], -1);
    sc.ok("renameKey", db(json!({ "key": "user:2", "to": "user:3" })));
    let e = sc.err("renameKey", db(json!({ "key": "user:3", "to": "user:1" })));
    assert!(e["message"].as_str().unwrap().contains("already exists"), "{e}");
    let deleted = sc.ok("deleteKeys", db(json!({ "keys": [["user:3", false], ["visits", false], ["nope", false]] })));
    assert_eq!(deleted["deleted"], 2);
    let e = sc.err("getKey", db(json!({ "key": "user:3" })));
    assert!(e["message"].as_str().unwrap().contains("doesn't exist"), "{e}");

    sc.ok("disconnect", json!({ "connectionId": "r" }));
    assert_eq!(sc.err("getKey", db(json!({ "key": "user:1" })))["code"], "not_connected");
    sc.shutdown();
    info
}

/// Every key matching `pattern` (SCAN to the end), sorted.
fn scan_all(sc: &mut Sidecar, pattern: &str) -> Vec<(String, String)> {
    let mut cursor = "0".to_string();
    let mut keys = Vec::new();
    loop {
        let r = sc.ok("scanKeys", json!({ "connectionId": "r", "db": 9, "pattern": pattern, "cursor": cursor, "count": 2 }));
        keys.extend(r["keys"].as_array().unwrap().iter().map(|k| (k["name"].as_str().unwrap().to_string(), k["type"].as_str().unwrap().to_string())));
        cursor = r["cursor"].as_str().unwrap().to_string();
        if cursor == "0" {
            break;
        }
    }
    keys.sort();
    keys.dedup();
    keys
}
