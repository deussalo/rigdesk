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
//!   POST /viewer/note {text, pngs: [base64], key?} - from `Api::hosted`: the human's message to the owning agent; a note
//!        sent again with a key already sent to the same owning agent is answered with the first one's event, not told twice
//!   GET  /frame?display=N  - a PNG of the PC's screen now, for the bearer's holder: the human to clip and draw on, or the operator's
//!        screenshot on demand; journaled
//!   GET  /health           - liveness, this process's boot id, the current standing, whether the agent shell is open, and whether an invite is
//!        open
//!   GET  /log?n=N          - the newest N entries of the action log (claims, holder changes, blocks, calls)
//!   GET  /servers          - the MCP servers found on the PC while an agent is connected, and whether MCP is on
//!   POST /servers/<name>/mcp - MCP to that server, forwarded to its loopback port (on a bridge, to the address it was
//!        told of); X-Desk-Session as for /mcp
//!   POST /shell {agent, command, timeout_s, as, background} or {agent, job} - the management lane (see `manage`): no
//!        session, admitted by the PC's agent-shell switch and the human's Block on that agent; only where the host offers it
//!   GET  /samples?after=ID - the PC's health samples after ID (see `health`); only where the host offers them
//!   POST /join {agent, ask} - a session the operator accepted in Rig, with no share link (see `operator`): admitted by the
//!        PC's own switch, refused while another agent holds the session or for a blocked agent; only where the host offers it.
//!        `answered_by: "person"` journals that the person at the PC allowed it on the PC's prompt
//!   POST /offer {agent, ask, nonce} - while the operator's ask stands, the person at the PC is asked too, in the host's own
//!        accept prompt; their answer is an `offer` event with that ask and nonce. POST /offer/end {ask} takes the prompt away

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
use crate::health::Collector;
use crate::journal::{Entry, Journal};
use crate::manage::Lane;
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
    /// Serves the management lane alone: /health, /log, /events and /shell. A Linux PC's root service is such an API,
    /// beside the desktop user's, which serves the rest.
    lane_only: bool,
    found: Option<Arc<Found>>,
    lane: std::sync::OnceLock<Arc<Lane>>,
    collector: std::sync::OnceLock<Arc<Collector>>,
    operator: std::sync::OnceLock<Arc<dyn crate::operator::Switch>>,
    noted: std::sync::Mutex<Noted>,
    /// The PC's own host name, which an invite's link names it by; /health reports it so Rig can match it.
    host: std::sync::OnceLock<String>,
    /// `/claim`s with a wrong bearer: when one was last journaled, and how many came since.
    refused_claims: std::sync::Mutex<(u64, u64)>,
}

/// A wrong-bearer `/claim` is journaled at most this often, with a count of the rest.
const REFUSED_CLAIM_EVERY_MS: u64 = 60_000;

/// The note keys sent to the owning agent, newest last; forgotten when another agent owns the session.
#[derive(Default)]
struct Noted {
    agent: String,
    keys: std::collections::VecDeque<(String, u64)>,
}

/// How many note keys are remembered: far more than a device sends again after one reconnect.
const NOTE_KEYS: usize = 256;

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

/// Whether a message to a PC's MCP server only reads, so it runs while the human uses the PC: opening, listing, pings,
/// notifications and the client's answers. Anything else (a tool call, a resource read, a prompt) acts as the agent.
fn mcp_reads(method: &str, message: &Value) -> bool {
    let answer = message.get("method").is_none();
    answer || matches!(method, "initialize" | "ping") || method.starts_with("notifications/") || method.ends_with("/list")
}

/// Reads and drops a body the route has no use for, up to 64 KiB. Left unread, hyper closes the connection after the
/// answer, and a client that sent `Expect: 100-continue` (Windows PowerShell's Invoke-RestMethod) sends its body, or its
/// next request, into a connection it was told to keep.
async fn drain(request: &mut Request<Incoming>) {
    let mut left = 64usize << 10;
    while let Some(Ok(frame)) = request.body_mut().frame().await {
        match frame.data_ref().map(|data| data.len()) {
            Some(n) if n >= left => break,
            Some(n) => left -= n,
            None => {}
        }
    }
}

/// The desktop a lane-only API has: none, since it never serves /mcp.
struct NoDesk;

