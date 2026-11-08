//! The agent API's HTTP surface, hyper 1 over http1. Bound to 127.0.0.1 and published on the tailnet by
//! `tailscale serve`, so only the tailnet reaches it. A per-PC bearer gates every route; on top of that, each
//! tool call carries the session token `/claim` returned. The bearer is compared in constant time.
//!
//! Routes:
//!   POST /mcp              - MCP JSON-RPC; X-Desk-Session carries the session token for tools/call
//!   POST /claim            - claim a grant for an agent, returns its session token (single use)
//!   GET  /events           - SSE, resumable with Last-Event-ID
//!   GET  /attachments/<id> - an image from a human message, owner session only
//!   POST /attachments/<id>/ack - mark messages delivered up to the message that image belongs to
//!   POST /leave            - the owning agent ends its session
//!   POST /debug/mint, /debug/viewer/<hand|human|block|unblock> - only with RIGDESK_AGENT_DEBUG_MINT set, for CI
//!   GET  /health           - liveness, this process's boot id and the current standing
//!   GET  /log?n=N          - the newest N entries of the action log (claims, holder changes, blocks, calls)

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::desk::Desk;
use crate::events::{Event, EventKind};
use crate::journal::{Entry, Journal};
use crate::mcp::Mcp;
use crate::session::{Owner, Shared};

type Body = http_body_util::combinators::BoxBody<Bytes, Infallible>;

pub struct Api {
    shared: Arc<Shared>,
    journal: Arc<Journal>,
    mcp: Arc<Mcp>,
    bearer: [u8; 32],
    /// Fresh each start: event ids and sessions do not outlive the process, so a client that sees a new boot starts over.
    boot: String,
}

fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into()).boxed()
}

fn json_response(status: StatusCode, body: &Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full(body.to_string()))
        .unwrap_or_else(|_| Response::new(full("{}")))
}

fn text(status: StatusCode, body: &str) -> Response<Body> {
    Response::builder().status(status).body(full(body.to_string())).unwrap_or_else(|_| Response::new(full("")))
}

impl Api {
    pub fn new(shared: Arc<Shared>, desk: Arc<dyn Desk>, journal: Arc<Journal>, bearer: &str) -> Arc<Self> {
        let mcp = Arc::new(Mcp { shared: shared.clone(), desk, journal: journal.clone() });
        let boot = format!("{:016x}", rand::random::<u64>());
        Arc::new(Api { shared, journal, mcp, bearer: Sha256::digest(bearer.as_bytes()).into(), boot })
    }

    fn bearer_ok(&self, request: &Request<Incoming>) -> bool {
        let given = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        let digest: [u8; 32] = Sha256::digest(given.as_bytes()).into();
        bool::from(digest.ct_eq(&self.bearer))
    }

