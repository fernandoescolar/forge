//! PostgreSQL / MySQL (MariaDB) / SQL Server tests, gated on env vars holding connect JSON:
//!   FORGE_SQL_TEST_PG, FORGE_SQL_TEST_MYSQL, FORGE_SQL_TEST_MSSQL
//! e.g. FORGE_SQL_TEST_PG='{"host":"localhost","port":5432,"user":"postgres","password":"pw","database":"postgres","ssl":"disable"}'

mod common;

use common::{EngineSql, Sidecar, env_connection, run_scenario};
use serde_json::{Value, json};

fn with_engine(mut v: Value, engine: &str) -> Value {
    if v.get("engine").is_none() {
        v["engine"] = json!(engine);
    }
    v
}

#[test]
fn postgres() {
    let Some(conn) = env_connection("FORGE_SQL_TEST_PG") else {
        eprintln!("FORGE_SQL_TEST_PG not set; skipping");
        return;
    };
    let conn = with_engine(conn, "postgres");
    let db = conn["database"].as_str().unwrap_or("postgres").to_string();
    let e = EngineSql {
        engine: "postgres",
        setup: vec![
            "DROP VIEW IF EXISTS forge_view; DROP TABLE IF EXISTS forge_items; DROP TABLE IF EXISTS forge_types",
            "CREATE TABLE forge_items (id serial PRIMARY KEY, name varchar(50) NOT NULL, price numeric(10,2),
                created timestamp, data bytea, flag boolean, big bigint)",
            "CREATE VIEW forge_view AS SELECT id, name FROM forge_items",
            r"INSERT INTO forge_items (name, price, created, data, flag, big) VALUES
                ('alpha', 1.50, '2024-01-02 03:04:05', '\xdeadbeef', true, 9007199254740993),
                ('beta', NULL, NULL, NULL, false, 1),
                ('gamma', 3.25, '2024-05-06 07:08:09', NULL, NULL, NULL)",
        ],
        price: json!("1.50"),
        created: json!("2024-01-02T03:04:05"),
        slow: "SELECT pg_sleep(30)",
        schema: Some("public"),
        db_in_list: Box::leak(db.into_boxed_str()),
        reports_rows_affected: true,
    };
    let mut sc = Sidecar::spawn();
    run_scenario(&mut sc, conn.clone(), &e);

    // typed cells and CAST-based binding for uuid/json/date/arrays
    let mut c = conn.clone();
    c["connectionId"] = json!("p");
    sc.ok("connect", c);
    sc.ok(
        "query",
        json!({ "connectionId": "p", "sql": "CREATE TABLE forge_types (id uuid PRIMARY KEY, doc jsonb, d date,
               ts timestamptz, tags text[], n numeric, f float8, i interval)" }),
    );
    let r = sc.ok(
        "applyChanges",
        json!({ "connectionId": "p", "table": "forge_types", "changes": [{ "kind": "insert", "values": {
            "id": "6f1c9d2e-0000-4000-8000-000000000001", "doc": { "a": [1, 2] }, "d": "2024-02-29",
            "ts": "2024-01-01T10:00:00Z", "tags": "{x,y}", "n": "123456789012345678901234567890.5", "f": 1.5, "i": "1 day" } }] }),
    );
    assert_eq!(r["applied"], 1);
    let r = sc.ok("query", json!({ "connectionId": "p", "sql": "SET TIME ZONE 'UTC'; SELECT *, 'NaN'::float8 AS nan FROM forge_types" }));
    let set = r["resultSets"].as_array().unwrap().last().unwrap().clone();
    let row = &set["rows"][0];
    assert_eq!(row[0], "6f1c9d2e-0000-4000-8000-000000000001");
    assert_eq!(row[1], r#"{"a": [1, 2]}"#);
    assert_eq!(row[2], "2024-02-29");
    assert!(row[3].as_str().unwrap().starts_with("2024-01-01T10:00:00"), "{row}");
    assert_eq!(row[4], "{x,y}");
    assert_eq!(row[5], "123456789012345678901234567890.5");
    assert_eq!(row[6], 1.5);
    assert_eq!(row[7], "1 day");
    assert_eq!(row[8], "NaN");
    assert_eq!(set["columns"][1]["type"], "JSONB");
    let schemas = sc.ok("listSchemas", json!({ "connectionId": "p" }));
    assert!(!schemas.as_array().unwrap().iter().any(|s| s == "pg_catalog" || s == "information_schema"));
    // per-database pools
    let objs = sc.ok("listObjects", json!({ "connectionId": "p", "database": "template1", "schema": "public" }));
    assert!(objs.is_array());
    sc.ok("query", json!({ "connectionId": "p", "sql": "DROP TABLE forge_types" }));
    sc.shutdown();
}

#[test]
fn mysql() {
    let Some(conn) = env_connection("FORGE_SQL_TEST_MYSQL") else {
        eprintln!("FORGE_SQL_TEST_MYSQL not set; skipping");
        return;
    };
    let conn = with_engine(conn, "mysql");
    let db = conn["database"].as_str().expect("FORGE_SQL_TEST_MYSQL needs a database").to_string();
    let e = EngineSql {
        engine: "mysql",
        setup: vec![
            "DROP VIEW IF EXISTS forge_view; DROP TABLE IF EXISTS forge_items",
            "CREATE TABLE forge_items (id int AUTO_INCREMENT PRIMARY KEY, name varchar(50) NOT NULL, price decimal(10,2),
                created datetime, data blob, flag boolean, big bigint)",
            "CREATE VIEW forge_view AS SELECT id, name FROM forge_items",
            "INSERT INTO forge_items (name, price, created, data, flag, big) VALUES
                ('alpha', 1.50, '2024-01-02 03:04:05', X'DEADBEEF', true, 9007199254740993),
                ('beta', NULL, NULL, NULL, false, 1),
                ('gamma', 3.25, '2024-05-06 07:08:09', NULL, NULL, NULL)",
        ],
        price: json!("1.50"),
        created: json!("2024-01-02T03:04:05"),
        slow: "SELECT SLEEP(30)",
        schema: None,
        db_in_list: Box::leak(db.clone().into_boxed_str()),
        reports_rows_affected: true,
    };
    let mut sc = Sidecar::spawn();
    run_scenario(&mut sc, conn.clone(), &e);

    // database parameter selects a per-database pool
    let mut c = conn.clone();
    c["connectionId"] = json!("m");
    sc.ok("connect", c);
    let objs = sc.ok("listObjects", json!({ "connectionId": "m", "database": "information_schema" }));
    assert!(objs.as_array().unwrap().iter().any(|o| o["name"] == "TABLES"), "{objs}");
    let objs = sc.ok("listObjects", json!({ "connectionId": "m", "database": db }));
    assert!(objs.as_array().unwrap().iter().any(|o| o["name"] == "forge_items"));
    let r = sc.ok("query", json!({ "connectionId": "m", "sql": "SELECT CAST(18446744073709551615 AS UNSIGNED), 1.5e0, b'101', JSON_OBJECT('a', 1)" }));
    let row = &r["resultSets"][0]["rows"][0];
    assert_eq!(row[0], "18446744073709551615");
    assert_eq!(row[1], 1.5);
    assert_eq!(row[2], "0x05", "b'..' literals are binary strings");
    assert!(row[3].as_str().unwrap().contains("\"a\""), "{row}");
    let r = sc.ok(
        "query",
        json!({ "connectionId": "m", "sql": "CREATE TEMPORARY TABLE forge_bits (b BIT(3), t TINYINT); INSERT INTO forge_bits VALUES (b'101', 7); SELECT b, t FROM forge_bits" }),
    );
    let sets = r["resultSets"].as_array().unwrap();
    assert_eq!(sets.len(), 3, "{r}");
    assert_eq!(sets[1]["rowsAffected"], 1);
    assert_eq!(sets[2]["rows"], json!([[5, 7]]));
    assert_eq!(sets[2]["columns"][0]["type"], "BIT");
    sc.shutdown();
}

