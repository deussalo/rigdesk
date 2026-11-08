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
//!   GET  /health           - liveness and the current standing

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
        Arc::new(Api { shared, journal, mcp, bearer: Sha256::digest(bearer.as_bytes()).into() })
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
        if path == "/health" {
            return json_response(StatusCode::OK, &json!({"ok": true, "standing": self.shared.standing()}));
        }
        if !self.bearer_ok(&request) {
            return text(StatusCode::UNAUTHORIZED, "bad bearer token");
        }
        let method = request.method().clone();
        match (method.as_str(), path.as_str()) {
            ("POST", "/claim") => self.claim(request).await,
            ("POST", "/mcp") => self.mcp(request).await,
            ("GET", "/events") => self.events(request),
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
        match self.mcp.handle(&body, token.as_deref()) {
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
