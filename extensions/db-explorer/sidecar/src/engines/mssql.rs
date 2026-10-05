//! SQL Server engine (tiberius) with a small per-database connection pool.

use super::{BatchError, ColumnDesc, ColumnInfo, ConnectParams, DbObject, Engine, ResultSet, ServerInfo, SslMode, TableDesc, check_affected};
use crate::sqlbuild::{Dialect, Stmt};
use crate::values;
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures::TryStreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiberius::{AuthMethod, Client, ColumnData, ColumnType, Config, EncryptionLevel, Query, QueryItem, Row, SqlBrowser};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

type MsClient = Client<Compat<TcpStream>>;

const MAX_IDLE: usize = 4;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

struct MsPool {
    config: Config,
    named_instance: bool,
    idle: Mutex<Vec<MsClient>>,
}

impl MsPool {
    async fn checkout(self: &Arc<Self>) -> Result<MsConn> {
        let idle = self.idle.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let client = match idle {
            Some(c) => c,
            None => tokio::time::timeout(CONNECT_TIMEOUT, connect_client(self.config.clone(), self.named_instance))
                .await
                .map_err(|_| anyhow!("timed out connecting to SQL Server"))??,
        };
        Ok(MsConn { client: Some(client), pool: self.clone(), reusable: false })
    }
}

/// A checked-out client. It goes back to the pool only when marked reusable; a dropped
/// (cancelled) or failed operation discards it, since the TDS stream may be mid-response.
struct MsConn {
    client: Option<MsClient>,
    pool: Arc<MsPool>,
    reusable: bool,
}

impl MsConn {
    fn client(&mut self) -> &mut MsClient {
        self.client.as_mut().expect("client present")
    }
    fn release(mut self) {
        self.reusable = true;
    }
}

impl Drop for MsConn {
    fn drop(&mut self) {
        if self.reusable && let Some(c) = self.client.take() {
            let mut idle = self.pool.idle.lock().unwrap_or_else(|e| e.into_inner());
            if idle.len() < MAX_IDLE {
                idle.push(c);
            }
        }
    }
}

async fn connect_client(mut config: Config, named_instance: bool) -> Result<MsClient> {
    for _ in 0..2 {
        let tcp = if named_instance {
            TcpStream::connect_named(&config).await?
        } else {
            TcpStream::connect(config.get_addr()).await?
        };
        tcp.set_nodelay(true)?;
        match Client::connect(config.clone(), tcp.compat_write()).await {
            Ok(c) => return Ok(c),
            // Azure SQL may redirect to another gateway.
            Err(tiberius::error::Error::Routing { host, port }) => {
                config.host(&host);
                config.port(port);
            }
            Err(e) => return Err(e.into()),
        }
    }
    bail!("too many redirects while connecting to SQL Server")
}

pub struct MsSqlEngine {
    base: Config,
    named_instance: bool,
    pools: Mutex<HashMap<Option<String>, Arc<MsPool>>>,
}

impl MsSqlEngine {
    pub async fn connect(p: &ConnectParams) -> Result<Self> {
        let mut config = Config::new();
        let host = p.host();
        let named_instance = match host.split_once('\\') {
            Some((h, instance)) => {
                config.host(h);
                config.instance_name(instance);
                if let Some(port) = p.port {
                    config.port(port);
                }
                true
            }
            None => {
                config.host(host);
                config.port(p.port.unwrap_or(1433));
                false
            }
        };
        config.authentication(AuthMethod::sql_server(
            p.user.as_deref().unwrap_or("sa"),
            p.password.as_deref().unwrap_or(""),
        ));
        if let Some(db) = p.database.as_deref().filter(|s| !s.is_empty()) {
            config.database(db);
        }
        config.application_name("forge-sql");
        config.encryption(match p.ssl_mode()? {
            SslMode::Disable => EncryptionLevel::Off,
            SslMode::Prefer => EncryptionLevel::On,
            SslMode::Require => EncryptionLevel::Required,
        });
        if p.trust_server_certificate.unwrap_or(false) {
            config.trust_cert();
        }
        let engine = Self { base: config, named_instance, pools: Mutex::new(HashMap::new()) };
        // Validate eagerly and keep the connection.
        engine.conn(None).await?.release();
        Ok(engine)
    }

