//! MongoDB: databases, collections and their documents, with the official driver. Unlike
//! the SQL engines there are no tables or rows here; documents travel as text in the
//! format of [`shell`] (JSON plus `ObjectId(…)`, `ISODate(…)`…), so an edit keeps types.

pub mod shell;

use anyhow::{Context, Result, anyhow, bail};
use bson::{Bson, Document, doc};
use futures::TryStreamExt as _;
use mongodb::Client;
use mongodb::options::{ClientOptions, FindOptions};
use serde::{Deserialize, Serialize};
use std::time::Instant;

use crate::engines::{ConnectParams, ServerInfo};

pub struct Mongo {
    client: Client,
    default_database: Option<String>,
}

/// The connection string for `p`: its `url` (`mongodb://…`, `mongodb+srv://…` for Atlas)
/// or one built from host, port, user and password.
pub fn connection_string(p: &ConnectParams) -> String {
    if let Some(url) = p.url.as_deref().filter(|u| !u.trim().is_empty()) {
        return url.trim().to_string();
    }
    let encode = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
    let auth = match (p.user.as_deref().filter(|u| !u.is_empty()), p.password.as_deref()) {
        (Some(user), Some(password)) if !password.is_empty() => format!("{}:{}@", encode(user), encode(password)),
        (Some(user), _) => format!("{}@", encode(user)),
        _ => String::new(),
    };
    let host = p.host.as_deref().filter(|h| !h.is_empty()).unwrap_or("localhost");
    let port = p.port.unwrap_or(27017);
    let database = p.database.as_deref().filter(|d| !d.is_empty()).map(encode).unwrap_or_default();
    let mut options = vec!["serverSelectionTimeoutMS=10000".to_string()];
    match p.ssl.as_deref() {
        Some("require") => options.push("tls=true".into()),
        Some("disable") | None | Some(_) => {}
    }
    if p.trust_server_certificate == Some(true) && p.ssl.as_deref() == Some("require") {
        options.push("tlsAllowInvalidCertificates=true".into());
    }
    // Users are usually defined in admin even when a database is given.
    if !auth.is_empty() && !database.is_empty() {
        options.push("authSource=admin".into());
    }
    format!("mongodb://{auth}{host}:{port}/{database}?{}", options.join("&"))
}

impl Mongo {
    pub async fn connect(p: &ConnectParams) -> Result<(Self, ServerInfo)> {
        let mut options = ClientOptions::parse(connection_string(p)).await.context("invalid connection string")?;
        options.app_name = Some("Forge Database Explorer".into());
        let default_database = options.default_database.clone().or_else(|| p.database.clone().filter(|d| !d.is_empty()));
        let client = Client::with_options(options)?;
        // Connecting is lazy: ask the server something, so a wrong host or password fails here.
        let info = client.database("admin").run_command(doc! { "buildInfo": 1 }).await?;
        let version = info.get_str("version").unwrap_or("unknown").to_string();
        let mongo = Self { client, default_database: default_database.clone() };
        Ok((mongo, ServerInfo { server_version: format!("MongoDB {version}"), default_database, engine: "mongodb".into() }))
    }

    pub async fn close(&self) {
        self.client.clone().shutdown().await;
    }

