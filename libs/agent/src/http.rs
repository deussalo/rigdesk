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
//!   POST /viewer/mint, /viewer/<hand|human|stop|block|unblock|revoke> - only from `Api::hosted`, where no RigDesk viewer
//!        carries the human's buttons (a stock RustDesk PC driven from the Rig host), so the bearer's holder presses them
//!   POST /viewer/note {text, pngs: [base64]} - from `Api::hosted`: the human's message to the owning agent
//!   GET  /frame?display=N  - a PNG of the PC's screen now, for the bearer's holder: the human to clip and draw on, or the operator's
//!        screenshot on demand; journaled
//!   GET  /health           - liveness, this process's boot id and the current standing
//!   GET  /log?n=N          - the newest N entries of the action log (claims, holder changes, blocks, calls)
//!   GET  /servers          - the MCP servers found on the PC while an agent is connected, and whether MCP is on
//!   POST /servers/<name>/mcp - MCP to that server, forwarded to its loopback port; X-Desk-Session as for /mcp

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
use crate::found::Found;
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
    viewer_routes: bool,
    found: Option<Arc<Found>>,
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
    /// `found`, where given, offers the MCP servers running on the PC to the agent; a bridge to a stock PC has none.
    pub fn new(shared: Arc<Shared>, desk: Arc<dyn Desk>, journal: Arc<Journal>, bearer: &str, found: Option<Arc<Found>>) -> Arc<Self> {
        let mcp = Arc::new(Mcp { shared: shared.clone(), desk, journal: journal.clone() });
        let boot = format!("{:016x}", rand::random::<u64>());
        Arc::new(Api { shared, journal, mcp, bearer: Sha256::digest(bearer.as_bytes()).into(), boot, viewer_routes: false, found })
    }

    /// As `new`, plus the `/viewer/*` routes: the human's share and holder buttons, for a host with no RigDesk viewer.
    pub fn hosted(shared: Arc<Shared>, desk: Arc<dyn Desk>, journal: Arc<Journal>, bearer: &str) -> Arc<Self> {
        let mcp = Arc::new(Mcp { shared: shared.clone(), desk, journal: journal.clone() });
        let boot = format!("{:016x}", rand::random::<u64>());
        Arc::new(Api { shared, journal, mcp, bearer: Sha256::digest(bearer.as_bytes()).into(), boot, viewer_routes: true, found: None })
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
        if let Some(found) = &self.found {
            tokio::spawn(found.clone().watch(self.shared.clone()));
        }
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
            ("GET", "/health") => json_response(StatusCode::OK, &json!({"ok": true, "boot": self.boot, "standing": self.shared.standing(), "down": self.shared.down()})),
            ("POST", "/claim") => self.claim(request).await,
            ("POST", "/mcp") => self.mcp(request).await,
            ("POST", "/leave") => self.leave(&request),
            ("GET", "/log") => {
                let n = query_number(request.uri().query(), "n").unwrap_or(200).min(5000);
                json_response(StatusCode::OK, &json!({"entries": self.journal.tail(n)}))
            }
            ("GET", "/events") => self.events(request),
            ("GET", "/frame") => self.frame(request).await,
            ("GET", "/servers") => match &self.found {
                Some(found) => json_response(StatusCode::OK, &json!({"on": found.on(), "servers": found.servers()})),
                None => json_response(StatusCode::OK, &json!({"on": false, "servers": [], "why": "this PC's RigDesk does not offer MCP servers"})),
            },
            ("POST", p) if p.starts_with("/servers/") && p.ends_with("/mcp") => {
                let name = p.trim_start_matches("/servers/").trim_end_matches("/mcp").to_string();
                self.forward(request, &name).await
            }
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
            ("POST", "/viewer/mint") if self.viewer_routes => {
                let minted = self.shared.mint();
                json_response(StatusCode::OK, &json!({"grant": minted.grant, "secret": minted.secret}))
            }
            ("POST", "/viewer/note") if self.viewer_routes => self.note(request).await,
            ("POST", p) if p.starts_with("/viewer/") && self.viewer_routes => {
                let done = match p.trim_start_matches("/viewer/") {
                    "hand" => self.shared.hand_to_agent(),
                    "human" => self.shared.human_input("the operator"),
                    "stop" => self.shared.stop(),
                    "block" => self.shared.block(),
                    "unblock" => self.shared.unblock(),
                    "revoke" => self.shared.revoke("the operator revoked the share"),
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

    /// One MCP message to a server on the PC, under the holder and Block rules: a tool call is an action, so it waits
    /// for the person at the PC as typing does; the rest (initialize, listing) is a read.
    async fn forward(self: Arc<Self>, request: Request<Incoming>, name: &str) -> Response<Body> {
        let Some(found) = self.found.clone() else {
            return text(StatusCode::NOT_FOUND, "this PC's RigDesk does not offer MCP servers");
        };
        if !found.on() {
            return text(StatusCode::FORBIDDEN, "MCP is off on this PC: screen, input and shell are still on");
        }
        let Some(server) = found.get(name) else {
            let have: Vec<String> = found.servers().into_iter().map(|s| s.name).collect();
            return text(StatusCode::NOT_FOUND, &format!("no MCP server {name} on this PC now; found: {}", if have.is_empty() { "none".into() } else { have.join(", ") }));
        };
        let Some(token) = self.session_token(&request).map(str::to_string) else {
            return text(StatusCode::UNAUTHORIZED, &crate::session::Refusal::NoSession.words());
        };
        let passed: Vec<(String, String)> = ["mcp-session-id", "mcp-protocol-version", "last-event-id"]
            .iter()
            .filter_map(|h| request.headers().get(*h).and_then(|v| v.to_str().ok()).map(|v| (h.to_string(), v.to_string())))
            .collect();
        let Ok(body) = request.into_body().collect().await.map(|b| b.to_bytes()) else {
            return text(StatusCode::BAD_REQUEST, "the request body could not be read");
        };
        let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let method = message.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let tool = message.pointer("/params/name").and_then(Value::as_str).unwrap_or("").to_string();
        let what = if tool.is_empty() { format!("mcp {name} {method}") } else { format!("mcp {name}/{tool}") };
        let owner = match self.mcp.admit(&token, method != "tools/call", &what) {
            Ok(owner) => owner,
            Err(words) => {
                return match message.get("id") {
                    Some(id) if method == "tools/call" => json_response(StatusCode::OK, &json!({"jsonrpc": "2.0", "id": id, "result": crate::mcp::tool_error(&words)})),
                    _ => text(StatusCode::FORBIDDEN, &words),
                };
            }
        };
        let headers: Vec<(&str, &str)> = passed.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        let answered = tokio::time::timeout(crate::found::CALL_WAIT, crate::found::post(server.port, &server.path, &headers, &body)).await;
        let answer = match answered {
            Ok(Ok(answer)) => answer,
            Ok(Err(why)) => {
                self.journal.record(&Entry::new(owner.who(), &what, format!("error: {why}"), Value::Null));
                return text(StatusCode::BAD_GATEWAY, &format!("MCP server {name} on this PC did not answer: {why}"));
            }
            Err(_) => {
                self.journal.record(&Entry::new(owner.who(), &what, "timed out", Value::Null));
                return text(StatusCode::GATEWAY_TIMEOUT, &format!("MCP server {name} on this PC gave no answer within {} s", crate::found::CALL_WAIT.as_secs()));
            }
        };
        if method == "tools/call" {
            let failed = answer.status != 200 || answer.json_rpc().map_or(true, |r| r.get("error").is_some() || r.pointer("/result/isError") == Some(&Value::Bool(true)));
            self.journal.record(&Entry::new(owner.who(), &what, if failed { "failed" } else { "ok" }, json!({"args": message.pointer("/params/arguments")})));
            self.shared.events.transient(EventKind::Action { agent_title: owner.title.clone(), tool: format!("{name}/{tool}"), summary: what.clone(), ok: !failed, x: None, y: None });
        }
        let mut reply = Response::builder().status(answer.status);
        for keep in ["content-type", "mcp-session-id"] {
            if let Some(value) = answer.header(keep) {
                reply = reply.header(keep, value);
            }
        }
        reply.body(full(answer.body)).unwrap_or_else(|_| text(StatusCode::BAD_GATEWAY, "the server's answer could not be passed on"))
    }

    async fn note(self: Arc<Self>, request: Request<Incoming>) -> Response<Body> {
        use base64::Engine;
        let Some(body) = Self::body_json(request).await else {
            return text(StatusCode::BAD_REQUEST, "note wants a JSON body");
        };
        if self.shared.standing().owner.is_none() {
            return text(StatusCode::CONFLICT, "no agent has joined: share the PC with one first");
        }
        let text_said = body.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        let mut pngs = Vec::new();
        for png in body.get("pngs").and_then(Value::as_array).into_iter().flatten() {
            match png.as_str().map(|b| base64::engine::general_purpose::STANDARD.decode(b)) {
                Some(Ok(bytes)) => pngs.push(bytes),
                _ => return text(StatusCode::BAD_REQUEST, "pngs holds base64 PNG strings"),
            }
        }
        if text_said.trim().is_empty() && pngs.is_empty() {
            return text(StatusCode::BAD_REQUEST, "an empty note");
        }
        let id = self.shared.events.message(text_said, pngs);
        json_response(StatusCode::OK, &json!({"event": id}))
    }

    async fn frame(self: Arc<Self>, request: Request<Incoming>) -> Response<Body> {
        let display = query_number(request.uri().query(), "display");
        let args = display.map_or(json!({}), |d| json!({"display": d}));
        let desk = self.mcp.desk.clone();
        let shot = tokio::task::spawn_blocking(move || desk.call("screenshot", &args)).await;
        let png = match shot {
            Ok(Ok(content)) => content.contents.into_iter().find_map(|c| match c {
                crate::desk::Content::Png(png) => Some(png),
                _ => None,
            }),
            Ok(Err(why)) => {
                self.journal.record(&Entry::new("bearer", "frame", format!("failed: {why}"), Value::Null));
                return text(StatusCode::SERVICE_UNAVAILABLE, &why);
            }
            Err(_) => None,
        };
        self.journal.record(&Entry::new("bearer", "frame", if png.is_some() { "ok" } else { "no image" }, json!({"display": display})));
        match png {
            Some(png) => Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "image/png")
                .body(full(png))
                .unwrap_or_else(|_| Response::new(full(""))),
            None => text(StatusCode::SERVICE_UNAVAILABLE, "the screenshot gave no image"),
        }
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
                EventKind::Reach { down } => journal.record(&Entry::new("bridge", "reach", down.map_or("up".into(), |why| format!("down: {why}")), Value::Null)),
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
    use crate::desk::{Called, Content, ToolSpec};
    use crate::events::Events;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct NoDesk;
    impl Desk for NoDesk {
        fn tools(&self) -> Vec<ToolSpec> {
            vec![ToolSpec { name: "type", description: "types text", input_schema: json!({"type": "object"}), read_only: false }]
        }
        fn call(&self, _tool: &str, _args: &Value) -> Result<Called, String> {
            Ok(vec![Content::Text("typed".into())].into())
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
        let api = Api::new(shared.clone(), Arc::new(NoDesk), Arc::new(Journal::open(dir.join("a.jsonl"), 1 << 20)), "b", None);
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
        let api = Api::new(shared.clone(), Arc::new(NoDesk), Arc::new(Journal::memory()), "b", None);
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

    #[tokio::test(flavor = "multi_thread")]
    async fn the_viewer_stops_the_agent_and_sends_it_a_note() {
        let shared = Arc::new(Shared::new(Events::new(64)));
        let api = Api::hosted(shared.clone(), Arc::new(NoDesk), Arc::new(Journal::memory()), "b");
        let port = api.serve(0).await.unwrap();
        let minted = post(port, "/viewer/mint", "", &json!({})).await;
        let agent = json!({"id": "a1", "title": "Helper", "graph": "g"});
        post(port, "/claim", "", &json!({"grant": minted["grant"], "secret": minted["secret"], "agent": agent})).await;
        let mut events = shared.events.subscribe();
        assert_eq!(post(port, "/viewer/stop", "", &json!({})).await["standing"]["holder"], json!("blocked"));
        let stopped = events.recv().await.unwrap();
        assert!(matches!(stopped.kind, EventKind::Holder { ref by, .. } if by == "stop"), "{stopped:?}");
        post(port, "/viewer/stop", "", &json!({})).await;
        let again = events.recv().await.unwrap();
        assert!(matches!(again.kind, EventKind::Holder { ref by, .. } if by == "stop"), "a second Stop still reaches the agent: {again:?}");
        use base64::Engine;
        let png = base64::engine::general_purpose::STANDARD.encode([0x89u8, b'P', b'N', b'G']);
        post(port, "/viewer/note", "", &json!({"text": "look here", "pngs": [png]})).await;
        let note = events.recv().await.unwrap();
        assert!(matches!(note.kind, EventKind::Message { ref text, ref attachments } if text == "look here" && attachments.len() == 1), "{note:?}");
    }
}
