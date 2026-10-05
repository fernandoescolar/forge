//! MySQL / MariaDB engine (sqlx). Databases act as schemas; pools are per database.

use super::sqlx_common::{Pools, exec_batch, get_str, query_pool};
use super::{BatchError, ColumnDesc, ConnectParams, DbObject, Engine, ResultSet, ServerInfo, SslMode, TableDesc};
use crate::sqlbuild::{Dialect, Stmt};
use crate::values;
use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use sqlx::mysql::{MySql, MySqlArguments, MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlQueryResult, MySqlSslMode};
use sqlx::query::Query;
use sqlx::ConnectOptions;
use std::str::FromStr;
use std::time::Duration;

pub struct MySqlEngine {
    pools: Pools<MySql>,
}

fn pool_options() -> MySqlPoolOptions {
    MySqlPoolOptions::new()
        .max_connections(4)
        .min_connections(0)
        .acquire_timeout(Duration::from_secs(20))
        .idle_timeout(Duration::from_secs(300))
}

impl MySqlEngine {
    pub async fn connect(p: &ConnectParams) -> Result<Self> {
        let opts = match p.url.as_deref().filter(|u| !u.is_empty()) {
            Some(url) => MySqlConnectOptions::from_str(url)?,
            None => {
                let mut o = MySqlConnectOptions::new()
                    .host(p.host())
                    .port(p.port.unwrap_or(3306))
                    .ssl_mode(match p.ssl_mode()? {
                        SslMode::Disable => MySqlSslMode::Disabled,
                        SslMode::Prefer => MySqlSslMode::Preferred,
                        SslMode::Require => MySqlSslMode::Required,
                    });
                if let Some(u) = p.user.as_deref().filter(|s| !s.is_empty()) {
                    o = o.username(u);
                }
                if let Some(pw) = p.password.as_deref() {
                    o = o.password(pw);
                }
                if let Some(db) = p.database.as_deref().filter(|s| !s.is_empty()) {
                    o = o.database(db);
                }
                o
            }
        };
        let opts = opts.charset("utf8mb4").disable_statement_logging();
        let default = pool_options().connect_with(opts.clone()).await?;
        let pools = Pools::new(default, move |db| pool_options().connect_lazy_with(opts.clone().database(db)));
        Ok(Self { pools })
    }

    fn pool(&self, db: Option<&str>) -> MySqlPool {
        self.pools.get(db)
    }

    async fn strings(&self, db: Option<&str>, sql: &str) -> Result<Vec<String>> {
        let rows = sqlx::raw_sql(sql).fetch_all(&self.pool(db)).await?;
        Ok(rows.iter().filter_map(|r| get_str(r, 0)).collect())
    }
}

fn bind<'q>(q: Query<'q, MySql, MySqlArguments>, v: &Value) -> Query<'q, MySql, MySqlArguments> {
    match v {
        Value::Null => q.bind(None::<String>),
        Value::Bool(b) => q.bind(*b),
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => q.bind(i),
            (_, Some(u)) => q.bind(u),
            _ => q.bind(n.as_f64()),
        },
        Value::String(s) => q.bind(s.clone()),
        other => q.bind(other.to_string()),
    }
}

fn affected(r: &MySqlQueryResult) -> u64 {
    r.rows_affected()
}

#[async_trait]
impl Engine for MySqlEngine {
    fn dialect(&self) -> Dialect {
        Dialect::MySql
    }

    fn table_schema<'a>(&self, database: Option<&'a str>, schema: Option<&'a str>) -> Option<&'a str> {
        schema.or(database)
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        let row = sqlx::raw_sql("SELECT VERSION(), DATABASE()").fetch_one(&self.pool(None)).await?;
        let version = get_str(&row, 0).unwrap_or_default();
        let engine = if version.to_ascii_lowercase().contains("mariadb") { "mariadb" } else { "mysql" };
        Ok(ServerInfo { server_version: version, default_database: get_str(&row, 1), engine: engine.into() })
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        self.strings(None, "SHOW DATABASES").await
    }

    async fn list_schemas(&self, _database: Option<&str>) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn list_objects(&self, database: Option<&str>, schema: Option<&str>) -> Result<Vec<DbObject>> {
        let rows = sqlx::query(
            "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = COALESCE(?, DATABASE())",
        )
        .bind(schema.or(database))
        .fetch_all(&self.pool(database))
        .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let name = get_str(r, 0)?;
                let ty = get_str(r, 1).unwrap_or_default();
                let kind = if ty.contains("VIEW") {
                    "view"
                } else if ty.contains("SEQUENCE") {
                    return None;
                } else {
                    "table"
                };
                Some(DbObject { name, schema: None, kind })
            })
            .collect())
    }

    async fn describe(&self, database: Option<&str>, schema: Option<&str>, table: &str) -> Result<TableDesc> {
        let pool = self.pool(database);
        let schema = schema.or(database);
        let rows = sqlx::query(
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, COLUMN_KEY, EXTRA
             FROM information_schema.COLUMNS
             WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) AND TABLE_NAME = ?
             ORDER BY ORDINAL_POSITION",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&pool)
        .await?;
        if rows.is_empty() {
            bail!("table `{table}` not found");
        }
        let pk_rows = sqlx::query(
            "SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE
             WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) AND TABLE_NAME = ? AND CONSTRAINT_NAME = 'PRIMARY'
             ORDER BY ORDINAL_POSITION",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&pool)
        .await?;
        let primary_key: Vec<String> = pk_rows.iter().filter_map(|r| get_str(r, 0)).collect();
        let columns = rows
            .iter()
            .map(|r| {
                let name = get_str(r, 0).unwrap_or_default();
                ColumnDesc {
                    primary_key: primary_key.contains(&name),
                    type_name: get_str(r, 1).unwrap_or_default(),
                    nullable: get_str(r, 2).is_some_and(|s| s == "YES"),
                    // MariaDB reports a missing default on nullable columns as the literal NULL.
                    default: get_str(r, 3).filter(|d| d != "NULL"),
                    auto_increment: get_str(r, 5).is_some_and(|s| s.to_ascii_lowercase().contains("auto_increment")),
                    name,
                }
            })
            .collect();
        Ok(TableDesc { columns, primary_key })
    }

    async fn query(&self, database: Option<&str>, sql: &str, max_rows: usize) -> Result<Vec<ResultSet>> {
        query_pool::<MySql>(&self.pool(database), sql, max_rows, values::mysql_row, affected, true, None).await
    }

    async fn execute_batch(&self, database: Option<&str>, stmts: &[Stmt]) -> Result<u64, BatchError> {
        exec_batch::<MySql>(&self.pool(database), stmts, bind, affected).await
    }

    async fn close(&self) {
        self.pools.close().await;
    }
}
