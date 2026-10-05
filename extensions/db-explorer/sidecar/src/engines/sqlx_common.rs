//! Shared helpers for the sqlx-backed engines (PostgreSQL, MySQL/MariaDB, SQLite).

use super::{BatchError, ColumnInfo, ResultSet, check_affected};
use crate::sqlbuild::Stmt;
use anyhow::Result;
use futures::TryStreamExt;
use serde_json::Value;
use sqlx::Either;
use sqlx::pool::PoolConnection;
use sqlx::query::Query;
use sqlx::{Column, Database, Executor, IntoArguments, Pool, Row, TypeInfo};
use std::collections::HashMap;
use std::sync::Mutex;

pub type Decoder<DB> = fn(&<DB as Database>::Row) -> Vec<Value>;
pub type Affected<DB> = fn(&<DB as Database>::QueryResult) -> u64;
type PoolFactory<DB> = Box<dyn Fn(&str) -> Pool<DB> + Send + Sync>;
pub type Binder<DB> = for<'q> fn(
    Query<'q, DB, <DB as Database>::Arguments<'q>>,
    &Value,
) -> Query<'q, DB, <DB as Database>::Arguments<'q>>;

/// Lazily created pools keyed by database name (None = the connection's default database).
pub struct Pools<DB: Database> {
    default: Pool<DB>,
    by_db: Mutex<HashMap<String, Pool<DB>>>,
    make: PoolFactory<DB>,
}

impl<DB: Database> Pools<DB> {
    pub fn new(default: Pool<DB>, make: impl Fn(&str) -> Pool<DB> + Send + Sync + 'static) -> Self {
        Self { default, by_db: Mutex::new(HashMap::new()), make: Box::new(make) }
    }

    pub fn get(&self, db: Option<&str>) -> Pool<DB> {
        match db {
            None => self.default.clone(),
            Some(db) => {
                let mut map = self.by_db.lock().unwrap_or_else(|e| e.into_inner());
                map.entry(db.to_string()).or_insert_with(|| (self.make)(db)).clone()
            }
        }
    }

    pub async fn close(&self) {
        let pools: Vec<Pool<DB>> = {
            let mut map = self.by_db.lock().unwrap_or_else(|e| e.into_inner());
            map.drain().map(|(_, p)| p).collect()
        };
        for p in pools {
            p.close().await;
        }
        self.default.close().await;
    }
}

/// Owns a pooled connection while a query runs. If the future is dropped (cancelled) or the
/// result stream was abandoned, the connection is closed instead of returned to the pool.
pub struct ConnGuard<DB: Database> {
    conn: Option<PoolConnection<DB>>,
    keep: bool,
    close_on_abort: bool,
    on_abort: Option<Box<dyn FnOnce() + Send>>,
}

impl<DB: Database> ConnGuard<DB> {
    pub async fn acquire(pool: &Pool<DB>, close_on_abort: bool) -> Result<Self> {
        Ok(Self { conn: Some(pool.acquire().await?), keep: false, close_on_abort, on_abort: None })
    }
    pub fn on_abort(mut self, f: impl FnOnce() + Send + 'static) -> Self {
        self.on_abort = Some(Box::new(f));
        self
    }
    pub fn conn(&mut self) -> &mut DB::Connection {
        self.conn.as_mut().expect("connection present")
    }
    pub fn finish(mut self, reusable: bool) {
        self.keep = true;
        if !reusable && let Some(c) = self.conn.as_mut() {
            c.close_on_drop();
        }
    }
}

impl<DB: Database> Drop for ConnGuard<DB> {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        if let Some(f) = self.on_abort.take() {
            f();
        }
        if self.close_on_abort && let Some(c) = self.conn.as_mut() {
            c.close_on_drop();
        }
    }
}

