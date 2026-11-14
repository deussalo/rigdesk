//! One MCP session to a server on the PC, shared by every client that reaches it through RigDesk: the stdio command
//! behind `rustdesk --mcp`, or a server that speaks only the older HTTP+SSE transport. Neither can open a session per
//! client, so the server's first `initialize` answer is kept and given to every later client, and request ids are the
//! relay's own on the server's side, so two clients' ids never meet. What the server asks of a client (sampling, roots,
//! elicitation) has no one to go to: it is answered as a method not found, and its notifications are dropped.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::{oneshot, OnceCell};
use tokio::task::AbortHandle;

/// How the relay's messages reach the server; its messages back come to [`Relay::heard`].
pub enum Upstream {
    /// One JSON message per line on the command's stdin.
    Stdin(tokio::sync::Mutex<ChildStdin>),
    /// The older HTTP+SSE transport: each message is POSTed to the endpoint the server named on its event stream.
    Posted { port: u16, endpoint: String },
}

pub struct Relay {
    upstream: Upstream,
    waiting: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
    /// The server's answer to the first `initialize`, given to every later one.
    opened: OnceCell<Value>,
    initialized_sent: AtomicBool,
    gone: AtomicBool,
    /// The task that reads the server's messages, stopped when the relay ends.
    reader: Mutex<Option<AbortHandle>>,
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.upstream {
            Upstream::Stdin(_) => write!(f, "Relay(stdin)"),
            Upstream::Posted { port, endpoint } => write!(f, "Relay(sse 127.0.0.1:{port}{endpoint})"),
        }
    }
}

/// Removes a request's waiter if its caller stops waiting first (a forwarder's timeout), so none is left behind.
struct Unwait<'a>(&'a Relay, u64);

impl Drop for Unwait<'_> {
    fn drop(&mut self) {
        self.0.waiting.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.1);
    }
}

impl Relay {
    pub fn new(upstream: Upstream) -> Arc<Self> {
        Arc::new(Relay {
            upstream,
            waiting: Mutex::default(),
            next_id: AtomicU64::new(1),
            opened: OnceCell::new(),
            initialized_sent: AtomicBool::new(false),
            gone: AtomicBool::new(false),
            reader: Mutex::default(),
        })
    }

    pub fn set_reader(&self, reader: AbortHandle) {
        *self.reader.lock().unwrap_or_else(|p| p.into_inner()) = Some(reader);
    }

    /// The server is gone: every waiting request is answered so, and its reader stops.
    pub fn end(&self) {
        self.gone.store(true, Ordering::SeqCst);
        self.waiting.lock().unwrap_or_else(|p| p.into_inner()).clear();
        if let Some(reader) = self.reader.lock().unwrap_or_else(|p| p.into_inner()).take() {
            reader.abort();
        }
    }

    pub fn ended(&self) -> bool {
        self.gone.load(Ordering::SeqCst)
    }

    async fn send(&self, message: &Value) -> Result<(), String> {
        match &self.upstream {
            Upstream::Stdin(stdin) => {
                let mut line = message.to_string();
                line.push('\n');
                let mut stdin = stdin.lock().await;
                stdin.write_all(line.as_bytes()).await.map_err(|e| format!("the server's stdin is closed: {e}"))?;
                stdin.flush().await.map_err(|e| e.to_string())
            }
            Upstream::Posted { port, endpoint } => {
                let answer = crate::found::post(*port, endpoint, &[], message.to_string().as_bytes()).await?;
                if (200..300).contains(&answer.status) {
                    return Ok(());
                }
                let said = String::from_utf8_lossy(&answer.body);
                Err(format!("the server refused the message with {}: {}", answer.status, said.trim()))
            }
        }
    }

    /// One message from the server: an answer goes to whoever waits on it; its own requests are answered here.
    pub async fn heard(&self, message: Value) {
        let id = message.get("id").cloned();
        match (message.get("method").and_then(Value::as_str), id) {
            (None, Some(id)) => {
                let waiter = id.as_u64().and_then(|n| self.waiting.lock().unwrap_or_else(|p| p.into_inner()).remove(&n));
                if let Some(waiter) = waiter {
                    let _ = waiter.send(message);
                }
            }
            (Some("ping"), Some(id)) => {
                let _ = self.send(&json!({"jsonrpc": "2.0", "id": id, "result": {}})).await;
            }
            (Some(method), Some(id)) => {
                let words = format!("{method}: no client to ask; RigDesk carries only the client's requests to this server");
                let _ = self.send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": words}})).await;
            }
            (_, None) => {}
        }
    }

    /// One request through to the server, under an id of the relay's; its answer with the client's id put back.
    async fn ask(&self, mut request: Value) -> Value {
        let theirs = request.get("id").cloned().unwrap_or(Value::Null);
        if self.ended() {
            return fault(&theirs, "the server has ended");
        }
        let ours = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (send, answered) = oneshot::channel();
        self.waiting.lock().unwrap_or_else(|p| p.into_inner()).insert(ours, send);
        let _unwait = Unwait(self, ours);
        request["id"] = json!(ours);
        if let Err(e) = self.send(&request).await {
            return fault(&theirs, &e);
        }
        match answered.await {
            Ok(mut answer) => {
                answer["id"] = theirs;
                answer
            }
            Err(_) => fault(&theirs, "the server ended before it answered"),
        }
    }

    async fn initialize(&self, request: Value) -> Value {
        let theirs = request.get("id").cloned().unwrap_or(Value::Null);
        let opened = self
            .opened
            .get_or_try_init(|| async {
                let answer = self.ask(request).await;
                answer.get("result").cloned().filter(|_| answer.get("error").is_none()).ok_or(answer)
            })
            .await;
        match opened {
            Ok(result) => json!({"jsonrpc": "2.0", "id": theirs, "result": result}),
            Err(mut answer) => {
                answer["id"] = theirs;
                answer
            }
        }
    }

    /// One client message, a JSON-RPC object: a request is answered; a notification, or a client's answer to a request it
    /// never got, is passed on with nothing to answer. Fails only when a notification could not be passed on.
    pub async fn handle(&self, message: Value) -> Result<Option<Value>, String> {
        let method = message.get("method").and_then(Value::as_str).map(str::to_string);
        if message.get("id").is_none() || method.is_none() {
            // The server was initialized once; a later client's `initialized` would be a second.
            let initialized = method.as_deref() == Some("notifications/initialized");
            if !initialized || !self.initialized_sent.swap(true, Ordering::SeqCst) {
                self.send(&message).await?;
            }
            return Ok(None);
        }
        Ok(Some(if method.as_deref() == Some("initialize") { self.initialize(message).await } else { self.ask(message).await }))
    }
}

pub fn fault(id: &Value, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": message}})
}
