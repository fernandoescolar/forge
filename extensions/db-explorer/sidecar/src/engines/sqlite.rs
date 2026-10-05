//! SQLite engine (sqlx, bundled libsqlite3).
//!
//! A single connection is used so that ATTACHed databases and session state persist.
//! Cancellation interrupts the running statement through a progress handler.

use super::sqlx_common::{exec_batch, get_str, query_pool};
use super::{BatchError, ColumnDesc, ConnectParams, DbObject, Engine, ResultSet, ServerInfo, TableDesc};
use crate::sqlbuild::{Dialect, Stmt, quote_ident};
use crate::values;
use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use sqlx::query::Query;
use sqlx::sqlite::{Sqlite, SqliteArguments, SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteQueryResult};
use sqlx::{ConnectOptions, Row};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub struct SqliteEngine {
    pool: SqlitePool,
    interrupt: Arc<AtomicBool>,
}

impl SqliteEngine {
    pub async fn connect(p: &ConnectParams) -> Result<Self> {
        let opts = match (p.url.as_deref().filter(|u| !u.is_empty()), p.file.as_deref().filter(|f| !f.is_empty())) {
            (Some(url), _) => SqliteConnectOptions::from_str(url)?,
            (None, Some(file)) => SqliteConnectOptions::new().filename(file),
            (None, None) => bail!("sqlite connections need a \"file\" path"),
        };
        let opts = opts.create_if_missing(false).busy_timeout(Duration::from_secs(5)).disable_statement_logging();
        let interrupt = Arc::new(AtomicBool::new(false));
        let flag = interrupt.clone();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .acquire_timeout(Duration::from_secs(60))
            .after_connect(move |conn, _| {
                let flag = flag.clone();
                Box::pin(async move {
                    let mut handle = conn.lock_handle().await?;
                    // Returning false interrupts the statement; the flag is consumed by the first check.
                    handle.set_progress_handler(1000, move || !flag.swap(false, Ordering::Relaxed));
                    Ok(())
                })
            })
            .connect_with(opts)
            .await?;
        Ok(Self { pool, interrupt })
    }

    fn schema<'a>(database: Option<&'a str>, schema: Option<&'a str>) -> &'a str {
        schema.or(database).unwrap_or("main")
    }
}

fn bind<'q>(q: Query<'q, Sqlite, SqliteArguments<'q>>, v: &Value) -> Query<'q, Sqlite, SqliteArguments<'q>> {
    match v {
        Value::Null => q.bind(None::<String>),
        Value::Bool(b) => q.bind(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => q.bind(i),
            None if n.is_f64() => q.bind(n.as_f64()),
            None => q.bind(n.to_string()),
        },
        Value::String(s) => q.bind(s.clone()),
        other => q.bind(other.to_string()),
    }
}

fn affected(r: &SqliteQueryResult) -> u64 {
    r.rows_affected()
}

#[async_trait]
impl Engine for SqliteEngine {
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    fn table_schema<'a>(&self, database: Option<&'a str>, schema: Option<&'a str>) -> Option<&'a str> {
        schema.or(database)
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        let (v,): (String,) = sqlx::query_as("SELECT sqlite_version()").fetch_one(&self.pool).await?;
        Ok(ServerInfo { server_version: v, default_database: Some("main".into()), engine: "sqlite".into() })
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT name FROM pragma_database_list ORDER BY seq").fetch_all(&self.pool).await?;
        Ok(rows.iter().filter_map(|r| get_str(r, 0)).collect())
    }

    async fn list_schemas(&self, _database: Option<&str>) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn list_objects(&self, database: Option<&str>, schema: Option<&str>) -> Result<Vec<DbObject>> {
        let s = Self::schema(database, schema);
        let sql = format!(
            r"SELECT name, type FROM {}.sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\_%' ESCAPE '\'",
            quote_ident(Dialect::Sqlite, s)
        );
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        let schema_out = (s != "main").then(|| s.to_string());
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(DbObject {
                    name: get_str(r, 0)?,
                    schema: schema_out.clone(),
                    kind: if get_str(r, 1)? == "view" { "view" } else { "table" },
                })
            })
            .collect())
    }

    async fn describe(&self, database: Option<&str>, schema: Option<&str>, table: &str) -> Result<TableDesc> {
        let rows = sqlx::query(r#"SELECT name, type, "notnull", dflt_value, pk FROM pragma_table_info(?1, ?2) ORDER BY cid"#)
            .bind(table)
            .bind(Self::schema(database, schema))
            .fetch_all(&self.pool)
            .await?;
        if rows.is_empty() {
            bail!("table \"{table}\" not found");
        }
        let mut pk: Vec<(i64, String)> = Vec::new();
        let mut columns = Vec::with_capacity(rows.len());
        for r in &rows {
            let name = get_str(r, 0).unwrap_or_default();
            let pk_pos: i64 = r.try_get_unchecked(4)?;
            if pk_pos > 0 {
                pk.push((pk_pos, name.clone()));
            }
            columns.push(ColumnDesc {
                name,
                type_name: get_str(r, 1).unwrap_or_default(),
                nullable: r.try_get_unchecked::<i64, _>(2)? == 0 && pk_pos == 0,
                default: get_str(r, 3),
                primary_key: pk_pos > 0,
                auto_increment: false,
            });
        }
        pk.sort();
        // A single INTEGER PRIMARY KEY column is an alias for the rowid.
        if let [(_, only)] = pk.as_slice()
            && let Some(c) = columns.iter_mut().find(|c| &c.name == only)
            && c.type_name.eq_ignore_ascii_case("INTEGER")
        {
            c.auto_increment = true;
        }
        Ok(TableDesc { columns, primary_key: pk.into_iter().map(|(_, n)| n).collect() })
    }

    async fn query(&self, _database: Option<&str>, sql: &str, max_rows: usize) -> Result<Vec<ResultSet>> {
        // Only one connection exists, so the flag can only interrupt the statement being cancelled.
        let flag = self.interrupt.clone();
        let on_abort: Box<dyn FnOnce() + Send> = Box::new(move || flag.store(true, Ordering::Relaxed));
        query_pool::<Sqlite>(&self.pool, sql, max_rows, values::sqlite_row, affected, false, Some(on_abort)).await
    }

    async fn execute_batch(&self, _database: Option<&str>, stmts: &[Stmt]) -> Result<u64, BatchError> {
        exec_batch::<Sqlite>(&self.pool, stmts, bind, affected).await
    }

    async fn close(&self) {
        self.pool.close().await;
    }
}