/// Run raw (possibly multi-statement) SQL and collect result sets.
/// Returns the result sets and whether the stream was abandoned early (truncation).
pub async fn run_script<DB>(
    conn: &mut DB::Connection,
    sql: &str,
    max_rows: usize,
    decode: Decoder<DB>,
    affected: Affected<DB>,
) -> Result<(Vec<ResultSet>, bool)>
where
    DB: Database,
    for<'c> &'c mut DB::Connection: Executor<'c, Database = DB>,
{
    let mut sets: Vec<ResultSet> = Vec::new();
    let mut current: Option<ResultSet> = None;
    let mut abandoned = false;
    {
        let mut stream = sqlx::raw_sql(sql).fetch_many(&mut *conn);
        while let Some(item) = stream.try_next().await? {
            match item {
                Either::Right(row) => {
                    let set = current.get_or_insert_with(|| ResultSet {
                        columns: columns_of::<DB>(row.columns()),
                        rows: Vec::new(),
                        truncated: false,
                        rows_affected: None,
                    });
                    if set.rows.len() >= max_rows {
                        set.truncated = true;
                        abandoned = true;
                        break;
                    }
                    set.rows.push(decode(&row));
                }
                Either::Left(done) => sets.push(current.take().unwrap_or_else(|| ResultSet {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    truncated: false,
                    rows_affected: Some(affected(&done)),
                })),
            }
        }
    }
    sets.extend(current.take());

    // A row-returning statement that produced no rows is indistinguishable from a command in the
    // raw protocol (and SQLite reports a stale change count for it); for single-statement
    // scripts, ask the server for the column metadata.
    if let [only] = sets.as_mut_slice()
        && only.columns.is_empty()
        && let Ok(desc) = (&mut *conn).describe(sql).await
        && !desc.columns().is_empty()
    {
        only.columns = columns_of::<DB>(desc.columns());
        only.rows_affected = None;
    }
    if sets.is_empty() {
        sets.push(ResultSet { columns: Vec::new(), rows: Vec::new(), truncated: false, rows_affected: None });
    }
    Ok((sets, abandoned))
}

fn columns_of<DB: Database>(cols: &[DB::Column]) -> Vec<ColumnInfo> {
    cols.iter()
        .map(|c| ColumnInfo { name: c.name().to_string(), type_name: c.type_info().name().to_string() })
        .collect()
}

/// Run a query on a pooled connection with cancellation safety.
pub async fn query_pool<DB>(
    pool: &Pool<DB>,
    sql: &str,
    max_rows: usize,
    decode: Decoder<DB>,
    affected: Affected<DB>,
    close_on_abort: bool,
    on_abort: Option<Box<dyn FnOnce() + Send>>,
) -> Result<Vec<ResultSet>>
where
    DB: Database,
    for<'c> &'c mut DB::Connection: Executor<'c, Database = DB>,
{
    let mut guard = ConnGuard::acquire(pool, close_on_abort).await?;
    if let Some(f) = on_abort {
        guard = guard.on_abort(f);
    }
    let res = run_script::<DB>(guard.conn(), sql, max_rows, decode, affected).await;
    match res {
        Ok((sets, abandoned)) => {
            guard.finish(!(abandoned && close_on_abort));
            Ok(sets)
        }
        Err(e) => {
            guard.finish(true);
            Err(e)
        }
    }
}

/// Execute parameterized statements in one transaction; any failure rolls everything back.
pub async fn exec_batch<DB>(
    pool: &Pool<DB>,
    stmts: &[Stmt],
    bind: Binder<DB>,
    affected: Affected<DB>,
) -> Result<u64, BatchError>
where
    DB: Database,
    for<'c> &'c mut DB::Connection: Executor<'c, Database = DB>,
    for<'q> <DB as Database>::Arguments<'q>: IntoArguments<'q, DB>,
{
    let mut tx = pool.begin().await.map_err(BatchError::general)?;
    let mut total = 0;
    for (i, s) in stmts.iter().enumerate() {
        let mut q = sqlx::query::<DB>(&s.sql);
        for p in &s.params {
            q = bind(q, p);
        }
        let r = q.execute(&mut *tx).await.map_err(|e| BatchError::at(i, e))?;
        let n = affected(&r);
        check_affected(s, n).map_err(|e| BatchError::at(i, e))?;
        total += n;
    }
    tx.commit().await.map_err(BatchError::general)?;
    Ok(total)
}

/// Read a string column without sqlx's strict type check (metadata columns differ by server).
pub fn get_str<R: Row>(row: &R, idx: usize) -> Option<String>
where
    usize: sqlx::ColumnIndex<R>,
    for<'r> Option<String>: sqlx::Decode<'r, R::Database>,
    for<'r> Option<Vec<u8>>: sqlx::Decode<'r, R::Database>,
{
    match row.try_get_unchecked::<Option<String>, _>(idx) {
        Ok(v) => v,
        Err(_) => row
            .try_get_unchecked::<Option<Vec<u8>>, _>(idx)
            .ok()
            .flatten()
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
    }
}

/// Text form of a JSON parameter (used for PostgreSQL text binds and string fallbacks).
pub fn json_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    }
}
