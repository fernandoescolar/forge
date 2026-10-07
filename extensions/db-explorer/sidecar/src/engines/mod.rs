//! Engine abstraction plus the engine-independent parts of fetchTable / applyChanges.

pub mod mssql;
pub mod mysql;
pub mod pg;
pub mod sqlite;
mod sqlx_common;

use crate::sqlbuild::{self, Change, Dialect, OrderBy, Stmt};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultSet {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<Value>>,
    pub truncated: bool,
    pub rows_affected: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DbObject {
    pub name: String,
    pub schema: Option<String>,
    pub kind: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnDesc {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub primary_key: bool,
    pub auto_increment: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableDesc {
    pub columns: Vec<ColumnDesc>,
    pub primary_key: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub server_version: String,
    pub default_database: Option<String>,
    pub engine: String,
}

/// Failure while applying a batch of changes; `index` is the failing statement, if any.
#[derive(Debug)]
pub struct BatchError {
    pub index: Option<usize>,
    pub error: anyhow::Error,
}

impl BatchError {
    pub fn general(e: impl Into<anyhow::Error>) -> Self {
        Self { index: None, error: e.into() }
    }
    pub fn at(index: usize, e: impl Into<anyhow::Error>) -> Self {
        Self { index: Some(index), error: e.into() }
    }
}

pub fn check_affected(stmt: &Stmt, n: u64) -> Result<()> {
    if stmt.expect_one && n != 1 {
        bail!(
            "{n} rows affected instead of 1: the row was changed or deleted by someone else, or the key is not unique"
        );
    }
    Ok(())
}

#[async_trait]
pub trait Engine: Send + Sync {
    fn dialect(&self) -> Dialect;
    /// Schema used to qualify table names (MySQL/SQLite fall back to the database name).
    fn table_schema<'a>(&self, _database: Option<&'a str>, schema: Option<&'a str>) -> Option<&'a str> {
        schema
    }
    async fn server_info(&self) -> Result<ServerInfo>;
    async fn list_databases(&self) -> Result<Vec<String>>;
    async fn list_schemas(&self, database: Option<&str>) -> Result<Vec<String>>;
    async fn list_objects(&self, database: Option<&str>, schema: Option<&str>) -> Result<Vec<DbObject>>;
    async fn describe(&self, database: Option<&str>, schema: Option<&str>, table: &str) -> Result<TableDesc>;
    async fn query(&self, database: Option<&str>, sql: &str, max_rows: usize) -> Result<Vec<ResultSet>>;
    async fn execute_batch(&self, database: Option<&str>, stmts: &[Stmt]) -> Result<u64, BatchError>;
    async fn close(&self);
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConnectParams {
    pub connection_id: String,
    pub engine: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    pub file: Option<String>,
    pub ssl: Option<String>,
    pub trust_server_certificate: Option<bool>,
    pub url: Option<String>,
    /// Redis Sentinel: the sentinels (`host:port`, comma-separated), the name of the master
    /// they watch, and the sentinels' own password if they have one.
    pub sentinels: Option<String>,
    pub master_name: Option<String>,
    pub sentinel_password: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SslMode {
    Disable,
    Prefer,
    Require,
}

impl ConnectParams {
    pub fn ssl_mode(&self) -> Result<SslMode> {
        Ok(match self.ssl.as_deref().unwrap_or("prefer") {
            "disable" => SslMode::Disable,
            "prefer" => SslMode::Prefer,
            "require" => SslMode::Require,
            other => bail!("invalid ssl mode \"{other}\" (expected disable, prefer or require)"),
        })
    }
    pub fn host(&self) -> &str {
        self.host.as_deref().filter(|h| !h.is_empty()).unwrap_or("localhost")
    }
}

/// Connect (validating the connection) and return the engine plus server info.
pub async fn connect(p: &ConnectParams) -> Result<(Arc<dyn Engine>, ServerInfo)> {
    let engine: Arc<dyn Engine> = match p.engine.as_str() {
        "postgres" | "postgresql" | "pg" => Arc::new(pg::PgEngine::connect(p).await?),
        "mysql" | "mariadb" => Arc::new(mysql::MySqlEngine::connect(p).await?),
        "sqlite" => Arc::new(sqlite::SqliteEngine::connect(p).await?),
        "mssql" | "sqlserver" => Arc::new(mssql::MsSqlEngine::connect(p).await?),
        other => bail!("unsupported engine \"{other}\""),
    };
    let info = engine.server_info().await?;
    Ok((engine, info))
}

pub async fn list_objects(e: &dyn Engine, database: Option<&str>, schema: Option<&str>) -> Result<Vec<DbObject>> {
    let mut objs = e.list_objects(database, schema).await?;
    objs.sort_by(|a, b| (a.kind, &a.name, &a.schema).cmp(&(b.kind, &b.name, &b.schema)));
    Ok(objs)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchTableParams {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub table: String,
    #[serde(default)]
    pub offset: u64,
    #[serde(default = "default_limit")]
    pub limit: u64,
    #[serde(default)]
    pub order_by: Vec<OrderBy>,
    #[serde(rename = "where")]
    pub where_: Option<String>,
}

fn default_limit() -> u64 {
    100
}

#[derive(Debug, Serialize)]
pub struct TablePage {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<Value>>,
    pub total: Option<u64>,
}

pub async fn fetch_table(e: &dyn Engine, p: &FetchTableParams) -> Result<TablePage> {
    let d = e.dialect();
    let db = p.database.as_deref();
    let schema = e.table_schema(db, p.schema.as_deref());
    let limit = p.limit.max(1);
    let select = sqlbuild::select_page(d, schema, &p.table, p.where_.as_deref(), &p.order_by, p.offset, limit);
    let count = sqlbuild::count(d, schema, &p.table, p.where_.as_deref());
    let (rows, total) = tokio::try_join!(e.query(db, &select, limit as usize), e.query(db, &count, 1))?;
    let page = rows.into_iter().next().ok_or_else(|| anyhow!("the query returned no result set"))?;
    let total = total
        .first()
        .and_then(|rs| rs.rows.first())
        .and_then(|r| r.first())
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
    Ok(TablePage { columns: page.columns, rows: page.rows, total })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyChangesParams {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub table: String,
    pub changes: Vec<Change>,
}

pub async fn apply_changes(e: &dyn Engine, p: &ApplyChangesParams) -> Result<u64, BatchError> {
    let db = p.database.as_deref();
    let schema = e.table_schema(db, p.schema.as_deref());
    let types: HashMap<String, String> = if e.dialect() == Dialect::Postgres {
        let desc = e
            .describe(db, p.schema.as_deref(), &p.table)
            .await
            .context("cannot describe the table")
            .map_err(BatchError::general)?;
        desc.columns.into_iter().map(|c| (c.name, c.type_name)).collect()
    } else {
        HashMap::new()
    };
    let stmts = p
        .changes
        .iter()
        .enumerate()
        .map(|(i, c)| sqlbuild::build_change(e.dialect(), schema, &p.table, c, &types).map_err(|err| BatchError::at(i, err)))
        .collect::<Result<Vec<_>, _>>()?;
    if stmts.is_empty() {
        return Ok(0);
    }
    e.execute_batch(db, &stmts).await?;
    Ok(stmts.len() as u64)
}

/// Treat empty strings as absent (clients often send "" for "no database").
pub fn nonempty(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.is_empty())
}
