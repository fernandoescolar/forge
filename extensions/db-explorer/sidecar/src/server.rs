//! Request loop and method dispatch.

use crate::engines::{self, ApplyChangesParams, ConnectParams, Engine, FetchTableParams, nonempty};
use crate::mongo::{self, Mongo};
use crate::protocol::{Request, RpcError, response};
use anyhow::Result;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, Id, JoinSet};

/// Outcome of an in-flight `connect`, published to requests waiting on the same connectionId.
type ConnectOutcome = Option<Result<(), RpcError>>;

#[derive(Default)]
pub struct Server {
    conns: RwLock<HashMap<String, Arc<dyn Engine>>>,
    /// MongoDB connections: documents, not tables, so not an [`Engine`].
    mongo: RwLock<HashMap<String, Arc<Mongo>>>,
    inflight: Mutex<HashMap<String, (Id, AbortHandle)>>,
    /// connectionId -> (generation, outcome) for connects that have not finished yet.
    pending_connects: Mutex<HashMap<String, (u64, watch::Receiver<ConnectOutcome>)>>,
    next_gen: AtomicU64,
}

/// Handed to the task running a `connect`; publishes its outcome to waiting requests.
pub struct ConnectTicket {
    connection_id: String,
    generation: u64,
    tx: watch::Sender<ConnectOutcome>,
}