    /// Serve until the process ends. Binds 127.0.0.1:port; port 0 asks the OS for a free one, returned here.
    pub async fn serve(self: Arc<Self>, port: u16) -> std::io::Result<u16> {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
        let bound = listener.local_addr()?.port();
        tokio::spawn(journal_holder_changes(self.shared.events.subscribe(), self.journal.clone()));
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { continue };
                let api = self.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let api = api.clone();
                        async move { Ok::<_, Infallible>(api.route(req).await) }
                    });
                    let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
                });
            }
        });
        Ok(bound)
    }

    async fn route(self: Arc<Self>, request: Request<Incoming>) -> Response<Body> {
        let path = request.uri().path().to_string();
        if !self.bearer_ok(&request) {
            return text(StatusCode::UNAUTHORIZED, "bad bearer token");
        }
        let method = request.method().clone();
        match (method.as_str(), path.as_str()) {
            ("GET", "/health") => json_response(StatusCode::OK, &json!({"ok": true, "boot": self.boot, "standing": self.shared.standing()})),
            ("POST", "/claim") => self.claim(request).await,
            ("POST", "/mcp") => self.mcp(request).await,
            ("POST", "/leave") => self.leave(&request),
            ("GET", "/log") => {
                let n = query_number(request.uri().query(), "n").unwrap_or(200).min(5000);
                json_response(StatusCode::OK, &json!({"entries": self.journal.tail(n)}))
            }
            ("GET", "/events") => self.events(request),
            ("POST", "/debug/mint") if std::env::var("RIGDESK_AGENT_DEBUG_MINT").is_ok() => {
                let minted = self.shared.mint();
                json_response(StatusCode::OK, &json!({"grant": minted.grant, "secret": minted.secret}))
            }
            ("POST", p) if p.starts_with("/debug/viewer/") && std::env::var("RIGDESK_AGENT_DEBUG_MINT").is_ok() => {
                // The viewer's buttons, for the CI smoke test only: the same calls the viewer makes over the peer protocol.
                let done = match p.trim_start_matches("/debug/viewer/") {
                    "hand" => self.shared.hand_to_agent(),
                    "human" => self.shared.human_input("debug"),
                    "block" => self.shared.block(),
                    "unblock" => self.shared.unblock(),
                    _ => return text(StatusCode::NOT_FOUND, "no such viewer verb"),
                };
                json_response(StatusCode::OK, &json!({"changed": done, "standing": self.shared.standing()}))
            }
            ("GET", p) if p.starts_with("/attachments/") => self.attachment(&request, p),
            ("POST", p) if p.starts_with("/attachments/") && p.ends_with("/ack") => self.ack(&request, p),
            _ => text(StatusCode::NOT_FOUND, "no such route"),
        }
    }

    fn session_token<'a>(&self, request: &'a Request<Incoming>) -> Option<&'a str> {
        request.headers().get("x-desk-session").and_then(|v| v.to_str().ok())
    }

    async fn body_json(request: Request<Incoming>) -> Option<Value> {
        let bytes = request.into_body().collect().await.ok()?.to_bytes();
        serde_json::from_slice(&bytes).ok()
    }

    async fn claim(self: Arc<Self>, request: Request<Incoming>) -> Response<Body> {
        let Some(body) = Self::body_json(request).await else {
            return text(StatusCode::BAD_REQUEST, "claim wants a JSON body");
        };
        let grant = body.get("grant").and_then(Value::as_str).unwrap_or("");
        let secret = body.get("secret").and_then(Value::as_str).unwrap_or("");
        let agent = body.get("agent").cloned().unwrap_or(Value::Null);
        let owner = Owner {
            agent_id: agent.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            title: agent.get("title").and_then(Value::as_str).unwrap_or("agent").to_string(),
            graph: agent.get("graph").and_then(Value::as_str).unwrap_or("").to_string(),
        };
        if owner.agent_id.is_empty() {
            return text(StatusCode::BAD_REQUEST, "claim wants agent.id");
        }
        match self.shared.claim(grant, secret, owner.clone()) {
            Ok(token) => {
                self.journal.record(&Entry::new(owner.who(), "claim", "ok", json!({"graph": owner.graph})));
                json_response(StatusCode::OK, &json!({"session": token, "standing": self.shared.standing()}))
            }
            Err(refusal) => {
                self.journal.record(&Entry::new(owner.who(), "claim", "refused", json!({"reason": refusal.words()})));
                json_response(StatusCode::FORBIDDEN, &json!({"error": refusal.words()}))
            }
        }
    }

    async fn mcp(self: Arc<Self>, request: Request<Incoming>) -> Response<Body> {
        let token = self.session_token(&request).map(str::to_owned);
        let Some(body) = Self::body_json(request).await else {
            return json_response(StatusCode::BAD_REQUEST, &json!({"jsonrpc": "2.0", "error": {"code": -32700, "message": "parse error"}}));
        };
        // A tool call runs synchronous host work (capture, UIA, a shell), so it must not run on the reactor.
        let mcp = self.mcp.clone();
        let reply = tokio::task::spawn_blocking(move || mcp.handle(&body, token.as_deref())).await.unwrap_or_else(|e| {
            Some(serde_json::json!({"jsonrpc": "2.0", "error": {"code": -32603, "message": format!("handler panicked: {e}")}}))
        });
        match reply {
            Some(reply) => json_response(StatusCode::OK, &reply),
            None => Response::builder().status(StatusCode::ACCEPTED).body(full("")).unwrap_or_else(|_| Response::new(full(""))),
        }
    }

    fn events(self: Arc<Self>, request: Request<Incoming>) -> Response<Body> {
        let after = request
            .headers()
            .get("last-event-id")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .or_else(|| {
                request.uri().query().and_then(|q| {
                    q.split('&').find_map(|kv| kv.strip_prefix("last-event-id=").and_then(|v| v.parse().ok()))
                })
            })
            .unwrap_or(0);
        let (backlog, gap, mut rx) = self.shared.events.resume(after);
        let (tx, body_rx) = mpsc::channel::<Result<hyper::body::Frame<Bytes>, Infallible>>(64);
        tokio::spawn(async move {
            if gap {
                let _ = tx.send(Ok(sse(&Event { id: 0, at_ms: crate::now_ms(), kind: EventKind::Dropped { messages: 0 } }))).await;
            }
            for event in backlog {
                if tx.send(Ok(sse(&event))).await.is_err() {
                    return;
                }
            }
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if tx.send(Ok(sse(&event))).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
        });
        let stream = tokio_stream_from(body_rx);
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(stream)
            .unwrap_or_else(|_| Response::new(full("")))
    }

    fn owner_from(&self, request: &Request<Incoming>) -> Option<Owner> {
        let token = self.session_token(request)?;
        self.shared.owner_of(token).ok()
    }

    fn attachment(&self, request: &Request<Incoming>, path: &str) -> Response<Body> {
        if self.owner_from(request).is_none() {
            return text(StatusCode::FORBIDDEN, "attachments need the owner's session token");
        }
        let id = path.trim_start_matches("/attachments/");
        match self.shared.events.attachment(id) {
            Some(png) => Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "image/png")
                .body(full(png))
                .unwrap_or_else(|_| Response::new(full(""))),
            None => text(StatusCode::NOT_FOUND, "no such attachment, or it was already acked"),
        }
    }

    fn leave(&self, request: &Request<Incoming>) -> Response<Body> {
        let Some(owner) = self.owner_from(request) else {
            return text(StatusCode::FORBIDDEN, "leave needs the owner's session token");
        };
        let left = self.shared.revoke("the agent left");
        self.journal.record(&Entry::new(owner.who(), "leave", if left { "ok" } else { "no session" }, json!({})));
        json_response(StatusCode::OK, &json!({"left": left}))
    }

    fn ack(&self, request: &Request<Incoming>, _path: &str) -> Response<Body> {
        if self.owner_from(request).is_none() {
            return text(StatusCode::FORBIDDEN, "ack needs the owner's session token");
        }
        let upto = request
            .uri()
            .query()
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("upto=").and_then(|v| v.parse::<u64>().ok())))
            .unwrap_or_else(|| self.shared.events.last_id());
        let cleared = self.shared.events.ack(upto);
        json_response(StatusCode::OK, &json!({"cleared": cleared}))
    }
}

