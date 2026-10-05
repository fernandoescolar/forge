//! PostgreSQL engine (sqlx).

use super::sqlx_common::{Pools, exec_batch, json_text, query_pool};
use super::{BatchError, ColumnDesc, ConnectParams, DbObject, Engine, ResultSet, ServerInfo, SslMode, TableDesc};
use crate::sqlbuild::{Dialect, Stmt};
use crate::values;
use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use sqlx::postgres::{PgArguments, PgConnectOptions, PgPool, PgPoolOptions, PgQueryResult, PgSslMode, Postgres};
use sqlx::query::Query;
use sqlx::{ConnectOptions, Row};
use std::str::FromStr;
use std::time::Duration;

pub struct PgEngine {
    pools: Pools<Postgres>,
}

fn pool_options() -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(4)
        .min_connections(0)
        .acquire_timeout(Duration::from_secs(20))
        .idle_timeout(Duration::from_secs(300))
}

impl PgEngine {
    pub async fn connect(p: &ConnectParams) -> Result<Self> {
        let opts = match p.url.as_deref().filter(|u| !u.is_empty()) {
            Some(url) => PgConnectOptions::from_str(url)?,
            None => {
                let mut o = PgConnectOptions::new()
                    .host(p.host())
                    .port(p.port.unwrap_or(5432))
                    .ssl_mode(match p.ssl_mode()? {
                        SslMode::Disable => PgSslMode::Disable,
                        SslMode::Prefer => PgSslMode::Prefer,
                        SslMode::Require => PgSslMode::Require,
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
        let opts = opts.application_name("forge-sql").disable_statement_logging();
        let default = pool_options().connect_with(opts.clone()).await?;
        let pools = Pools::new(default, move |db| pool_options().connect_lazy_with(opts.clone().database(db)));
        Ok(Self { pools })
    }

    fn pool(&self, db: Option<&str>) -> PgPool {
        self.pools.get(db)
    }
}

fn bind<'q>(q: Query<'q, Postgres, PgArguments>, v: &Value) -> Query<'q, Postgres, PgArguments> {
    // Everything is bound as text and CAST to the column type in SQL.
    q.bind(json_text(v))
}

fn affected(r: &PgQueryResult) -> u64 {
    r.rows_affected()
}

#[async_trait]
impl Engine for PgEngine {
    fn dialect(&self) -> Dialect {
        Dialect::Postgres
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        let (version, db): (String, String) =
            sqlx::query_as("SELECT current_setting('server_version'), current_database()::text")
                .fetch_one(&self.pool(None))
                .await?;
        Ok(ServerInfo { server_version: version, default_database: Some(db), engine: "postgres".into() })
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT datname::text FROM pg_database WHERE NOT datistemplate AND datallowconn ORDER BY 1",
        )
        .fetch_all(&self.pool(None))
        .await?)
    }

    async fn list_schemas(&self, database: Option<&str>) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            r"SELECT nspname::text FROM pg_namespace
              WHERE nspname NOT IN ('pg_catalog', 'information_schema')
                AND nspname NOT LIKE 'pg\_toast%' AND nspname NOT LIKE 'pg\_temp%'
              ORDER BY 1",
        )
        .fetch_all(&self.pool(database))
        .await?)
    }

    async fn list_objects(&self, database: Option<&str>, schema: Option<&str>) -> Result<Vec<DbObject>> {
        let rows = sqlx::query(
            r"SELECT n.nspname::text, c.relname::text, c.relkind IN ('v', 'm')
              FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE c.relkind IN ('r', 'p', 'f', 'v', 'm') AND NOT c.relispartition
                AND CASE WHEN $1::text IS NULL
                    THEN n.nspname NOT IN ('pg_catalog', 'information_schema')
                         AND n.nspname NOT LIKE 'pg\_toast%' AND n.nspname NOT LIKE 'pg\_temp%'
                    ELSE n.nspname = $1 END",
        )
        .bind(schema)
        .fetch_all(&self.pool(database))
        .await?;
        rows.iter()
            .map(|r| {
                Ok(DbObject {
                    schema: Some(r.try_get(0)?),
                    name: r.try_get(1)?,
                    kind: if r.try_get::<bool, _>(2)? { "view" } else { "table" },
                })
            })
            .collect()
    }

    async fn describe(&self, database: Option<&str>, schema: Option<&str>, table: &str) -> Result<TableDesc> {
        let rows = sqlx::query(
            r"SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull,
                     pg_get_expr(d.adbin, d.adrelid),
                     COALESCE(a.attnum = ANY(i.indkey::int2[]), false),
                     a.attidentity <> '' OR COALESCE(pg_get_expr(d.adbin, d.adrelid) LIKE 'nextval(%', false),
                     array_position(i.indkey::int2[], a.attnum)::int4
              FROM pg_attribute a
              JOIN pg_class c ON c.oid = a.attrelid
              JOIN pg_namespace n ON n.oid = c.relnamespace
              LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
              LEFT JOIN pg_index i ON i.indrelid = c.oid AND i.indisprimary
              WHERE c.relname = $2 AND n.nspname = COALESCE($1, current_schema())
                AND a.attnum > 0 AND NOT a.attisdropped
              ORDER BY a.attnum",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool(database))
        .await?;
        if rows.is_empty() {
            bail!("table \"{table}\" not found");
        }
        let mut pk: Vec<(i32, String)> = Vec::new();
        let mut columns = Vec::with_capacity(rows.len());
        for r in &rows {
            let name: String = r.try_get(0)?;
            let is_pk: bool = r.try_get(4)?;
            if is_pk {
                pk.push((r.try_get::<Option<i32>, _>(6)?.unwrap_or(0), name.clone()));
            }
            columns.push(ColumnDesc {
                name,
                type_name: r.try_get(1)?,
                nullable: r.try_get(2)?,
                default: r.try_get(3)?,
                primary_key: is_pk,
                auto_increment: r.try_get(5)?,
            });
        }
        pk.sort();
        Ok(TableDesc { columns, primary_key: pk.into_iter().map(|(_, n)| n).collect() })
    }

    async fn query(&self, database: Option<&str>, sql: &str, max_rows: usize) -> Result<Vec<ResultSet>> {
        query_pool::<Postgres>(&self.pool(database), sql, max_rows, values::pg_row, affected, true, None).await
    }

    async fn execute_batch(&self, database: Option<&str>, stmts: &[Stmt]) -> Result<u64, BatchError> {
        exec_batch::<Postgres>(&self.pool(database), stmts, bind, affected).await
    }

    async fn close(&self) {
        self.pools.close().await;
    }
}