/// Read requests from `input` until EOF, writing one response line per request to `output`.
pub async fn run<R, W>(input: R, output: W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let server = Arc::new(Server::default());
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut out = output;
        while let Some(line) = rx.recv().await {
            let ok = out.write_all(line.as_bytes()).await.is_ok()
                && out.write_all(b"\n").await.is_ok()
                && out.flush().await.is_ok();
            if !ok {
                break;
            }
        }
    });

    let mut tasks = JoinSet::new();
    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req = match server.parse_request(&line) {
            Ok(req) => req,
            Err(resp) => {
                let _ = tx.send(resp);
                continue;
            }
        };
        // Register connects before spawning, so requests on later lines see them as pending.
        let ticket = server.register_connect(&req);
        let server = server.clone();
        let tx = tx.clone();
        tasks.spawn(async move {
            let _ = tx.send(server.handle(req, ticket).await);
        });
        while tasks.try_join_next().is_some() {}
    }

    // stdin closed: give in-flight requests a moment to answer, then shut down.
    let _ = tokio::time::timeout(Duration::from_secs(2), async { while tasks.join_next().await.is_some() {} }).await;
    tasks.abort_all();
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
    server.close_all().await;
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnRef {
    connection_id: String,
    database: Option<String>,
    schema: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DescribeParams {
    connection_id: String,
    database: Option<String>,
    schema: Option<String>,
    table: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryParams {
    connection_id: String,
    database: Option<String>,
    sql: String,
    max_rows: Option<usize>,
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(|e| RpcError::new("invalid_params", format!("invalid params: {e}")))
}

fn to_json<T: serde::Serialize>(v: T) -> Result<Value, RpcError> {
    serde_json::to_value(v).map_err(|e| RpcError::new("internal", e.to_string()))
}

/// Normalize a client-supplied request id (number or string) into a map key.
fn request_key(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Server {
    /// Parse a request line; unparseable lines yield the error response (id null if unknown).
    pub fn parse_request(&self, line: &str) -> Result<Request, String> {
        serde_json::from_str(line).map_err(|e| {
            let id = serde_json::from_str::<Value>(line).ok().and_then(|v| v.get("id").cloned()).unwrap_or(Value::Null);
            response(id, Err(RpcError::new("parse_error", format!("invalid request: {e}"))))
        })
    }

    /// For `connect` requests, mark the connectionId as pending. Must be called in stdin order.
    pub fn register_connect(&self, req: &Request) -> Option<ConnectTicket> {
        if req.method != "connect" {
            return None;
        }
        let connection_id = req.params.get("connectionId")?.as_str()?.to_string();
        let generation = self.next_gen.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = watch::channel(None);
        self.pending_connects.lock().unwrap_or_else(|e| e.into_inner()).insert(connection_id.clone(), (generation, rx));
        Some(ConnectTicket { connection_id, generation, tx })
    }

    fn finish_connect(&self, ticket: ConnectTicket, outcome: Result<(), RpcError>) {
        let mut pending = self.pending_connects.lock().unwrap_or_else(|e| e.into_inner());
        if pending.get(&ticket.connection_id).is_some_and(|(g, _)| *g == ticket.generation) {
            pending.remove(&ticket.connection_id);
        }
        let _ = ticket.tx.send(Some(outcome));
    }

    /// Handle one parsed request and return its response line.
    pub async fn handle(self: &Arc<Self>, req: Request, ticket: Option<ConnectTicket>) -> String {
        let cancel_key = matches!(req.method.as_str(), "query" | "fetchTable" | "find" | "aggregate")
            .then(|| req.params.get("requestId").and_then(request_key))
            .flatten();

        // Run the request in its own task so panics are contained and queries can be aborted.
        let server = self.clone();
        let (method, params) = (req.method, req.params);
        let handle = tokio::spawn(async move { server.dispatch(&method, params, ticket).await });
        if let Some(key) = &cancel_key {
            self.inflight.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), (handle.id(), handle.abort_handle()));
        }
        let task_id = handle.id();
        let result = match handle.await {
            Ok(r) => r,
            Err(e) if e.is_cancelled() => Err(RpcError::cancelled()),
            Err(e) => {
                eprintln!("forge-sql: request {} panicked: {e}", req.id);
                Err(RpcError::new("internal", "internal error while handling the request"))
            }
        };
        if let Some(key) = &cancel_key {
            let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
            if inflight.get(key).is_some_and(|(id, _)| *id == task_id) {
                inflight.remove(key);
            }
        }
        response(req.id, result)
    }

    /// Waits for an in-flight `connect` on `id`, if there is one; its error if it failed.
    async fn connected(&self, id: &str) -> Result<(), RpcError> {
        let pending = self.pending_connects.lock().unwrap_or_else(|e| e.into_inner()).get(id).map(|(_, rx)| rx.clone());
        if let Some(mut rx) = pending {
            // If the connect task died without an answer (Err), fall through to the plain lookup.
            if let Ok(outcome) = rx.wait_for(Option::is_some).await
                && let Some(Err(e)) = &*outcome
            {
                return Err(e.clone());
            }
        }
        Ok(())
    }

    /// A MongoDB connection, once its `connect` has finished.
    async fn mongo(&self, id: &str) -> Result<Arc<Mongo>, RpcError> {
        self.connected(id).await?;
        self.mongo
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| RpcError::new("not_connected", format!("no MongoDB connection with id \"{id}\"")))
    }

    fn is_mongo(&self, id: &str) -> bool {
        self.mongo.read().unwrap_or_else(|e| e.into_inner()).contains_key(id)
    }

    /// Look up a connection, waiting for an in-flight `connect` on the same id if there is one.
    async fn engine(&self, id: &str) -> Result<Arc<dyn Engine>, RpcError> {
        self.connected(id).await?;
        self.conns
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| RpcError::new("not_connected", format!("no connection with id \"{id}\"")))
    }

    async fn close_all(&self) {
        let all: Vec<_> = self.conns.write().unwrap_or_else(|e| e.into_inner()).drain().collect();
        for (_, e) in all {
            let _ = tokio::time::timeout(Duration::from_secs(1), e.close()).await;
        }
        let all: Vec<_> = self.mongo.write().unwrap_or_else(|e| e.into_inner()).drain().collect();
        for (_, m) in all {
            let _ = tokio::time::timeout(Duration::from_secs(1), m.close()).await;
        }
    }

    /// The MongoDB methods (documents and collections).
    async fn dispatch_mongo(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "listCollections" => {
                let p: ConnRef = parse(params)?;
                let database = p.database.ok_or_else(|| RpcError::new("invalid_params", "missing database"))?;
                to_json(self.mongo(&p.connection_id).await?.list_collections(&database).await?)
            }
            "listIndexes" => {
                let p: mongo::DocumentParams = parse(params)?;
                to_json(self.mongo(&p.connection_id).await?.list_indexes(&p.database, &p.collection).await?)
            }
            "find" => {
                let p: mongo::FindParams = parse(params)?;
                to_json(self.mongo(&p.connection_id).await?.find(&p).await?)
            }
            "aggregate" => {
                let p: mongo::AggregateParams = parse(params)?;
                to_json(self.mongo(&p.connection_id).await?.aggregate(&p).await?)
            }
            "insertDocument" => {
                let p: mongo::DocumentParams = parse(params)?;
                let id = self.mongo(&p.connection_id).await?.insert(&p.database, &p.collection, p.document()?).await?;
                Ok(json!({ "id": id }))
            }
            "replaceDocument" => {
                let p: mongo::DocumentParams = parse(params)?;
                self.mongo(&p.connection_id).await?.replace(&p.database, &p.collection, p.id()?, p.document()?).await?;
                Ok(Value::Null)
            }
            "deleteDocument" => {
                let p: mongo::DocumentParams = parse(params)?;
                self.mongo(&p.connection_id).await?.delete(&p.database, &p.collection, p.id()?).await?;
                Ok(Value::Null)
            }
            other => Err(RpcError::new("unknown_method", format!("unknown method \"{other}\""))),
        }
    }

    async fn dispatch(self: Arc<Self>, method: &str, params: Value, ticket: Option<ConnectTicket>) -> Result<Value, RpcError> {
        match method {
            "ping" => Ok(json!("pong")),
            "connect" => {
                let connected = async {
                    let p: ConnectParams = parse(params)?;
                    if p.engine == "mongodb" {
                        let (conn, info) = Mongo::connect(&p).await?;
                        let old = self.mongo.write().unwrap_or_else(|e| e.into_inner()).insert(p.connection_id.clone(), Arc::new(conn));
                        if let Some(old) = old {
                            tokio::spawn(async move { old.close().await });
                        }
                        return to_json(info);
                    }
                    let (engine, info) = engines::connect(&p).await?;
                    let old = self.conns.write().unwrap_or_else(|e| e.into_inner()).insert(p.connection_id.clone(), engine);
                    if let Some(old) = old {
                        tokio::spawn(async move { old.close().await });
                    }
                    to_json(info)
                }
                .await;
                if let Some(ticket) = ticket {
                    self.finish_connect(ticket, connected.as_ref().map(|_| ()).map_err(Clone::clone));
                }
                connected
            }
            "disconnect" => {
                let p: ConnRef = parse(params)?;
                let old = self.conns.write().unwrap_or_else(|e| e.into_inner()).remove(&p.connection_id);
                if let Some(old) = old {
                    old.close().await;
                }
                let old = self.mongo.write().unwrap_or_else(|e| e.into_inner()).remove(&p.connection_id);
                if let Some(old) = old {
                    old.close().await;
                }
                Ok(Value::Null)
            }
            "listDatabases" => {
                let p: ConnRef = parse(params)?;
                self.connected(&p.connection_id).await?;
                if self.is_mongo(&p.connection_id) {
                    return to_json(self.mongo(&p.connection_id).await?.list_databases().await?);
                }
                to_json(self.engine(&p.connection_id).await?.list_databases().await?)
            }
            "listCollections" | "listIndexes" | "find" | "aggregate" | "insertDocument" | "replaceDocument" | "deleteDocument" => self.dispatch_mongo(method, params).await,
            "listSchemas" => {
                let p: ConnRef = parse(params)?;
                to_json(self.engine(&p.connection_id).await?.list_schemas(nonempty(p.database.as_deref())).await?)
            }
            "listObjects" => {
                let p: ConnRef = parse(params)?;
                let e = self.engine(&p.connection_id).await?;
                to_json(engines::list_objects(&*e, nonempty(p.database.as_deref()), nonempty(p.schema.as_deref())).await?)
            }
            "describe" => {
                let p: DescribeParams = parse(params)?;
                let e = self.engine(&p.connection_id).await?;
                to_json(e.describe(nonempty(p.database.as_deref()), nonempty(p.schema.as_deref()), &p.table).await?)
            }
            "query" => {
                let p: QueryParams = parse(params)?;
                let e = self.engine(&p.connection_id).await?;
                let start = Instant::now();
                let sets = e.query(nonempty(p.database.as_deref()), &p.sql, p.max_rows.unwrap_or(1000)).await?;
                Ok(json!({ "resultSets": sets, "elapsedMs": start.elapsed().as_millis() as u64 }))
            }
            "fetchTable" => {
                let id = params.get("connectionId").and_then(Value::as_str).unwrap_or_default().to_string();
                let mut p: FetchTableParams = parse(params)?;
                p.database = nonempty(p.database.as_deref()).map(str::to_string);
                p.schema = nonempty(p.schema.as_deref()).map(str::to_string);
                to_json(engines::fetch_table(&*self.engine(&id).await?, &p).await?)
            }
            "applyChanges" => {
                let id = params.get("connectionId").and_then(Value::as_str).unwrap_or_default().to_string();
                let mut p: ApplyChangesParams = parse(params)?;
                p.database = nonempty(p.database.as_deref()).map(str::to_string);
                p.schema = nonempty(p.schema.as_deref()).map(str::to_string);
                let e = self.engine(&id).await?;
                match engines::apply_changes(&*e, &p).await {
                    Ok(applied) => Ok(json!({ "applied": applied })),
                    Err(be) => {
                        let mut err = RpcError::from(be.error);
                        if let Some(i) = be.index {
                            let kind = p.changes.get(i).map(|c| c.kind()).unwrap_or("change");
                            err.message = format!("Change {i} ({kind}) failed, nothing was applied: {}", err.message);
                            err.index = Some(i);
                            err.code.get_or_insert_with(|| "change_failed".into());
                        }
                        Err(err)
                    }
                }
            }
            "cancel" => {
                let key = params.get("requestId").and_then(request_key);
                let handle = key.and_then(|k| self.inflight.lock().unwrap_or_else(|e| e.into_inner()).remove(&k));
                let cancelled = match handle {
                    Some((_, h)) => {
                        h.abort();
                        true
                    }
                    None => false,
                };
                Ok(json!({ "cancelled": cancelled }))
            }
            other => Err(RpcError::new("unknown_method", format!("unknown method \"{other}\""))),
        }
    }
}