    pub async fn list_databases(&self) -> Result<Vec<String>> {
        match self.client.list_database_names().await {
            Ok(mut names) => {
                names.sort();
                Ok(names)
            }
            // A user allowed into one database only can't list them: show that one.
            Err(e) if self.default_database.is_some() && is_unauthorized(&e) => Ok(self.default_database.clone().into_iter().collect()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn list_collections(&self, database: &str) -> Result<Vec<CollectionInfo>> {
        let mut specs: Vec<_> = self.client.database(database).list_collections().await?.try_collect().await?;
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(specs
            .into_iter()
            .filter(|s| !s.name.starts_with("system."))
            .map(|s| CollectionInfo { kind: if matches!(s.collection_type, mongodb::results::CollectionType::View) { "view" } else { "collection" }, name: s.name })
            .collect())
    }

    pub async fn list_indexes(&self, database: &str, collection: &str) -> Result<Vec<IndexInfo>> {
        let coll = self.client.database(database).collection::<Document>(collection);
        let indexes: Vec<_> = coll.list_indexes().await?.try_collect().await?;
        Ok(indexes
            .into_iter()
            .map(|i| IndexInfo {
                name: i.options.as_ref().and_then(|o| o.name.clone()).unwrap_or_default(),
                keys: shell::format_compact(&Bson::Document(i.keys)),
                unique: i.options.as_ref().and_then(|o| o.unique).unwrap_or(false),
            })
            .collect())
    }

    pub async fn find(&self, p: &FindParams) -> Result<Documents> {
        let coll = self.client.database(&p.database).collection::<Document>(&p.collection);
        let filter = shell::parse_document(p.filter.as_deref().unwrap_or_default(), "filter")?;
        let sort = shell::parse_document(p.sort.as_deref().unwrap_or_default(), "sort")?;
        let projection = shell::parse_document(p.projection.as_deref().unwrap_or_default(), "projection")?;
        let start = Instant::now();
        let options = FindOptions::builder()
            .sort((!sort.is_empty()).then_some(sort))
            .projection((!projection.is_empty()).then_some(projection))
            .skip(p.skip)
            .limit(p.limit.map(|l| l as i64))
            .build();
        let docs: Vec<Document> = coll.find(filter.clone()).with_options(options).await?.try_collect().await?;
        // Counting everything is cheap from the collection's metadata; with a filter it isn't.
        let total = if filter.is_empty() { coll.estimated_document_count().await.ok() } else { coll.count_documents(filter).await.ok() };
        Ok(Documents::new(docs, false, total, start))
    }

    pub async fn aggregate(&self, p: &AggregateParams) -> Result<Documents> {
        let coll = self.client.database(&p.database).collection::<Document>(&p.collection);
        let pipeline = shell::parse_pipeline(&p.pipeline)?;
        let max = p.max_docs.unwrap_or(1000);
        let start = Instant::now();
        let mut cursor = coll.aggregate(pipeline).await?;
        let mut docs = Vec::new();
        let mut truncated = false;
        while let Some(doc) = cursor.try_next().await? {
            if docs.len() == max {
                truncated = true;
                break;
            }
            docs.push(doc);
        }
        Ok(Documents::new(docs, truncated, None, start))
    }

    pub async fn insert(&self, database: &str, collection: &str, document: &str) -> Result<String> {
        let doc = shell::parse_document(document, "document")?;
        let result = self.client.database(database).collection::<Document>(collection).insert_one(doc).await?;
        Ok(shell::format_compact(&result.inserted_id))
    }

    /// Replaces the document with `_id` = `id` by `document` (which keeps that `_id`).
    pub async fn replace(&self, database: &str, collection: &str, id: &str, document: &str) -> Result<()> {
        let id = shell::parse(id).context("the document's _id")?;
        let mut doc = shell::parse_document(document, "document")?;
        match doc.get("_id") {
            None => {
                // Keep it first, where it was.
                let mut with_id = doc! { "_id": id.clone() };
                with_id.extend(doc);
                doc = with_id;
            }
            Some(new) if *new != id => bail!("the _id can't change (it is {}); insert a new document instead", shell::format_compact(&id)),
            Some(_) => {}
        }
        let result = self.client.database(database).collection::<Document>(collection).replace_one(doc! { "_id": id }, doc).await?;
        if result.matched_count == 0 {
            bail!("the document is gone: someone deleted it, or its _id changed");
        }
        Ok(())
    }

    pub async fn delete(&self, database: &str, collection: &str, id: &str) -> Result<()> {
        let id = shell::parse(id).context("the document's _id")?;
        let result = self.client.database(database).collection::<Document>(collection).delete_one(doc! { "_id": id }).await?;
        if result.deleted_count == 0 {
            bail!("the document is gone: someone deleted it already");
        }
        Ok(())
    }
}

fn is_unauthorized(e: &mongodb::error::Error) -> bool {
    matches!(*e.kind, mongodb::error::ErrorKind::Command(ref c) if c.code == 13)
}

#[derive(Debug, Serialize)]
pub struct CollectionInfo {
    pub name: String,
    pub kind: &'static str,
}

#[derive(Debug, Serialize)]
pub struct IndexInfo {
    pub name: String,
    pub keys: String,
    pub unique: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindParams {
    pub connection_id: String,
    pub database: String,
    pub collection: String,
    pub filter: Option<String>,
    pub sort: Option<String>,
    pub projection: Option<String>,
    pub skip: Option<u64>,
    pub limit: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregateParams {
    pub connection_id: String,
    pub database: String,
    pub collection: String,
    pub pipeline: String,
    pub max_docs: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentParams {
    pub connection_id: String,
    pub database: String,
    pub collection: String,
    pub id: Option<String>,
    pub document: Option<String>,
}

/// Documents found: each as text (to show and edit), its `_id` (to save or delete it) and
/// its top-level fields in short (for the table).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Documents {
    pub documents: Vec<FoundDocument>,
    pub truncated: bool,
    pub total: Option<u64>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct FoundDocument {
    pub id: Option<String>,
    pub text: String,
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl Documents {
    fn new(docs: Vec<Document>, truncated: bool, total: Option<u64>, start: Instant) -> Self {
        let documents = docs
            .into_iter()
            .map(|doc| FoundDocument {
                id: doc.get("_id").map(shell::format_compact),
                fields: doc.iter().map(|(k, v)| (k.clone(), shell::summary(v))).collect(),
                text: shell::format(&Bson::Document(doc)),
            })
            .collect();
        Self { documents, truncated, total, elapsed_ms: start.elapsed().as_millis() as u64 }
    }
}

impl DocumentParams {
    pub fn id(&self) -> Result<&str> {
        self.id.as_deref().ok_or_else(|| anyhow!("missing id"))
    }
    pub fn document(&self) -> Result<&str> {
        self.document.as_deref().ok_or_else(|| anyhow!("missing document"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_connection_string() {
        let p = |f: &dyn Fn(&mut ConnectParams)| {
            let mut p = ConnectParams { connection_id: "c".into(), engine: "mongodb".into(), ..Default::default() };
            f(&mut p);
            connection_string(&p)
        };
        assert_eq!(p(&|_| {}), "mongodb://localhost:27017/?serverSelectionTimeoutMS=10000");
        assert_eq!(
            p(&|p| {
                p.host = Some("db.example.com".into());
                p.port = Some(27018);
                p.user = Some("ada".into());
                p.password = Some("p@ss:w/rd".into());
                p.database = Some("shop".into());
                p.ssl = Some("require".into());
            }),
            "mongodb://ada:p%40ss%3Aw%2Frd@db.example.com:27018/shop?serverSelectionTimeoutMS=10000&tls=true&authSource=admin"
        );
        assert_eq!(p(&|p| p.url = Some(" mongodb+srv://u:p@cluster0.example.net/ ".into())), "mongodb+srv://u:p@cluster0.example.net/", "a connection string wins");
    }
}