#[test]
fn mssql() {
    let Some(conn) = env_connection("FORGE_SQL_TEST_MSSQL") else {
        eprintln!("FORGE_SQL_TEST_MSSQL not set; skipping");
        return;
    };
    let conn = with_engine(conn, "mssql");
    let e = EngineSql {
        engine: "mssql",
        setup: vec![
            "DROP VIEW IF EXISTS forge_view; DROP TABLE IF EXISTS forge_items",
            "CREATE TABLE forge_items (id int IDENTITY(1,1) PRIMARY KEY, name nvarchar(50) NOT NULL, price decimal(10,2),
                created datetime2, data varbinary(max), flag bit, big bigint)",
            "CREATE VIEW forge_view AS SELECT id, name FROM forge_items",
            "INSERT INTO forge_items (name, price, created, data, flag, big) VALUES
                (N'alpha', 1.50, '2024-01-02 03:04:05', 0xDEADBEEF, 1, 9007199254740993),
                (N'beta', NULL, NULL, NULL, 0, 1),
                (N'gamma', 3.25, '2024-05-06 07:08:09', NULL, NULL, NULL)",
        ],
        price: json!("1.50"),
        created: json!("2024-01-02T03:04:05"),
        slow: "WAITFOR DELAY '00:00:30'",
        schema: Some("dbo"),
        db_in_list: "master",
        reports_rows_affected: false,
    };
    let mut sc = Sidecar::spawn();
    run_scenario(&mut sc, conn.clone(), &e);

    let mut c = conn.clone();
    c["connectionId"] = json!("s");
    sc.ok("connect", c);
    let r = sc.ok(
        "query",
        json!({ "connectionId": "s", "database": "master", "sql": "SELECT DB_NAME() AS db, CAST(1 AS tinyint) AS t, NEWID() AS g,
               CAST('2024-01-01T10:00:00+02:00' AS datetimeoffset) AS o, CAST(12.5 AS money) AS m, CAST(NULL AS bigint) AS n" }),
    );
    let set = &r["resultSets"][0];
    assert_eq!(set["rows"][0][0], "master");
    assert_eq!(set["rows"][0][1], 1);
    assert_eq!(set["columns"][1]["type"], "tinyint");
    assert_eq!(set["rows"][0][2].as_str().unwrap().len(), 36);
    assert_eq!(set["rows"][0][3], "2024-01-01T10:00:00+02:00");
    assert_eq!(set["rows"][0][5], Value::Null);
    let schemas = sc.ok("listSchemas", json!({ "connectionId": "s" }));
    assert!(schemas.as_array().unwrap().iter().any(|s| s == "dbo"));
    assert!(!schemas.as_array().unwrap().iter().any(|s| s == "sys" || s == "guest"));
    sc.shutdown();
}