    fn pool(&self, db: Option<&str>) -> Arc<MsPool> {
        let mut pools = self.pools.lock().unwrap_or_else(|e| e.into_inner());
        pools
            .entry(db.map(str::to_string))
            .or_insert_with(|| {
                let mut config = self.base.clone();
                if let Some(db) = db {
                    config.database(db);
                }
                Arc::new(MsPool { config, named_instance: self.named_instance, idle: Mutex::new(Vec::new()) })
            })
            .clone()
    }

    async fn conn(&self, db: Option<&str>) -> Result<MsConn> {
        self.pool(db).checkout().await
    }

    /// Run a parameterized metadata query and return all rows of the first result set.
    async fn rows(&self, db: Option<&str>, sql: &str, params: &[Option<&str>]) -> Result<Vec<Row>> {
        let mut conn = self.conn(db).await?;
        let mut q = Query::new(sql);
        for p in params {
            q.bind(*p);
        }
        let rows = q.query(conn.client()).await?.into_first_result().await?;
        conn.release();
        Ok(rows)
    }
}

fn get_str(row: &Row, idx: usize) -> Option<String> {
    row.try_get::<&str, _>(idx).ok().flatten().map(str::to_string)
}

fn get_i32(row: &Row, idx: usize) -> Option<i32> {
    row.try_get::<i32, _>(idx).ok().flatten()
}

fn get_bool(row: &Row, idx: usize) -> bool {
    row.try_get::<bool, _>(idx).ok().flatten().unwrap_or(false)
}

fn type_name(t: ColumnType) -> &'static str {
    use ColumnType::*;
    match t {
        Null => "null",
        Bit | Bitn => "bit",
        Int1 => "tinyint",
        Int2 => "smallint",
        Int4 | Intn => "int",
        Int8 => "bigint",
        Datetime4 => "smalldatetime",
        Float4 => "real",
        Float8 | Floatn => "float",
        Money => "money",
        Money4 => "smallmoney",
        Datetime | Datetimen => "datetime",
        Guid => "uniqueidentifier",
        Decimaln => "decimal",
        Numericn => "numeric",
        Daten => "date",
        Timen => "time",
        Datetime2 => "datetime2",
        DatetimeOffsetn => "datetimeoffset",
        BigVarBin => "varbinary",
        BigVarChar => "varchar",
        BigBinary => "binary",
        BigChar => "char",
        NVarchar => "nvarchar",
        NChar => "nchar",
        Xml => "xml",
        Udt => "udt",
        Text => "text",
        Image => "image",
        NText => "ntext",
        SSVariant => "sql_variant",
    }
}

