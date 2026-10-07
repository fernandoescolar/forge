//! Redis: databases (0–15), their keys (found with SCAN, never KEYS), each key's value by
//! type (string, hash, list, set, sorted set, stream) a page at a time, editing every type,
//! TTLs, and a console that runs command lines like `redis-cli`.
//!
//! A connection goes to one server (host and port, or a `redis://` / `rediss://` URL), or
//! to the master that Redis Sentinel names. Each database gets its own connection, opened on
//! first use; when one drops (a restart, a Sentinel failover) the master is looked up again
//! and the request tried once more.
//!
//! Keys, fields and values travel as text: as they are when they are UTF-8, else escaped as
//! `redis-cli` does (`\xNN`), with a flag, so that an edit writes the same bytes back.

pub mod text;

use anyhow::{Context, Result, anyhow, bail};
use redis::aio::MultiplexedConnection;
use redis::sentinel::{SentinelClient, SentinelClientBuilder, SentinelServerType};
use redis::{Client, ConnectionAddr, ConnectionInfo, IntoConnectionInfo, Value};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use tokio::sync::Mutex;

use crate::engines::{ConnectParams, ServerInfo};

/// Where connections go: one server, or the master Sentinel names (looked up again after a
/// failure).
enum Source {
    Server(ConnectionInfo),
    Sentinel(Mutex<SentinelClient>),
}

pub struct Redis {
    source: Source,
    default_db: i64,
    conns: Mutex<HashMap<i64, MultiplexedConnection>>,
}

/// `host:port` (6379 when left out; 26379 for a sentinel).
fn address(text: &str, default_port: u16, tls: bool, insecure: bool) -> Result<ConnectionAddr> {
    let text = text.trim();
    let (host, port) = match text.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !host.ends_with(':') => (host.trim_matches(['[', ']']), port.parse().with_context(|| format!("“{port}” is not a port"))?),
        _ => (text, default_port),
    };
    if host.is_empty() {
        bail!("missing host");
    }
    Ok(if tls { ConnectionAddr::TcpTls { host: host.to_string(), port, insecure, tls_params: None } } else { ConnectionAddr::Tcp(host.to_string(), port) })
}

fn database(p: &ConnectParams) -> Result<i64> {
    match p.database.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        None => Ok(0),
        Some(d) => d.parse().map_err(|_| anyhow!("the database is a number (0–15), not “{d}”")),
    }
}

