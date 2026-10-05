//! Bidirectional JSON-RPC 2.0 peer over a byte stream.
//!
//! Both LSP servers and ACP agents are full peers: besides answering our requests they
//! send their own requests (`workspace/configuration`, `fs/read_text_file`,
//! `session/request_permission`, ...) and notifications. [`Peer`] multiplexes all three
//! over one stream, using either LSP `Content-Length` framing or ACP newline framing.

use async_trait::async_trait;
use dashmap::DashMap;
use ide_api::{IdeError, IdeResult};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{Mutex, oneshot},
    task::JoinHandle,
};
use tracing::{debug, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// `Content-Length: N\r\n\r\n<body>` as used by LSP.
    ContentLength,
    /// One JSON document per line as used by ACP.
    Newline,
}

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;

    pub fn method_not_found(method: &str) -> Self {
        Self { code: Self::METHOD_NOT_FOUND, message: format!("method not supported by Forge: {method}") }
    }
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self { code: Self::INVALID_PARAMS, message: msg.into() }
    }
}

impl From<IdeError> for RpcError {
    fn from(e: IdeError) -> Self {
        Self { code: Self::INTERNAL, message: e.to_string() }
    }
}

/// Handles messages initiated by the remote side.
#[async_trait]
pub trait Handler: Send + Sync + 'static {
    /// Called concurrently: a slow request (e.g. waiting for user approval) does not block the stream.
    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError>;
    /// Called in arrival order, which matters for streamed chunks.
    async fn notification(&self, method: &str, params: Value);
    /// Called once when the stream closes.
    async fn closed(&self) {}
}

type Pending = DashMap<u64, oneshot::Sender<Result<Value, RpcError>>>;

pub struct Peer {
    writer: Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    framing: Framing,
    next_id: AtomicU64,
    pending: Pending,
    closed: AtomicBool,
    timeout: Duration,
}

impl Peer {
    /// Starts reading from `reader`. The returned task ends when the stream closes.
    pub fn start<R, W>(reader: R, writer: W, framing: Framing, handler: Arc<dyn Handler>, timeout: Duration) -> (Arc<Self>, JoinHandle<()>)
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let peer = Arc::new(Self {
            writer: Mutex::new(Box::new(writer)),
            framing,
            next_id: AtomicU64::new(1),
            pending: DashMap::new(),
            closed: AtomicBool::new(false),
            timeout,
        });
        let task = tokio::spawn(peer.clone().read_loop(BufReader::new(reader), handler));
        (peer, task)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub async fn request(&self, method: &str, params: Value) -> IdeResult<Value> {
        self.request_with_timeout(method, params, self.timeout).await
    }