impl Desk for NoDesk {
    fn tools(&self) -> Vec<crate::desk::ToolSpec> {
        Vec::new()
    }

    fn call(&self, tool: &str, _args: &Value) -> Result<crate::desk::Called, String> {
        Err(format!("the management lane has no desktop, so no {tool}"))
    }
}

fn text(status: StatusCode, body: &str) -> Response<Body> {
    Response::builder().status(status).body(full(body.to_string())).unwrap_or_else(|_| Response::new(full("")))
}

impl Api {
    /// `found`, where given, offers the MCP servers running on the PC to the agent; a bridge to a stock PC has none.
    pub fn new(shared: Arc<Shared>, desk: Arc<dyn Desk>, journal: Arc<Journal>, bearer: &str, found: Option<Arc<Found>>) -> Arc<Self> {
        let mcp = Arc::new(Mcp { shared: shared.clone(), desk, journal: journal.clone() });
        let boot = format!("{:016x}", rand::random::<u64>());
        Arc::new(Api { shared, journal, mcp, bearer: Sha256::digest(bearer.as_bytes()).into(), boot, viewer_routes: false, lane_only: false, found, lane: Default::default(), collector: Default::default(), operator: Default::default(), noted: Default::default(), host: Default::default(), refused_claims: Default::default() })
    }

    /// As `new`, plus the `/viewer/*` routes: the human's share and holder buttons, for a host with no RigDesk viewer.
    /// `found` offers the MCP servers the host was told of on the PC (see [`Found::remote`]).
    pub fn hosted(shared: Arc<Shared>, desk: Arc<dyn Desk>, journal: Arc<Journal>, bearer: &str, found: Option<Arc<Found>>) -> Arc<Self> {
        let mcp = Arc::new(Mcp { shared: shared.clone(), desk, journal: journal.clone() });
        let boot = format!("{:016x}", rand::random::<u64>());
        Arc::new(Api { shared, journal, mcp, bearer: Sha256::digest(bearer.as_bytes()).into(), boot, viewer_routes: true, lane_only: false, found, lane: Default::default(), collector: Default::default(), operator: Default::default(), noted: Default::default(), host: Default::default(), refused_claims: Default::default() })
    }

    /// An API that serves the management lane alone, with its own bearer and journal: a Linux PC's root service, whose
    /// lane the desktop user's processes must not reach. With a collector, it takes the PC's health samples too.
    pub fn lane_only(lane: Arc<Lane>, collector: Option<Arc<Collector>>, shared: Arc<Shared>, journal: Arc<Journal>, bearer: &str) -> Arc<Self> {
        let mcp = Arc::new(Mcp { shared: shared.clone(), desk: Arc::new(NoDesk), journal: journal.clone() });
        let boot = format!("{:016x}", rand::random::<u64>());
        let api = Api { shared, journal, mcp, bearer: Sha256::digest(bearer.as_bytes()).into(), boot, viewer_routes: false, lane_only: true, found: None, lane: Default::default(), collector: Default::default(), operator: Default::default(), noted: Default::default(), host: Default::default(), refused_claims: Default::default() };
        api.manage(lane, collector);
        Arc::new(api)
    }

    /// Offers the management lane at /shell and, with a collector, health samples at /samples. Before `serve`, once.
    pub fn manage(&self, lane: Arc<Lane>, collector: Option<Arc<Collector>>) {
        let _ = self.lane.set(lane);
        if let Some(collector) = collector {
            let _ = self.collector.set(collector);
        }
    }

    /// Offers `/join`, sessions the operator starts from Rig, admitted by `switch`. Before `serve`, once.
    pub fn operator_joins(&self, switch: Arc<dyn crate::operator::Switch>) {
        let _ = self.operator.set(switch);
    }

    /// Names the PC's host in /health. Before `serve`, once.
    pub fn names_host(&self, host: String) {
        let _ = self.host.set(host);
    }