/// Variable-width column types ("intn", "floatn", ...) only reveal their size in the data.
fn refine_type(t: ColumnType, data: &ColumnData<'_>) -> Option<&'static str> {
    match (t, data) {
        (ColumnType::Intn, ColumnData::U8(_)) => Some("tinyint"),
        (ColumnType::Intn, ColumnData::I16(_)) => Some("smallint"),
        (ColumnType::Intn, ColumnData::I64(_)) => Some("bigint"),
        (ColumnType::Floatn, ColumnData::F32(_)) => Some("real"),
        (ColumnType::Datetimen, ColumnData::SmallDateTime(_)) => Some("smalldatetime"),
        _ => None,
    }
}

/// Full declared type from sys.columns metadata, e.g. nvarchar(50), decimal(10,2).
fn declared_type(name: &str, max_length: i32, precision: i32, scale: i32) -> String {
    match name {
        "varchar" | "char" | "varbinary" | "binary" if max_length == -1 => format!("{name}(max)"),
        "varchar" | "char" | "varbinary" | "binary" => format!("{name}({max_length})"),
        "nvarchar" | "nchar" if max_length == -1 => format!("{name}(max)"),
        "nvarchar" | "nchar" => format!("{name}({})", max_length / 2),
        "decimal" | "numeric" => format!("{name}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" => format!("{name}({scale})"),
        _ => name.to_string(),
    }
}

fn bind_value<'a>(q: &mut Query<'a>, v: &Value) {
    match v {
        Value::Null => q.bind(None::<&str>),
        Value::Bool(b) => q.bind(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => q.bind(i),
            None if n.is_f64() => q.bind(n.as_f64().unwrap_or_default()),
            None => q.bind(n.to_string()),
        },
        Value::String(s) => q.bind(s.clone()),
        other => q.bind(other.to_string()),
    }
}

#[async_trait]
impl Engine for MsSqlEngine {
    fn dialect(&self) -> Dialect {
        Dialect::MsSql
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        let rows = self.rows(None, "SELECT CAST(@@VERSION AS nvarchar(4000)), DB_NAME()", &[]).await?;
        let row = rows.first().context("no version row")?;
        let version = get_str(row, 0).unwrap_or_default();
        let version = version.lines().next().unwrap_or_default().trim().to_string();
        Ok(ServerInfo { server_version: version, default_database: get_str(row, 1), engine: "mssql".into() })
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let rows = self.rows(None, "SELECT name FROM sys.databases WHERE HAS_DBACCESS(name) = 1 ORDER BY name", &[]).await?;
        Ok(rows.iter().filter_map(|r| get_str(r, 0)).collect())
    }

    async fn list_schemas(&self, database: Option<&str>) -> Result<Vec<String>> {
        let rows = self
            .rows(
                database,
                "SELECT s.name FROM sys.schemas s
                 WHERE (s.name = 'dbo' OR EXISTS (SELECT 1 FROM sys.objects o WHERE o.schema_id = s.schema_id))
                   AND s.name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest') AND s.name NOT LIKE 'db[_]%'
                 ORDER BY s.name",
                &[],
            )
            .await?;
        Ok(rows.iter().filter_map(|r| get_str(r, 0)).collect())
    }

    async fn list_objects(&self, database: Option<&str>, schema: Option<&str>) -> Result<Vec<DbObject>> {
        let rows = self
            .rows(
                database,
                "SELECT s.name, o.name, CASE WHEN o.type = 'V' THEN 'view' ELSE 'table' END
                 FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id
                 WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 AND (@P1 IS NULL OR s.name = @P1)",
                &[schema],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(DbObject {
                    schema: get_str(r, 0),
                    name: get_str(r, 1)?,
                    kind: if get_str(r, 2)? == "view" { "view" } else { "table" },
                })
            })
            .collect())
    }

    async fn describe(&self, database: Option<&str>, schema: Option<&str>, table: &str) -> Result<TableDesc> {
        let rows = self
            .rows(
                database,
                "SELECT c.name, TYPE_NAME(c.user_type_id), CAST(c.max_length AS int), CAST(c.precision AS int),
                        CAST(c.scale AS int), c.is_nullable, OBJECT_DEFINITION(c.default_object_id), c.is_identity,
                        CAST(ic.key_ordinal AS int)
                 FROM sys.columns c
                 LEFT JOIN sys.indexes i ON i.object_id = c.object_id AND i.is_primary_key = 1
                 LEFT JOIN sys.index_columns ic
                        ON ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.column_id = c.column_id
                 WHERE c.object_id = OBJECT_ID(QUOTENAME(COALESCE(@P1, SCHEMA_NAME())) + '.' + QUOTENAME(@P2))
                 ORDER BY c.column_id",
                &[schema, Some(table)],
            )
            .await?;
        if rows.is_empty() {
            bail!("table [{table}] not found");
        }
        let mut pk: Vec<(i32, String)> = Vec::new();
        let columns = rows
            .iter()
            .map(|r| {
                let name = get_str(r, 0).unwrap_or_default();
                let key_ordinal = get_i32(r, 8);
                if let Some(k) = key_ordinal {
                    pk.push((k, name.clone()));
                }
                ColumnDesc {
                    type_name: declared_type(
                        &get_str(r, 1).unwrap_or_default(),
                        get_i32(r, 2).unwrap_or(0),
                        get_i32(r, 3).unwrap_or(0),
                        get_i32(r, 4).unwrap_or(0),
                    ),
                    nullable: get_bool(r, 5),
                    default: get_str(r, 6),
                    primary_key: key_ordinal.is_some(),
                    auto_increment: get_bool(r, 7),
                    name,
                }
            })
            .collect();
        pk.sort();
        Ok(TableDesc { columns, primary_key: pk.into_iter().map(|(_, n)| n).collect() })
    }

    async fn query(&self, database: Option<&str>, sql: &str, max_rows: usize) -> Result<Vec<ResultSet>> {
        let mut conn = self.conn(database).await?;
        let mut sets: Vec<ResultSet> = Vec::new();
        let mut col_types: Vec<ColumnType> = Vec::new();
        let mut abandoned = false;
        {
            let mut stream = conn.client().simple_query(sql).await?;
            while let Some(item) = stream.try_next().await? {
                match item {
                    QueryItem::Metadata(meta) => {
                        col_types = meta.columns().iter().map(|c| c.column_type()).collect();
                        sets.push(ResultSet {
                            columns: meta
                                .columns()
                                .iter()
                                .map(|c| ColumnInfo { name: c.name().to_string(), type_name: type_name(c.column_type()).into() })
                                .collect(),
                            rows: Vec::new(),
                            truncated: false,
                            rows_affected: None,
                        });
                    }
                    QueryItem::Row(row) => {
                        let Some(set) = sets.last_mut() else { continue };
                        if set.rows.len() >= max_rows {
                            set.truncated = true;
                            abandoned = true;
                            break;
                        }
                        let first = set.rows.is_empty();
                        let cells: Vec<Value> = row
                            .cells()
                            .enumerate()
                            .map(|(i, (_, data))| {
                                if first
                                    && let (Some(t), Some(col)) = (col_types.get(i), set.columns.get_mut(i))
                                    && let Some(refined) = refine_type(*t, data)
                                {
                                    col.type_name = refined.into();
                                }
                                values::mssql_value(data)
                            })
                            .collect();
                        set.rows.push(cells);
                    }
                }
            }
        }
        if sets.is_empty() {
            // tiberius does not expose DONE row counts for batches without result sets.
            sets.push(ResultSet { columns: Vec::new(), rows: Vec::new(), truncated: false, rows_affected: None });
        }
        if !abandoned {
            conn.release();
        }
        Ok(sets)
    }

    async fn execute_batch(&self, database: Option<&str>, stmts: &[Stmt]) -> Result<u64, BatchError> {
        let mut conn = self.conn(database).await.map_err(BatchError::general)?;
        let client = conn.client();
        async fn batch(c: &mut MsClient, sql: &str) -> Result<()> {
            c.simple_query(sql).await?.into_results().await?;
            Ok(())
        }
        batch(client, "SET XACT_ABORT ON; BEGIN TRANSACTION").await.map_err(BatchError::general)?;
        let mut total = 0;
        for (i, s) in stmts.iter().enumerate() {
            let mut q = Query::new(s.sql.as_str());
            for p in &s.params {
                bind_value(&mut q, p);
            }
            let res = async {
                let n = q.execute(&mut *client).await?.total();
                check_affected(s, n)?;
                Ok::<u64, anyhow::Error>(n)
            }
            .await;
            match res {
                Ok(n) => total += n,
                Err(e) => {
                    // If the rollback fails the client is dropped (closing the connection rolls back).
                    if batch(client, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await.is_ok() {
                        conn.release();
                    }
                    return Err(BatchError::at(i, e));
                }
            }
        }
        batch(client, "COMMIT TRANSACTION").await.map_err(BatchError::general)?;
        conn.release();
        Ok(total)
    }

    async fn close(&self) {
        self.pools.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::declared_type;

    #[test]
    fn declared_types() {
        assert_eq!(declared_type("nvarchar", 100, 0, 0), "nvarchar(50)");
        assert_eq!(declared_type("varbinary", -1, 0, 0), "varbinary(max)");
        assert_eq!(declared_type("decimal", 9, 10, 2), "decimal(10,2)");
        assert_eq!(declared_type("int", 4, 10, 0), "int");
    }
}