    pub async fn request_with_timeout(&self, method: &str, params: Value, timeout: Duration) -> IdeResult<Value> {
        if self.is_closed() {
            return Err(IdeError::Unavailable(format!("connection closed, cannot send {method}")));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, tx);
        if let Err(e) = self.write(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await {
            self.pending.remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(IdeError::Protocol(format!("{method} failed ({}): {}", e.code, e.message))),
            Ok(Err(_)) => Err(IdeError::Unavailable(format!("connection closed while waiting for {method}"))),
            Err(_) => {
                self.pending.remove(&id);
                Err(IdeError::Unavailable(format!("request timed out: {method}")))
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> IdeResult<()> {
        self.write(&json!({"jsonrpc":"2.0","method":method,"params":params})).await
    }

    async fn write(&self, msg: &Value) -> IdeResult<()> {
        let body = serde_json::to_vec(msg).map_err(|e| IdeError::Protocol(e.to_string()))?;
        let mut w = self.writer.lock().await;
        let io = |e: std::io::Error| IdeError::Io(e.to_string());
        match self.framing {
            Framing::ContentLength => {
                w.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes()).await.map_err(io)?;
                w.write_all(&body).await.map_err(io)?;
            }
            Framing::Newline => {
                w.write_all(&body).await.map_err(io)?;
                w.write_all(b"\n").await.map_err(io)?;
            }
        }
        w.flush().await.map_err(io)
    }

    async fn read_loop<R: AsyncBufRead + Unpin>(self: Arc<Self>, mut reader: R, handler: Arc<dyn Handler>) {
        loop {
            let msg = match read_message(&mut reader, self.framing).await {
                Ok(Some(msg)) => msg,
                Ok(None) => break,
                Err(e) => {
                    warn!("JSON-RPC stream error: {e}");
                    break;
                }
            };
            self.dispatch(msg, &handler).await;
        }
        self.closed.store(true, Ordering::Release);
        // Dropping the senders wakes every waiter with "connection closed".
        self.pending.clear();
        handler.closed().await;
    }

    async fn dispatch(self: &Arc<Self>, msg: Value, handler: &Arc<dyn Handler>) {
        let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
        let id = msg.get("id").cloned().filter(|id| !id.is_null());
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            (Some(method), Some(id)) => {
                let (peer, handler) = (self.clone(), handler.clone());
                tokio::spawn(async move {
                    let reply = match handler.request(&method, params).await {
                        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                        Err(e) => json!({"jsonrpc":"2.0","id":id,"error":{"code":e.code,"message":e.message}}),
                    };
                    if let Err(e) = peer.write(&reply).await {
                        debug!("failed to answer {method}: {e}");
                    }
                });
            }
            (Some(method), None) => handler.notification(&method, params).await,
            (None, Some(id)) => {
                let Some((_, tx)) = id.as_u64().and_then(|id| self.pending.remove(&id)) else {
                    debug!(%id, "response for unknown request");
                    return;
                };
                let result = match msg.get("error") {
                    Some(err) => Err(RpcError {
                        code: err.get("code").and_then(Value::as_i64).unwrap_or(RpcError::INTERNAL),
                        message: err.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| err.to_string()),
                    }),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(result);
            }
            (None, None) => debug!("ignoring malformed JSON-RPC message: {msg}"),
        }
    }
}

/// Reads one framed message. `Ok(None)` means clean end of stream.
pub async fn read_message<R: AsyncBufRead + Unpin>(reader: &mut R, framing: Framing) -> anyhow::Result<Option<Value>> {
    match framing {
        Framing::Newline => loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await? == 0 {
                return Ok(None);
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(v) => return Ok(Some(v)),
                // Agents sometimes print logs to stdout; skip anything that isn't JSON.
                Err(_) => debug!("skipping non-JSON line: {line}"),
            }
        },
        Framing::ContentLength => {
            let mut content_length = None;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).await? == 0 {
                    return Ok(None);
                }
                if line == "\r\n" || line == "\n" {
                    if content_length.is_some() {
                        break;
                    }
                    continue;
                }
                if let Some((k, v)) = line.split_once(':')
                    && k.trim().eq_ignore_ascii_case("content-length") {
                        content_length = Some(v.trim().parse::<usize>()?);
                    }
            }
            let mut body = vec![0; content_length.unwrap_or_default()];
            reader.read_exact(&mut body).await?;
            Ok(Some(serde_json::from_slice(&body)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    struct Echo;
    #[async_trait]
    impl Handler for Echo {
        async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
            match method {
                "echo" => Ok(params),
                other => Err(RpcError::method_not_found(other)),
            }
        }
        async fn notification(&self, _: &str, _: Value) {}
    }

    struct Collect(tokio::sync::mpsc::UnboundedSender<(String, Value)>);
    #[async_trait]
    impl Handler for Collect {
        async fn request(&self, method: &str, _: Value) -> Result<Value, RpcError> {
            Err(RpcError::method_not_found(method))
        }
        async fn notification(&self, method: &str, params: Value) {
            let _ = self.0.send((method.to_string(), params));
        }
    }

    fn pair(framing: Framing, a: Arc<dyn Handler>, b: Arc<dyn Handler>) -> (Arc<Peer>, Arc<Peer>) {
        let (a_io, b_io) = duplex(1 << 16);
        let (ar, aw) = tokio::io::split(a_io);
        let (br, bw) = tokio::io::split(b_io);
        let (pa, _) = Peer::start(ar, aw, framing, a, Duration::from_secs(5));
        let (pb, _) = Peer::start(br, bw, framing, b, Duration::from_secs(5));
        (pa, pb)
    }

    #[tokio::test]
    async fn request_response_both_framings() {
        for framing in [Framing::ContentLength, Framing::Newline] {
            let (client, _server) = pair(framing, Arc::new(Echo), Arc::new(Echo));
            assert_eq!(client.request("echo", json!({"x":1})).await.unwrap(), json!({"x":1}));
            let err = client.request("nope", Value::Null).await.unwrap_err();
            assert!(err.to_string().contains("-32601"), "{err}");
        }
    }

    #[tokio::test]
    async fn notifications_arrive_in_order() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (client, _server) = pair(Framing::Newline, Arc::new(Echo), Arc::new(Collect(tx)));
        for i in 0..20 {
            client.notify("tick", json!(i)).await.unwrap();
        }
        for i in 0..20 {
            assert_eq!(rx.recv().await.unwrap(), ("tick".to_string(), json!(i)));
        }
    }

    #[tokio::test]
    async fn closing_stream_fails_pending_requests() {
        let (a_io, b_io) = duplex(1024);
        let (ar, aw) = tokio::io::split(a_io);
        let (peer, task) = Peer::start(ar, aw, Framing::Newline, Arc::new(Echo), Duration::from_secs(5));
        let req = tokio::spawn({
            let peer = peer.clone();
            async move { peer.request("echo", Value::Null).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(b_io);
        task.await.unwrap();
        assert!(matches!(req.await.unwrap(), Err(IdeError::Unavailable(_))));
        assert!(peer.is_closed());
    }

    #[tokio::test]
    async fn content_length_parser_handles_extra_headers() {
        let raw = b"Content-Type: application/vscode-jsonrpc\r\ncontent-length: 7\r\n\r\n{\"a\":1}".to_vec();
        let mut r = BufReader::new(&raw[..]);
        assert_eq!(read_message(&mut r, Framing::ContentLength).await.unwrap(), Some(json!({"a":1})));
        assert_eq!(read_message(&mut r, Framing::ContentLength).await.unwrap(), None);
    }
}