fn sse(event: &Event) -> hyper::body::Frame<Bytes> {
    let data = serde_json::to_string(event).unwrap_or_default();
    hyper::body::Frame::data(Bytes::from(format!("id: {}\ndata: {}\n\n", event.id, data)))
}

fn tokio_stream_from(rx: mpsc::Receiver<Result<hyper::body::Frame<Bytes>, Infallible>>) -> Body {
    use http_body_util::StreamBody;
    use tokio_stream::wrappers::ReceiverStream;
    StreamBody::new(ReceiverStream::new(rx)).boxed()
}

fn query_number(query: Option<&str>, key: &str) -> Option<usize> {
    query?.split('&').find_map(|pair| pair.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
}

/// Every holder change and revoke goes to the action log, whoever caused it: the viewer, the PC's own input or the API.
async fn journal_holder_changes(mut rx: tokio::sync::broadcast::Receiver<Event>, journal: Arc<Journal>) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(event) => match event.kind {
                EventKind::Holder { holder, by } => {
                    let holder = serde_json::to_value(holder).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
                    journal.record(&Entry::new(by, format!("holder {holder}"), "ok", Value::Null));
                }
                EventKind::Revoked { agent_id, reason } => journal.record(&Entry::new(reason, "revoke", "ok", json!({"agent_id": agent_id}))),
                _ => {}
            },
            Err(RecvError::Lagged(missed)) => journal.record(&Entry::new("rigdesk", "journal", format!("missed {missed} events"), Value::Null)),
            Err(RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desk::{Content, ToolSpec};
    use crate::events::Events;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct NoDesk;
    impl Desk for NoDesk {
        fn tools(&self) -> Vec<ToolSpec> {
            vec![ToolSpec { name: "type", description: "types text", input_schema: json!({"type": "object"}), read_only: false }]
        }
        fn call(&self, _tool: &str, _args: &Value) -> Result<Vec<Content>, String> {
            Ok(vec![Content::Text("typed".into())])
        }
    }

    async fn post(port: u16, path: &str, session: &str, body: &Value) -> Value {
        let body = body.to_string();
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let request = format!(
            "POST {path} HTTP/1.1\r\nhost: x\r\nauthorization: Bearer b\r\nx-desk-session: {session}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await.unwrap();
        let (_, json) = answer.split_once("\r\n\r\n").unwrap();
        serde_json::from_str(json).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_block_from_the_viewer_is_readable_in_the_log() {
        let dir = std::env::temp_dir().join(format!("rigdesk-http-{}", rand::random::<u32>()));
        let shared = Arc::new(Shared::new(Events::new(64)));
        let api = Api::new(shared.clone(), Arc::new(NoDesk), Arc::new(Journal::open(dir.join("a.jsonl"), 1 << 20)), "b");
        let port = api.serve(0).await.unwrap();
        let minted = shared.mint();
        let agent = json!({"id": "a1", "title": "Helper", "graph": "g"});
        post(port, "/claim", "", &json!({"grant": minted.grant, "secret": minted.secret, "agent": agent})).await;
        assert!(shared.block());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(b"GET /log?n=10 HTTP/1.1\r\nhost: x\r\nauthorization: Bearer b\r\nconnection: close\r\n\r\n").await.unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await.unwrap();
        let (_, body) = answer.split_once("\r\n\r\n").unwrap();
        let log: Value = serde_json::from_str(body).unwrap();
        let whats: Vec<&str> = log["entries"].as_array().unwrap().iter().map(|e| e["what"].as_str().unwrap()).collect();
        assert!(whats.contains(&"claim"), "{whats:?}");
        assert!(whats.contains(&"holder blocked"), "{whats:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn after_leave_the_session_token_is_refused() {
        let shared = Arc::new(Shared::new(Events::new(64)));
        let api = Api::new(shared.clone(), Arc::new(NoDesk), Arc::new(Journal::memory()), "b");
        let port = api.serve(0).await.unwrap();
        let minted = shared.mint();
        let agent = json!({"id": "a1", "title": "Helper", "graph": "g"});
        let claimed = post(port, "/claim", "", &json!({"grant": minted.grant, "secret": minted.secret, "agent": agent})).await;
        let token = claimed["session"].as_str().unwrap().to_string();
        assert_eq!(post(port, "/leave", &token, &json!({})).await["left"], json!(true));
        let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "type", "arguments": {}}});
        let refused = post(port, "/mcp", &token, &call).await;
        assert_eq!(refused["result"]["isError"], json!(true));
        assert!(shared.standing().owner.is_none());
    }
}