    /// Someone without Rig's bearer tried to claim a link: nothing in the request is trusted, so only the count is kept.
    fn claim_refused(&self) {
        let mut refused = self.refused_claims.lock().unwrap_or_else(|p| p.into_inner());
        if refused.0 != 0 && crate::now_ms().saturating_sub(refused.0) < REFUSED_CLAIM_EVERY_MS {
            refused.1 += 1;
            return;
        }
        self.journal.record(&Entry::new("a caller without Rig's key", "claim", "refused: bad bearer token", json!({"also_refused_since_last": refused.1})));
        *refused = (crate::now_ms(), 0);
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
        if let Some(lane) = self.lane.get() {
            tokio::spawn(crate::manage::sweep_forever(lane.clone()));
        }
        if let Some(collector) = self.collector.get() {
            tokio::spawn(collector.clone().run_forever());
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

    /// Serve until the process ends on a unix socket at `path`, made anew and connectable by its owner alone: on a Linux
    /// PC, root's, so only tailscaled (root) reaches it, not the desktop user's processes. A lane-only API: no MCP
    /// servers, so only the journal of holder changes, the lane's sweep and any collector run beside it.
    #[cfg(unix)]
    pub async fn serve_unix(self: Arc<Self>, path: &std::path::Path) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = tokio::net::UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        tokio::spawn(journal_holder_changes(self.shared.events.subscribe(), self.journal.clone()));
        if let Some(lane) = self.lane.get() {
            tokio::spawn(crate::manage::sweep_forever(lane.clone()));
        }
        if let Some(collector) = self.collector.get() {
            tokio::spawn(collector.clone().run_forever());
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
        Ok(())
    }

    async fn route(self: Arc<Self>, mut request: Request<Incoming>) -> Response<Body> {
        let path = request.uri().path().to_string();
        if !self.bearer_ok(&request) {
            if path == "/claim" {
                self.claim_refused();
            }
            return text(StatusCode::UNAUTHORIZED, "bad bearer token");
        }
        let method = request.method().clone();
        if self.lane_only && !matches!((method.as_str(), path.as_str()), ("GET", "/health" | "/log" | "/events" | "/samples") | ("POST", "/shell")) {
            return text(StatusCode::NOT_FOUND, "this is the PC's management lane: /health, /log, /events, /samples and /shell; the desktop's routes are on its other API");
        }
        match (method.as_str(), path.as_str()) {
            ("GET", "/health") if self.lane_only => json_response(StatusCode::OK, &json!({"ok": true, "boot": self.boot, "os": std::env::consts::OS, "lane": true,
                                                                  "shell": self.lane.get().map(|lane| lane.switch())})),
            ("GET", "/health") => json_response(StatusCode::OK, &json!({"ok": true, "boot": self.boot, "os": std::env::consts::OS, "host": self.host.get(), "standing": self.shared.standing(), "down": self.shared.down(),
                                                                  "shell": self.lane.get().map(|lane| lane.switch()),
                                                                  "join": self.operator.get().map(|switch| crate::operator::said(switch.as_ref())),
                                                                  "invite_open": self.shared.invite_until().is_some()})),
            ("POST", "/claim") => self.claim(request).await,
            ("POST", "/mcp") => self.mcp(request).await,
            ("POST", "/leave") => {
                drain(&mut request).await;
                self.leave(&request)
            }
            ("GET", "/blocked") if !self.lane_only => {
                let agent = request.uri().query().unwrap_or("").split('&').find_map(|kv| kv.strip_prefix("agent=")).unwrap_or("").to_string();
                json_response(StatusCode::OK, &json!({"agent": agent, "since_ms": self.shared.blocked_since(&agent)}))
            }
            ("GET", "/log") => {
                let n = query_number(request.uri().query(), "n").unwrap_or(200).min(5000);
                json_response(StatusCode::OK, &json!({"entries": self.journal.tail(n)}))
            }
            ("GET", "/events") => self.events(request),
            ("POST", "/shell") => self.shell(request).await,
            ("POST", "/join") => self.join(request).await,
            ("POST", "/offer") => self.offer(request).await,
            ("POST", "/offer/end") => self.offer_end(request).await,
            ("GET", "/samples") => match self.collector.get() {
                Some(collector) => {
                    let after = query_number(request.uri().query(), "after").unwrap_or(0) as u64;
                    json_response(StatusCode::OK, &json!({"samples": collector.samples.after(after)}))
                }
                None => text(StatusCode::NOT_FOUND, "this PC's RigDesk takes no health samples"),
            },
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
                drain(&mut request).await;
                let minted = self.shared.mint();
                json_response(StatusCode::OK, &json!({"grant": minted.grant, "secret": minted.secret}))
            }
            ("POST", p) if p.starts_with("/debug/viewer/") && std::env::var("RIGDESK_AGENT_DEBUG_MINT").is_ok() => {
                drain(&mut request).await;
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
                drain(&mut request).await;
                let minted = self.shared.mint();
                json_response(StatusCode::OK, &json!({"grant": minted.grant, "secret": minted.secret}))
            }
            ("POST", "/viewer/note") if self.viewer_routes => self.note(request).await,
            ("POST", p) if p.starts_with("/viewer/") && self.viewer_routes => {
                drain(&mut request).await;
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
            ("POST", p) if p.starts_with("/attachments/") && p.ends_with("/ack") => {
                drain(&mut request).await;
                self.ack(&request, p)
            }
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
        if !message.is_object() {
            return text(StatusCode::BAD_REQUEST, "send one JSON-RPC message, not a batch");
        }
        let method = message.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let tool = message.pointer("/params/name").and_then(Value::as_str).unwrap_or("").to_string();
        let what = if tool.is_empty() { format!("mcp {name} {method}") } else { format!("mcp {name}/{tool}") };
        let owner = match self.mcp.admit(&token, mcp_reads(&method, &message), &what) {
            Ok(owner) => owner,
            Err(words) => {
                return match message.get("id") {
                    Some(id) if method == "tools/call" => json_response(StatusCode::OK, &json!({"jsonrpc": "2.0", "id": id, "result": crate::mcp::tool_error(&words)})),
                    _ => text(StatusCode::FORBIDDEN, &words),
                };
            }
        };
        let headers: Vec<(&str, &str)> = passed.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        let answered = tokio::time::timeout(crate::found::CALL_WAIT, server.call(&headers, &body)).await;
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
        if !mcp_reads(&method, &message) {
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
        let Some(owner) = self.shared.standing().owner else {
            return text(StatusCode::CONFLICT, "no agent has joined: share the PC with one first");
        };
        let key = match body.get("key") {
            None | Some(Value::Null) => None,
            Some(Value::String(key)) if !key.is_empty() && key.len() <= 200 => Some(key.clone()),
            Some(_) => return text(StatusCode::BAD_REQUEST, "key is a string of 1 to 200 characters"),
        };
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
        let Some(key) = key else {
            let id = self.shared.events.message(text_said, pngs);
            return json_response(StatusCode::OK, &json!({"event": id}));
        };
        let mut noted = self.noted.lock().unwrap_or_else(|p| p.into_inner());
        if noted.agent != owner.agent_id {
            *noted = Noted { agent: owner.agent_id.clone(), keys: Default::default() };
        }
        if let Some((_, id)) = noted.keys.iter().find(|(k, _)| *k == key) {
            return json_response(StatusCode::OK, &json!({"event": id, "replayed": true}));
        }
        let id = self.shared.events.message(text_said, pngs);
        noted.keys.push_back((key, id));
        if noted.keys.len() > NOTE_KEYS {
            noted.keys.pop_front();
        }
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
        // The one open grant is an invite, so this claim is for it or for nothing: the PC's switch for Rig's sessions holds it too.
        let invite = self.shared.invite_until().is_some();
        if let Some(Err(closed)) = self.operator.get().filter(|_| invite).map(|switch| switch.open()) {
            let words = crate::operator::Refused::Closed(closed).words();
            self.journal.record(&Entry::new(owner.who(), "claim", "refused", json!({"reason": words, "invited": true})));
            return json_response(StatusCode::FORBIDDEN, &json!({"error": words}));
        }
        match self.shared.claim(grant, secret, owner.clone()) {
            Ok(token) => {
                self.journal.record(&Entry::new(owner.who(), "claim", "ok", json!({"graph": owner.graph, "invited": invite})));
                json_response(StatusCode::OK, &json!({"session": token, "standing": self.shared.standing()}))
            }
            Err(refusal) => {
                self.journal.record(&Entry::new(owner.who(), "claim", "refused", json!({"reason": refusal.words()})));
                json_response(StatusCode::FORBIDDEN, &json!({"error": refusal.words()}))
            }
        }
    }

    async fn join(self: Arc<Self>, request: Request<Incoming>) -> Response<Body> {
        let Some(switch) = self.operator.get().cloned() else {
            return json_response(StatusCode::NOT_FOUND, &json!({"error": "this PC's RigDesk takes no sessions started from Rig", "code": "no_join"}));
        };
        let Some(body) = Self::body_json(request).await else {
            return json_response(StatusCode::BAD_REQUEST, &json!({"error": "join wants a JSON body", "code": "bad_request"}));
        };
        let (owner, ask) = match crate::operator::request(&body) {
            Ok(read) => read,
            Err(why) => return json_response(StatusCode::BAD_REQUEST, &json!({"error": why, "code": "bad_request"})),
        };
        let joined = switch.open().map_err(crate::operator::Refused::Closed).and_then(|()| self.shared.join_operator(owner.clone()));
        let by = if body.get("answered_by").and_then(Value::as_str) == Some("person") { "Rig, allowed by the person at the PC" } else { "Rig, operator-approved" };
        let detail = json!({"graph": owner.graph, "ask": ask, "by": by});
        match joined {
            Ok(token) => {
                self.journal.record(&Entry::new(owner.who(), "join", "ok", detail));
                json_response(StatusCode::OK, &json!({"session": token, "standing": self.shared.standing()}))
            }
            Err(refused) => {
                self.journal.record(&Entry::new(owner.who(), "join", format!("refused: {}", refused.code()), detail));
                json_response(StatusCode::FORBIDDEN, &json!({"error": refused.words(), "code": refused.code()}))
            }
        }
    }

    async fn offer(self: Arc<Self>, request: Request<Incoming>) -> Response<Body> {
        let Some(switch) = self.operator.get().cloned() else {
            return json_response(StatusCode::NOT_FOUND, &json!({"error": "this PC's RigDesk takes no sessions started from Rig", "code": "no_offer"}));
        };
        let Some(body) = Self::body_json(request).await else {
            return json_response(StatusCode::BAD_REQUEST, &json!({"error": "offer wants a JSON body", "code": "bad_request"}));
        };
        let offer = match crate::operator::offer_request(&body) {
            Ok(offer) => offer,
            Err(why) => return json_response(StatusCode::BAD_REQUEST, &json!({"error": why, "code": "bad_request"})),
        };
        if let Err(closed) = switch.open() {
            let refused = crate::operator::Refused::Closed(closed);
            return json_response(StatusCode::FORBIDDEN, &json!({"error": refused.words(), "code": refused.code()}));
        }
        let (who, ask) = (offer.owner.who(), offer.ask.clone());
        match switch.offer(offer) {
            Ok(()) => {
                self.journal.record(&Entry::new(who, "offer", "shown", json!({"ask": ask})));
                json_response(StatusCode::OK, &json!({"shown": true}))
            }
            Err(why) => json_response(StatusCode::CONFLICT, &json!({"error": why, "code": "no_prompt"})),
        }
    }

    async fn offer_end(self: Arc<Self>, request: Request<Incoming>) -> Response<Body> {
        let Some(switch) = self.operator.get().cloned() else {
            return json_response(StatusCode::NOT_FOUND, &json!({"error": "this PC's RigDesk takes no sessions started from Rig", "code": "no_offer"}));
        };
        let ask = Self::body_json(request).await.and_then(|body| body.get("ask").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
        json_response(StatusCode::OK, &json!({"ended": switch.offer_end(&ask)}))
    }

    async fn shell(self: Arc<Self>, request: Request<Incoming>) -> Response<Body> {
        let Some(lane) = self.lane.get().cloned() else {
            return json_response(StatusCode::NOT_FOUND, &json!({"error": "this PC's RigDesk has no agent shell lane", "code": "no_lane"}));
        };
        let Some(body) = Self::body_json(request).await else {
            return json_response(StatusCode::BAD_REQUEST, &json!({"error": "shell wants a JSON body", "code": "bad_request"}));
        };
        let (owner, asked) = match crate::manage::request(&body) {
            Ok(read) => read,
            Err(why) => return json_response(StatusCode::BAD_REQUEST, &json!({"error": why, "code": "bad_request"})),
        };
        // A command blocks for as long as it runs, so it must not run on the reactor.
        let served = tokio::task::spawn_blocking(move || lane.serve(&owner, asked)).await;
        match served {
            Ok(Ok(answer)) => json_response(StatusCode::OK, &json!(answer)),
            Ok(Err(refused)) => {
                let status = if matches!(refused, crate::manage::Refused::NoJob(_)) { StatusCode::NOT_FOUND } else { StatusCode::FORBIDDEN };
                json_response(status, &json!({"error": refused.words(), "code": refused.code()}))
            }
            Err(e) => json_response(StatusCode::INTERNAL_SERVER_ERROR, &json!({"error": format!("the shell handler panicked: {e}"), "code": "panicked"})),
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
        let shared = self.shared.clone();
        let (tx, body_rx) = mpsc::channel::<Result<hyper::body::Frame<Bytes>, Infallible>>(64);
        tokio::spawn(async move {
            // A reader that falls behind the live channel is caught up from the ring, as a reconnect would be; only what
            // the ring itself no longer holds is reported dropped. Transient events (actions) are not kept, so not resent.
            let mut sent = after;
            loop {
                let (backlog, gap, mut rx) = shared.events.resume(sent);
                if gap {
                    let _ = tx.send(Ok(sse(&Event { id: 0, at_ms: crate::now_ms(), kind: EventKind::Dropped { messages: 0 } }))).await;
                }
                let after = sent;
                for event in backlog.into_iter().filter(|e| e.id > after) {
                    sent = event.id;
                    if tx.send(Ok(sse(&event))).await.is_err() {
                        return;
                    }
                }
                let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + KEEP_ALIVE, KEEP_ALIVE);
                loop {
                    let received = tokio::select! {
                        received = rx.recv() => received,
                        _ = beat.tick() => {
                            if tx.send(Ok(hyper::body::Frame::data(Bytes::from_static(b": keep-alive\n\n")))).await.is_err() {
                                return;
                            }
                            continue;
                        }
                    };
                    match received {
                        Ok(event) if event.id != 0 && event.id <= sent => continue,
                        Ok(event) => {
                            sent = sent.max(event.id);
                            if tx.send(Ok(sse(&event))).await.is_err() {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                        Err(_) => return,
                    }
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

// A proxy in front of the API (tailscale serve) keeps the reader's connection open after this process dies; a reader
// that hears nothing for a few beats knows the stream is gone.
const KEEP_ALIVE: std::time::Duration = std::time::Duration::from_secs(15);

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
                EventKind::Mcp { on } => journal.record(&Entry::new("human", "mcp", if on { "on" } else { "off" }, Value::Null)),
                EventKind::Offer { ask, answer, .. } => journal.record(&Entry::new("human", "offer", answer, json!({"ask": ask}))),
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

    async fn health(port: u16) -> Value {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(b"GET /health HTTP/1.1\r\nhost: x\r\nauthorization: Bearer b\r\nconnection: close\r\n\r\n").await.unwrap();
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
    async fn a_route_with_no_use_for_the_body_keeps_the_connection_for_a_client_that_waits_to_send_it() {
        // Windows PowerShell's Invoke-RestMethod: Expect: 100-continue, the body only after 100 Continue or ~350 ms.
        let api = Api::new(Arc::new(Shared::new(Events::new(64))), Arc::new(NoDesk), Arc::new(Journal::memory()), "b", None);
        let port = api.serve(0).await.unwrap();
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(b"POST /leave HTTP/1.1\r\nhost: x\r\nauthorization: Bearer b\r\ncontent-type: application/json\r\ncontent-length: 2\r\nexpect: 100-continue\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        stream.write_all(b"{}").await.unwrap();
        stream.write_all(b"GET /health HTTP/1.1\r\nhost: x\r\nauthorization: Bearer b\r\nconnection: close\r\n\r\n").await.unwrap();
        let mut answer = String::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read_to_string(&mut answer)).await;
        assert!(answer.contains("403 Forbidden"), "{answer}");
        assert!(answer.contains("\"boot\""), "the connection closed after /leave: {answer}");
    }

    struct Toggle(std::sync::Mutex<Result<(), crate::operator::Closed>>);
    impl crate::operator::Switch for Toggle {
        fn open(&self) -> Result<(), crate::operator::Closed> {
            self.0.lock().unwrap().clone()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rigs_bearer_starts_a_session_only_while_the_pcs_switch_is_open() {
        let dir = std::env::temp_dir().join(format!("rigdesk-http-{}", rand::random::<u32>()));
        let shared = Arc::new(Shared::new(Events::new(64)));
        let journal = Arc::new(Journal::open(dir.join("a.jsonl"), 1 << 20));
        let api = Api::new(shared.clone(), Arc::new(NoDesk), journal.clone(), "b", None);
        let switch = Arc::new(Toggle(std::sync::Mutex::new(Err(crate::operator::Closed::Off("rerun the install line".into())))));
        api.operator_joins(switch.clone());
        let port = api.serve(0).await.unwrap();
        let join = json!({"agent": {"id": "a1", "title": "Helper", "graph": "g"}, "ask": "desk-1"});
        assert_eq!(post(port, "/join", "", &join).await["code"], json!("join_off"));
        *switch.0.lock().unwrap() = Err(crate::operator::Closed::Paused);
        assert_eq!(post(port, "/join", "", &join).await["code"], json!("join_paused"));
        *switch.0.lock().unwrap() = Ok(());
        let joined = post(port, "/join", "", &join).await;
        let token = joined["session"].as_str().unwrap().to_string();
        let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "type", "arguments": {}}});
        assert_eq!(post(port, "/mcp", &token, &call).await["result"]["isError"], json!(false));
        let other = json!({"agent": {"id": "a2", "title": "Other", "graph": "g"}, "ask": "desk-2"});
        let held = post(port, "/join", "", &other).await;
        assert_eq!(held["code"], json!("held"));
        assert!(held["error"].as_str().unwrap().contains("Helper holds"));
        let joins: Vec<String> = journal.tail(20).into_iter().filter(|e| e.what == "join").map(|e| e.outcome).collect();
        assert_eq!(joins, vec!["refused: join_off", "refused: join_paused", "ok", "refused: held"]);
        assert!(shared.revoke("done"));
        post(port, "/join", "", &json!({"agent": {"id": "a1", "title": "Helper", "graph": "g"}, "ask": "desk-3", "answered_by": "person"})).await;
        let by: Vec<Value> = journal.tail(20).into_iter().filter(|e| e.what == "join" && e.outcome == "ok").map(|e| e.detail["by"].clone()).collect();
        assert_eq!(by, vec![json!("Rig, operator-approved"), json!("Rig, allowed by the person at the PC")]);
        let _ = std::fs::remove_dir_all(dir);
    }

    struct Prompting(std::sync::Mutex<Vec<crate::operator::Offer>>);
    impl crate::operator::Switch for Prompting {
        fn open(&self) -> Result<(), crate::operator::Closed> {
            Ok(())
        }
        fn offer(&self, offer: crate::operator::Offer) -> Result<(), String> {
            self.0.lock().unwrap().push(offer);
            Ok(())
        }
        fn offer_end(&self, ask: &str) -> bool {
            let mut shown = self.0.lock().unwrap();
            let before = shown.len();
            shown.retain(|offer| offer.ask != ask);
            shown.len() != before
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rigs_bearer_shows_an_agents_request_to_the_person_at_the_pc_and_takes_it_away() {
        let api = Api::new(Arc::new(Shared::new(Events::new(64))), Arc::new(NoDesk), Arc::new(Journal::memory()), "b", None);
        let switch = Arc::new(Prompting(Default::default()));
        api.operator_joins(switch.clone());
        let port = api.serve(0).await.unwrap();
        let agent = json!({"id": "a1", "title": "Helper", "graph": "g"});
        assert_eq!(post(port, "/offer", "", &json!({"agent": agent, "ask": "desk-1"})).await["code"], json!("bad_request"), "an offer without Rig's nonce");
        assert_eq!(post(port, "/offer", "", &json!({"agent": agent, "ask": "desk-1", "nonce": "0123456789abcdef"})).await["shown"], json!(true));
        assert_eq!(switch.0.lock().unwrap()[0].owner.title, "Helper");
        assert_eq!(post(port, "/offer/end", "", &json!({"ask": "desk-1"})).await["ended"], json!(true));
        assert_eq!(post(port, "/offer/end", "", &json!({"ask": "desk-1"})).await["ended"], json!(false));
        let plain = Api::new(Arc::new(Shared::new(Events::new(64))), Arc::new(NoDesk), Arc::new(Journal::memory()), "b", None);
        let plain = plain.serve(0).await.unwrap();
        assert_eq!(post(plain, "/offer", "", &json!({"agent": agent, "ask": "desk-1", "nonce": "0123456789abcdef"})).await["code"], json!("no_offer"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_invites_claim_is_journaled_as_one_and_a_claim_without_rigs_key_is_journaled_refused() {
        let dir = std::env::temp_dir().join(format!("rigdesk-http-{}", rand::random::<u32>()));
        let shared = Arc::new(Shared::new(Events::new(64)));
        let journal = Arc::new(Journal::open(dir.join("a.jsonl"), 1 << 20));
        let api = Api::new(shared.clone(), Arc::new(NoDesk), journal.clone(), "b", None);
        let switch = Arc::new(Toggle(std::sync::Mutex::new(Err(crate::operator::Closed::Paused))));
        api.operator_joins(switch.clone());
        let port = api.serve(0).await.unwrap();
        for _ in 0..3 {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            stream.write_all(b"POST /claim HTTP/1.1\r\nhost: x\r\nauthorization: Bearer stolen\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}").await.unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).await.unwrap();
            assert!(answer.starts_with("HTTP/1.1 401"), "{answer}");
        }
        assert_eq!(health(port).await["invite_open"], json!(false));
        let (invite, _) = shared.invite(None).unwrap();
        assert_eq!(health(port).await["invite_open"], json!(true), "Rig asks its operator before claiming an invite on a PC an agent may run commands on");
        let agent = json!({"id": "a1", "title": "Helper", "graph": "g"});
        let claim = json!({"grant": invite.grant, "secret": invite.secret, "agent": agent});
        assert!(post(port, "/claim", "", &claim).await["error"].as_str().unwrap().contains("paused"));
        *switch.0.lock().unwrap() = Ok(());
        assert!(post(port, "/claim", "", &claim).await["session"].is_string());
        assert_eq!(health(port).await["invite_open"], json!(false));

        let claims: Vec<(String, Value)> = journal.tail(20).into_iter().filter(|e| e.what == "claim").map(|e| (e.outcome, e.detail)).collect();
        assert_eq!(claims[0], ("refused: bad bearer token".to_string(), json!({"also_refused_since_last": 0})));
        assert_eq!(claims.len(), 3, "three wrong bearers are one line: {claims:?}");
        assert_eq!((claims[1].0.as_str(), &claims[1].1["invited"]), ("refused", &json!(true)));
        assert_eq!((claims[2].0.as_str(), &claims[2].1["invited"]), ("ok", &json!(true)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pc_without_operator_joins_says_so() {
        let api = Api::new(Arc::new(Shared::new(Events::new(64))), Arc::new(NoDesk), Arc::new(Journal::memory()), "b", None);
        let port = api.serve(0).await.unwrap();
        assert_eq!(post(port, "/join", "", &json!({"agent": {"id": "a1"}})).await["code"], json!("no_join"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_viewer_stops_the_agent_and_sends_it_a_note() {
        let shared = Arc::new(Shared::new(Events::new(64)));
        let api = Api::hosted(shared.clone(), Arc::new(NoDesk), Arc::new(Journal::memory()), "b", None);
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
        let first = post(port, "/viewer/note", "", &json!({"text": "turn it on", "key": "phone:1:7"})).await;
        let again = post(port, "/viewer/note", "", &json!({"text": "turn it on", "key": "phone:1:7"})).await;
        assert_eq!((again["event"].clone(), again["replayed"].clone()), (first["event"].clone(), json!(true)), "a note sent again is told once");
        let told = events.recv().await.unwrap();
        assert!(matches!(told.kind, EventKind::Message { ref text, .. } if text == "turn it on"), "{told:?}");
        let next = post(port, "/viewer/note", "", &json!({"text": "and the other", "key": "phone:1:8"})).await;
        let told = events.recv().await.unwrap();
        assert!(matches!(told.kind, EventKind::Message { ref text, .. } if text == "and the other"), "the replay made no event: {told:?}");
        assert_eq!(told.id, next["event"].as_u64().unwrap());
    }
}