impl Redis {
    pub async fn connect(p: &ConnectParams) -> Result<(Self, ServerInfo)> {
        let tls = p.ssl.as_deref() == Some("require");
        let insecure = tls && p.trust_server_certificate == Some(true);
        let user = p.user.as_deref().filter(|u| !u.is_empty());
        let password = p.password.as_deref().filter(|p| !p.is_empty());
        let (source, default_db) = if let Some(url) = p.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            let info = url.into_connection_info().map_err(|e| anyhow!("invalid connection URL: {e}"))?;
            let db = info.redis_settings().db();
            (Source::Server(info), db)
        } else if let Some(sentinels) = p.sentinels.as_deref().filter(|s| !s.trim().is_empty()) {
            let master = p.master_name.as_deref().map(str::trim).filter(|m| !m.is_empty()).unwrap_or("mymaster");
            let nodes = sentinels.split([',', ' ', '\n']).filter(|s| !s.trim().is_empty()).map(|s| address(s, 26379, false, false)).collect::<Result<Vec<_>>>()?;
            let db = database(p)?;
            let mut builder = SentinelClientBuilder::new(nodes, master, SentinelServerType::Master)?.set_client_to_redis_db(db);
            if let Some(user) = user {
                builder = builder.set_client_to_redis_username(user);
            }
            if let Some(password) = password {
                builder = builder.set_client_to_redis_password(password);
            }
            if let Some(password) = p.sentinel_password.as_deref().filter(|p| !p.is_empty()) {
                builder = builder.set_client_to_sentinel_password(password);
            }
            if tls {
                let mode = if insecure { redis::TlsMode::Insecure } else { redis::TlsMode::Secure };
                builder = builder.set_client_to_redis_tls_mode(mode).set_client_to_sentinel_tls_mode(mode);
            }
            (Source::Sentinel(Mutex::new(builder.build()?)), db)
        } else {
            let host = p.host.as_deref().filter(|h| !h.is_empty()).unwrap_or("localhost");
            let addr = address(&format!("{host}:{}", p.port.unwrap_or(6379)), 6379, tls, insecure)?;
            let db = database(p)?;
            let mut settings = redis::RedisConnectionInfo::default().set_db(db);
            if let Some(user) = user {
                settings = settings.set_username(user);
            }
            if let Some(password) = password {
                settings = settings.set_password(password);
            }
            let info = addr.into_connection_info()?.set_redis_settings(settings);
            (Source::Server(info), db)
        };
        let redis = Self { source, default_db, conns: Mutex::new(HashMap::new()) };
        // Connecting is lazy: ask the server something, so a wrong host or password fails here.
        let info: String = redis.run(default_db, redis::cmd("INFO").arg("server")).await?;
        let version = info_field(&info, "redis_version").unwrap_or("unknown");
        let product = if info_field(&info, "valkey_version").is_some() { "Valkey" } else { "Redis" };
        let mode = info_field(&info, "redis_mode").filter(|m| *m != "standalone").map(|m| format!(" ({m})")).unwrap_or_default();
        Ok((redis, ServerInfo { server_version: format!("{product} {version}{mode}"), default_database: Some(default_db.to_string()), engine: "redis".into() }))
    }

    pub async fn close(&self) {
        self.conns.lock().await.clear();
    }

    /// A client for the server to talk to now (for Sentinel: the master it names now).
    async fn client(&self, db: i64) -> Result<Client> {
        let client = match &self.source {
            Source::Server(info) => Client::open(info.clone())?,
            Source::Sentinel(sentinel) => sentinel.lock().await.async_get_client().await?,
        };
        let info = client.get_connection_info().clone();
        let settings = info.redis_settings().clone().set_db(db);
        Ok(Client::open(info.set_redis_settings(settings))?)
    }

    async fn conn(&self, db: i64) -> Result<MultiplexedConnection> {
        let mut conns = self.conns.lock().await;
        if let Some(conn) = conns.get(&db) {
            return Ok(conn.clone());
        }
        let conn = self.client(db).await?.get_multiplexed_async_connection().await?;
        conns.insert(db, conn.clone());
        Ok(conn)
    }

    /// Runs `cmd` on database `db`, reconnecting once when the connection dropped.
    async fn run<T: redis::FromRedisValue>(&self, db: i64, cmd: &redis::Cmd) -> Result<T> {
        let mut conn = self.conn(db).await?;
        match cmd.query_async(&mut conn).await {
            Ok(v) => Ok(v),
            Err(e) if e.is_io_error() || e.is_connection_dropped() || e.is_unrecoverable_error() => {
                self.conns.lock().await.remove(&db);
                let mut conn = self.conn(db).await?;
                Ok(cmd.query_async(&mut conn).await?)
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn pipe<T: redis::FromRedisValue>(&self, db: i64, pipe: &redis::Pipeline) -> Result<T> {
        let mut conn = self.conn(db).await?;
        match pipe.query_async(&mut conn).await {
            Ok(v) => Ok(v),
            Err(e) if e.is_io_error() || e.is_connection_dropped() || e.is_unrecoverable_error() => {
                self.conns.lock().await.remove(&db);
                let mut conn = self.conn(db).await?;
                Ok(pipe.query_async(&mut conn).await?)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The databases and their key counts: all of them (CONFIG GET databases, else 16);
    /// managed services that forbid CONFIG still answer INFO keyspace.
    pub async fn databases(&self) -> Result<Vec<DatabaseInfo>> {
        let count = match self.run::<Vec<String>>(self.default_db, redis::cmd("CONFIG").arg("GET").arg("databases")).await {
            Ok(pair) => pair.get(1).and_then(|n| n.parse::<i64>().ok()).unwrap_or(16),
            Err(_) => 16,
        };
        let info: String = self.run(self.default_db, redis::cmd("INFO").arg("keyspace")).await?;
        let keys: HashMap<i64, u64> = info
            .lines()
            .filter_map(|line| {
                let (db, rest) = line.strip_prefix("db")?.split_once(':')?;
                let keys = rest.split(',').find_map(|kv| kv.strip_prefix("keys="))?.parse().ok()?;
                Some((db.parse().ok()?, keys))
            })
            .collect();
        Ok((0..count.max(self.default_db + 1)).map(|db| DatabaseInfo { db, keys: keys.get(&db).copied().unwrap_or(0) }).collect())
    }

    /// One SCAN step: up to about `count` keys matching `pattern`, with their types.
    pub async fn scan(&self, p: &ScanParams) -> Result<Scanned> {
        let mut cmd = redis::cmd("SCAN");
        cmd.arg(p.cursor.as_deref().unwrap_or("0")).arg("MATCH").arg(p.pattern.as_deref().filter(|s| !s.is_empty()).unwrap_or("*")).arg("COUNT").arg(p.count.unwrap_or(1000));
        if let Some(kind) = p.r#type.as_deref().filter(|t| !t.is_empty()) {
            cmd.arg("TYPE").arg(kind);
        }
        let (cursor, keys): (String, Vec<Vec<u8>>) = self.run(p.db, &cmd).await?;
        let mut types = redis::pipe();
        for key in &keys {
            types.cmd("TYPE").arg(key);
        }
        let types: Vec<String> = if keys.is_empty() { vec![] } else { self.pipe(p.db, &types).await? };
        let keys = keys
            .iter()
            .zip(types)
            .map(|(key, kind)| {
                let (name, escaped) = text::display(key);
                ScannedKey { name, escaped, r#type: kind }
            })
            .collect();
        Ok(Scanned { cursor, keys })
    }

    /// A key's type, TTL, size and a page of its value.
    pub async fn get(&self, p: &KeyParams) -> Result<KeyValue> {
        let key = p.key()?;
        let mut head = redis::pipe();
        head.cmd("TYPE").arg(&key).cmd("PTTL").arg(&key);
        let (kind, pttl): (String, i64) = self.pipe(p.db, &head).await?;
        if kind == "none" {
            bail!("the key {} doesn't exist (any more)", p.key);
        }
        let limit = p.limit.unwrap_or(500).max(1);
        let offset = p.offset.unwrap_or(0);
        let cursor = p.cursor.clone().unwrap_or_else(|| "0".into());
        let show = |bytes: &[u8]| text::display(bytes);
        let mut value = KeyValue { key: p.key.clone(), r#type: kind.clone(), ttl: if pttl < 0 { pttl } else { (pttl + 999) / 1000 }, length: 0, text: None, escaped: false, columns: vec![], rows: vec![], escaped_rows: vec![], next: None };
        match kind.as_str() {
            "string" => {
                let bytes: Vec<u8> = self.run(p.db, redis::cmd("GET").arg(&key)).await?;
                value.length = bytes.len() as u64;
                let (text, escaped) = show(&bytes);
                value.text = Some(text);
                value.escaped = escaped;
            }
            "hash" => {
                value.length = self.run(p.db, redis::cmd("HLEN").arg(&key)).await?;
                let (next, pairs): (String, Vec<Vec<u8>>) = self.run(p.db, redis::cmd("HSCAN").arg(&key).arg(&cursor).arg("COUNT").arg(limit)).await?;
                value.columns = vec!["field".into(), "value".into()];
                for pair in pairs.chunks(2) {
                    let ((f, fe), (v, ve)) = (show(&pair[0]), show(pair.get(1).map(Vec::as_slice).unwrap_or_default()));
                    value.rows.push(vec![json!(f), json!(v)]);
                    value.escaped_rows.push(fe || ve);
                }
                value.next = (next != "0").then(|| json!(next));
            }
            "list" => {
                value.length = self.run(p.db, redis::cmd("LLEN").arg(&key)).await?;
                let items: Vec<Vec<u8>> = self.run(p.db, redis::cmd("LRANGE").arg(&key).arg(offset).arg(offset + limit as i64 - 1)).await?;
                value.columns = vec!["index".into(), "value".into()];
                for (i, item) in items.iter().enumerate() {
                    let (v, e) = show(item);
                    value.rows.push(vec![json!(offset + i as i64), json!(v)]);
                    value.escaped_rows.push(e);
                }
                value.next = ((offset + items.len() as i64) < value.length as i64).then(|| json!(offset + items.len() as i64));
            }
            "set" => {
                value.length = self.run(p.db, redis::cmd("SCARD").arg(&key)).await?;
                let (next, members): (String, Vec<Vec<u8>>) = self.run(p.db, redis::cmd("SSCAN").arg(&key).arg(&cursor).arg("COUNT").arg(limit)).await?;
                value.columns = vec!["member".into()];
                for m in &members {
                    let (v, e) = show(m);
                    value.rows.push(vec![json!(v)]);
                    value.escaped_rows.push(e);
                }
                value.next = (next != "0").then(|| json!(next));
            }
            "zset" => {
                value.length = self.run(p.db, redis::cmd("ZCARD").arg(&key)).await?;
                let items: Vec<(Vec<u8>, f64)> = self.run(p.db, redis::cmd("ZRANGE").arg(&key).arg(offset).arg(offset + limit as i64 - 1).arg("WITHSCORES")).await?;
                value.columns = vec!["member".into(), "score".into()];
                for (m, score) in &items {
                    let (v, e) = show(m);
                    value.rows.push(vec![json!(v), json!(score)]);
                    value.escaped_rows.push(e);
                }
                value.next = ((offset + items.len() as i64) < value.length as i64).then(|| json!(offset + items.len() as i64));
            }
            "stream" => {
                value.length = self.run(p.db, redis::cmd("XLEN").arg(&key)).await?;
                let start = p.cursor.as_deref().map(|id| format!("({id}")).unwrap_or_else(|| "-".into());
                let entries: Vec<(String, Vec<Vec<u8>>)> = self.run(p.db, redis::cmd("XRANGE").arg(&key).arg(&start).arg("+").arg("COUNT").arg(limit)).await?;
                value.columns = vec!["id".into(), "fields".into()];
                for (id, fields) in &entries {
                    let mut map = serde_json::Map::new();
                    let mut escaped = false;
                    for pair in fields.chunks(2) {
                        let ((f, fe), (v, ve)) = (show(&pair[0]), show(pair.get(1).map(Vec::as_slice).unwrap_or_default()));
                        escaped |= fe || ve;
                        map.insert(f, json!(v));
                    }
                    value.rows.push(vec![json!(id), json!(serde_json::Value::Object(map).to_string())]);
                    value.escaped_rows.push(escaped);
                }
                value.next = (entries.len() == limit).then(|| entries.last().map(|(id, _)| json!(id))).flatten();
            }
            other => bail!("keys of type {other} can't be shown (only with the console)"),
        }
        Ok(value)
    }

    /// One change to a key's value; see [`Edit`].
    pub async fn edit(&self, p: &EditParams) -> Result<()> {
        let key = text::from_display(&p.key, p.key_escaped)?;
        let bytes = |text: &str| text::from_display(text, p.escaped);
        let db = p.db;
        let exists: bool = self.run(db, redis::cmd("EXISTS").arg(&key)).await?;
        let creating = matches!(p.edit, Edit::Create { .. });
        if creating && exists {
            bail!("a key named {} already exists", p.key);
        }
        if !creating && !exists && !matches!(p.edit, Edit::SetString { .. }) {
            bail!("the key {} doesn't exist (any more)", p.key);
        }
        let mut cmd = redis::Cmd::new();
        match &p.edit {
            Edit::SetString { value } => {
                cmd.arg("SET").arg(&key).arg(bytes(value)?).arg("KEEPTTL");
            }
            Edit::HashSet { field, value } => {
                cmd.arg("HSET").arg(&key).arg(bytes(field)?).arg(bytes(value)?);
            }
            Edit::HashDelete { fields } => {
                cmd.arg("HDEL").arg(&key);
                for f in fields {
                    cmd.arg(bytes(f)?);
                }
            }
            Edit::ListSet { index, value } => {
                cmd.arg("LSET").arg(&key).arg(*index).arg(bytes(value)?);
            }
            Edit::ListPush { value, head } => {
                cmd.arg(if *head { "LPUSH" } else { "RPUSH" }).arg(&key).arg(bytes(value)?);
            }
            Edit::ListDelete { indexes } => {
                // By index: mark each, then remove the marks (LREM works by value).
                let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
                let mark = format!("__forge_deleted_{nanos}_{}__", std::process::id());
                let mut pipe = redis::pipe();
                pipe.atomic();
                for i in indexes {
                    pipe.cmd("LSET").arg(&key).arg(*i).arg(&mark).ignore();
                }
                pipe.cmd("LREM").arg(&key).arg(0).arg(&mark).ignore();
                return self.pipe(db, &pipe).await;
            }
            Edit::SetAdd { member } => {
                cmd.arg("SADD").arg(&key).arg(bytes(member)?);
            }
            Edit::SetDelete { members } => {
                cmd.arg("SREM").arg(&key);
                for m in members {
                    cmd.arg(bytes(m)?);
                }
            }
            Edit::SetRename { member, to } => {
                let mut pipe = redis::pipe();
                pipe.atomic().cmd("SREM").arg(&key).arg(bytes(member)?).ignore().cmd("SADD").arg(&key).arg(bytes(to)?).ignore();
                return self.pipe(db, &pipe).await;
            }
            Edit::ZSetAdd { member, score } => {
                cmd.arg("ZADD").arg(&key).arg(*score).arg(bytes(member)?);
            }
            Edit::ZSetDelete { members } => {
                cmd.arg("ZREM").arg(&key);
                for m in members {
                    cmd.arg(bytes(m)?);
                }
            }
            Edit::ZSetRename { member, to } => {
                let mut pipe = redis::pipe();
                let score: Option<f64> = self.run(db, redis::cmd("ZSCORE").arg(&key).arg(bytes(member)?)).await?;
                let score = score.ok_or_else(|| anyhow!("the member is gone"))?;
                pipe.atomic().cmd("ZREM").arg(&key).arg(bytes(member)?).ignore().cmd("ZADD").arg(&key).arg(score).arg(bytes(to)?).ignore();
                return self.pipe(db, &pipe).await;
            }
            Edit::StreamAdd { fields } => {
                if fields.is_empty() {
                    bail!("a stream entry needs at least one field");
                }
                cmd.arg("XADD").arg(&key).arg("*");
                for (f, v) in fields {
                    cmd.arg(bytes(f)?).arg(bytes(v)?);
                }
            }
            Edit::StreamDelete { ids } => {
                cmd.arg("XDEL").arg(&key);
                for id in ids {
                    cmd.arg(id);
                }
            }
            Edit::Create { r#type, field, value, score } => {
                let value = bytes(value.as_deref().unwrap_or_default())?;
                match r#type.as_str() {
                    "string" => cmd.arg("SET").arg(&key).arg(value).arg("NX"),
                    "hash" => cmd.arg("HSET").arg(&key).arg(bytes(field.as_deref().filter(|f| !f.is_empty()).ok_or_else(|| anyhow!("a hash needs a first field"))?)?).arg(value),
                    "list" => cmd.arg("RPUSH").arg(&key).arg(value),
                    "set" => cmd.arg("SADD").arg(&key).arg(value),
                    "zset" => cmd.arg("ZADD").arg(&key).arg(score.unwrap_or(0.0)).arg(value),
                    "stream" => cmd.arg("XADD").arg(&key).arg("*").arg(bytes(field.as_deref().filter(|f| !f.is_empty()).ok_or_else(|| anyhow!("a stream entry needs a field"))?)?).arg(value),
                    other => bail!("can't create a key of type {other}"),
                };
            }
        }
        let _: Value = self.run(db, &cmd).await?;
        Ok(())
    }

    /// Sets the key's time to live in seconds, or removes it (`None`).
    pub async fn expire(&self, db: i64, key: &[u8], ttl: Option<i64>) -> Result<()> {
        let done: i64 = match ttl {
            Some(seconds) if seconds > 0 => self.run(db, redis::cmd("EXPIRE").arg(key).arg(seconds)).await?,
            Some(_) => bail!("a time to live is at least 1 second"),
            None => {
                let exists: bool = self.run(db, redis::cmd("EXISTS").arg(key)).await?;
                let _: i64 = self.run(db, redis::cmd("PERSIST").arg(key)).await?;
                i64::from(exists)
            }
        };
        if done == 0 {
            bail!("the key doesn't exist (any more)");
        }
        Ok(())
    }

    /// Renames a key; fails when the new name is taken.
    pub async fn rename(&self, db: i64, key: &[u8], to: &[u8]) -> Result<()> {
        let renamed: i64 = self.run(db, redis::cmd("RENAMENX").arg(key).arg(to)).await?;
        if renamed == 0 {
            bail!("a key with that name already exists");
        }
        Ok(())
    }

    /// Deletes keys (UNLINK: freed in the background), returning how many existed.
    pub async fn delete(&self, db: i64, keys: &[Vec<u8>]) -> Result<i64> {
        if keys.is_empty() {
            return Ok(0);
        }
        let mut cmd = redis::cmd("UNLINK");
        for key in keys {
            cmd.arg(key);
        }
        match self.run(db, &cmd).await {
            Ok(n) => Ok(n),
            // Before Redis 4.
            Err(e) if e.to_string().contains("unknown command") => {
                let mut cmd = redis::cmd("DEL");
                for key in keys {
                    cmd.arg(key);
                }
                self.run(db, &cmd).await
            }
            Err(e) => Err(e),
        }
    }

    /// Runs a command line as `redis-cli` would and returns its reply as `redis-cli` prints
    /// it; errors from the server are replies too.
    pub async fn command(&self, db: i64, line: &str) -> Result<String> {
        let args = text::split(line)?;
        let Some(name) = args.first() else { return Ok(String::new()) };
        let name = String::from_utf8_lossy(name).to_uppercase();
        // Commands that would take the shared connection over (or change its protocol).
        let takes_over = matches!(name.as_str(), "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "MONITOR" | "SYNC" | "PSYNC" | "QUIT" | "RESET" | "HELLO")
            || (name == "CLIENT" && args.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"REPLY")));
        if takes_over {
            bail!("{name} can't run in the console (it would take over the connection)");
        }
        if name == "SELECT" {
            bail!("choose the database above instead of SELECT");
        }
        let mut cmd = redis::Cmd::new();
        for arg in &args {
            cmd.arg(arg.as_slice());
        }
        match self.run::<Value>(db, &cmd).await {
            Ok(value) => Ok(text::format_reply(&value)),
            Err(e) => match e.downcast_ref::<redis::RedisError>() {
                Some(re) if re.code().is_some() => Ok(format!("(error) {}", redis_message(re))),
                _ => Err(e),
            },
        }
    }
}

fn info_field<'a>(info: &'a str, name: &str) -> Option<&'a str> {
    info.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix(':')).map(str::trim)
}

/// The server's error (`WRONGTYPE Operation against a key…`), without the driver's wrapping.
pub fn redis_message(e: &redis::RedisError) -> String {
    match e.kind() {
        redis::ErrorKind::AuthenticationFailed => return "Authentication failed: check the user and password".into(),
        redis::ErrorKind::MasterNameNotFoundBySentinel => return "The sentinels know no master by that name".into(),
        redis::ErrorKind::NoValidReplicasFoundBySentinel => return "The sentinels know no replica to use".into(),
        _ => {}
    }
    match (e.code(), e.detail()) {
        (Some(code), Some(detail)) => format!("{code} {detail}"),
        _ if e.is_io_error() || e.is_connection_refusal() => format!("Could not reach the server: {e}"),
        _ => e.to_string(),
    }
}

#[derive(Debug, Serialize)]
pub struct DatabaseInfo {
    pub db: i64,
    pub keys: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanParams {
    pub connection_id: String,
    pub db: i64,
    pub pattern: Option<String>,
    pub cursor: Option<String>,
    pub count: Option<u64>,
    pub r#type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Scanned {
    /// `"0"` when the scan is complete.
    pub cursor: String,
    pub keys: Vec<ScannedKey>,
}

#[derive(Debug, Serialize)]
pub struct ScannedKey {
    pub name: String,
    pub escaped: bool,
    pub r#type: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyParams {
    pub connection_id: String,
    pub db: i64,
    pub key: String,
    #[serde(default)]
    pub key_escaped: bool,
    /// Hashes and sets page with a SCAN cursor, streams with the last id seen.
    pub cursor: Option<String>,
    /// Lists and sorted sets page by position.
    pub offset: Option<i64>,
    pub limit: Option<usize>,
    /// For `expire`: seconds, or null to keep the key for ever.
    pub ttl: Option<i64>,
    /// For `rename`.
    pub to: Option<String>,
}

impl KeyParams {
    pub fn key(&self) -> Result<Vec<u8>> {
        text::from_display(&self.key, self.key_escaped)
    }
}

/// A key and a page of its value. `text` for a string; else `columns` and `rows` (hash:
/// field, value; list: index, value; set: member; sorted set: member, score; stream: id,
/// fields as JSON), and `next` to pass back as `cursor` / `offset` for the next page.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyValue {
    pub key: String,
    pub r#type: String,
    /// Seconds left, -1 without expiry.
    pub ttl: i64,
    /// Bytes of a string; entries of anything else.
    pub length: u64,
    pub text: Option<String>,
    pub escaped: bool,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    /// Rows whose text is escaped (not UTF-8): edits of them must say so.
    pub escaped_rows: Vec<bool>,
    pub next: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditParams {
    pub connection_id: String,
    pub db: i64,
    pub key: String,
    #[serde(default)]
    pub key_escaped: bool,
    /// The texts below are escaped (see [`text::display`]).
    #[serde(default)]
    pub escaped: bool,
    #[serde(flatten)]
    pub edit: Edit,
}

/// A change to one key.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Edit {
    SetString { value: String },
    HashSet { field: String, value: String },
    HashDelete { fields: Vec<String> },
    ListSet { index: i64, value: String },
    ListPush { value: String, #[serde(default)] head: bool },
    ListDelete { indexes: Vec<i64> },
    SetAdd { member: String },
    SetDelete { members: Vec<String> },
    SetRename { member: String, to: String },
    ZSetAdd { member: String, score: f64 },
    ZSetDelete { members: Vec<String> },
    ZSetRename { member: String, to: String },
    StreamAdd { fields: Vec<(String, String)> },
    StreamDelete { ids: Vec<String> },
    /// A new key: of `type`, with a first value (a hash's or stream's first `field`).
    Create { r#type: String, field: Option<String>, value: Option<String>, score: Option<f64> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_addresses() {
        assert!(matches!(address("cache.local", 6379, false, false).unwrap(), ConnectionAddr::Tcp(h, 6379) if h == "cache.local"));
        assert!(matches!(address(" 10.0.0.2:26380 ", 26379, false, false).unwrap(), ConnectionAddr::Tcp(h, 26380) if h == "10.0.0.2"));
        assert!(matches!(address("[::1]:7000", 6379, true, true).unwrap(), ConnectionAddr::TcpTls { host, port: 7000, insecure: true, .. } if host == "::1"));
        assert!(address("host:port", 6379, false, false).is_err());
        assert!(address("", 6379, false, false).is_err());
    }

    #[test]
    fn reads_edits() {
        let edit: EditParams = serde_json::from_value(json!({ "connectionId": "r", "db": 0, "key": "user:1", "op": "hashSet", "field": "name", "value": "Ada" })).unwrap();
        assert!(matches!(edit.edit, Edit::HashSet { ref field, ref value } if field == "name" && value == "Ada"));
        let edit: EditParams = serde_json::from_value(json!({ "connectionId": "r", "db": 0, "key": "s", "op": "streamAdd", "fields": [["a", "1"]] })).unwrap();
        assert!(matches!(edit.edit, Edit::StreamAdd { ref fields } if fields.len() == 1));
        assert!(serde_json::from_value::<EditParams>(json!({ "connectionId": "r", "db": 0, "key": "k", "op": "flushAll" })).is_err());
    }

    #[test]
    fn reads_info() {
        let info = "# Server\r\nredis_version:8.0.2\r\nredis_mode:standalone\r\n";
        assert_eq!(info_field(info, "redis_version"), Some("8.0.2"));
        assert_eq!(info_field(info, "redis_mode"), Some("standalone"));
        assert_eq!(info_field(info, "valkey_version"), None);
    }
}
