//! MongoDB, gated on FORGE_SQL_TEST_MONGO holding connect JSON, e.g.
//!   docker run -d --rm -p 27099:27017 mongo
//!   FORGE_SQL_TEST_MONGO='{"host":"localhost","port":27099}' cargo test --test mongo

mod common;

use common::{Sidecar, env_connection};
use serde_json::{Value, json};

#[test]
fn mongodb() {
    let Some(mut conn) = env_connection("FORGE_SQL_TEST_MONGO") else {
        eprintln!("FORGE_SQL_TEST_MONGO not set; skipping");
        return;
    };
    conn["engine"] = json!("mongodb");
    conn["connectionId"] = json!("m");
    let mut sc = Sidecar::spawn();
    let info = sc.ok("connect", conn.clone());
    assert!(info["serverVersion"].as_str().unwrap().starts_with("MongoDB "), "{info}");
    assert_eq!(info["engine"], "mongodb");

    let db = json!({ "connectionId": "m", "database": "forge_test", "collection": "people" });
    let with = |extra: Value| {
        let mut v = db.clone();
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    };
    // A clean collection: delete what an earlier run left.
    let old = sc.ok("find", with(json!({ "limit": 1000 })));
    for d in old["documents"].as_array().unwrap() {
        sc.ok("deleteDocument", with(json!({ "id": d["id"] })));
    }

    // Insert, with the shell's helpers.
    for doc in [
        r#"{ "_id": ObjectId("65f1c0ffee0000000000aaaa"), "name": "Ada", "born": ISODate("1815-12-10"), "langs": ["Analytical Engine"], "score": NumberLong("9007199254740993") }"#,
        r#"{ "name": "Grace", "born": ISODate("1906-12-09T00:00:00Z"), "langs": ["COBOL", "FLOW-MATIC"], "score": 5 }"#,
        r#"{ "name": "Linus", "score": 3.0 }"#,
    ] {
        let r = sc.ok("insertDocument", with(json!({ "document": doc })));
        assert!(r["id"].as_str().unwrap().starts_with("ObjectId(\""), "{r}");
    }
    let names = sc.ok("listDatabases", json!({ "connectionId": "m" }));
    assert!(names.as_array().unwrap().iter().any(|n| n == "forge_test"), "{names}");
    let collections = sc.ok("listCollections", json!({ "connectionId": "m", "database": "forge_test" }));
    assert!(collections.as_array().unwrap().iter().any(|c| c["name"] == "people" && c["kind"] == "collection"), "{collections}");

    // Find: filter, sort, projection, paging, total.
    let r = sc.ok("find", with(json!({ "filter": r#"{ "langs.0": { "$exists": true } }"#, "sort": r#"{ "name": 1 }"#, "limit": 10 })));
    let docs = r["documents"].as_array().unwrap();
    assert_eq!(docs.len(), 2);
    assert_eq!(r["total"], 2);
    assert_eq!(docs[0]["fields"]["name"], "Ada");
    assert_eq!(docs[0]["fields"]["langs"], "[ 1 item ]");
    assert_eq!(docs[0]["id"], "ObjectId(\"65f1c0ffee0000000000aaaa\")");
    let ada = docs[0]["text"].as_str().unwrap().to_string();
    assert!(ada.contains("\"born\": ISODate(\"1815-12-10T00:00:00Z\")") && ada.contains("NumberLong(\"9007199254740993\")"), "{ada}");
    let page = sc.ok("find", with(json!({ "sort": r#"{ "name": 1 }"#, "skip": 1, "limit": 1, "projection": r#"{ "name": 1, "_id": 0 }"# })));
    assert_eq!(page["documents"][0]["text"], "{\n  \"name\": \"Grace\"\n}");
    assert_eq!(page["total"], 3);

    // Replace the whole document: types survive, the _id stays.
    let edited = ada.replace("\"Ada\"", "\"Ada Lovelace\"");
    sc.ok("replaceDocument", with(json!({ "id": docs[0]["id"], "document": edited })));
    let again = sc.ok("find", with(json!({ "filter": r#"{ "_id": ObjectId("65f1c0ffee0000000000aaaa") }"# })));
    assert_eq!(again["documents"][0]["text"].as_str().unwrap(), edited, "saved exactly as edited");
    let without_id = "{ \"name\": \"Ada L.\" }";
    sc.ok("replaceDocument", with(json!({ "id": docs[0]["id"], "document": without_id })));
    let again = sc.ok("find", with(json!({ "filter": r#"{ "name": "Ada L." }"# })));
    assert_eq!(again["documents"][0]["id"], "ObjectId(\"65f1c0ffee0000000000aaaa\")", "an edit without _id keeps it");
    let e = sc.err("replaceDocument", with(json!({ "id": docs[0]["id"], "document": "{ \"_id\": 1, \"name\": \"x\" }" })));
    assert!(e["message"].as_str().unwrap().contains("_id can't change"), "{e}");
    let e = sc.err("find", with(json!({ "filter": "{ name: 1 }" })));
    assert!(e["message"].as_str().unwrap().contains("not valid JSON"), "{e}");

    // Aggregate.
    let r = sc.ok("aggregate", with(json!({ "pipeline": r#"[{ "$match": { "score": { "$gt": 2 } } }, { "$group": { "_id": null, "n": { "$sum": 1 } } }]"# })));
    assert_eq!(r["documents"][0]["fields"]["n"], 2, "Grace and Linus; Ada lost her score in the replace");
    let indexes = sc.ok("listIndexes", with(json!({})));
    assert_eq!(indexes[0]["name"], "_id_");
    assert_eq!(indexes[0]["keys"], "{\"_id\": 1}");

    // Delete.
    sc.ok("deleteDocument", with(json!({ "id": "ObjectId(\"65f1c0ffee0000000000aaaa\")" })));
    let e = sc.err("deleteDocument", with(json!({ "id": "ObjectId(\"65f1c0ffee0000000000aaaa\")" })));
    assert!(e["message"].as_str().unwrap().contains("gone"), "{e}");
    sc.ok("disconnect", json!({ "connectionId": "m" }));
    let e = sc.err("find", with(json!({})));
    assert_eq!(e["code"], "not_connected");
    sc.shutdown();
}
