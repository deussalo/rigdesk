//! MCP servers already running on the PC, found while an agent is connected and offered to it through the agent API.
//!
//! Every few seconds RigDesk lists the TCP ports the user's processes listen on at loopback and asks each new one MCP's
//! opening question (`initialize`): over streamable HTTP at `/mcp` then `/`, then over the older HTTP+SSE transport at
//! `/sse`. One that answers as MCP is a server, named by what it calls itself. Calls to it are forwarded from
//! `/servers/<name>/mcp` to its port, under the same holder and Block rules as the desk tools; an SSE server's event
//! stream stays open while it is offered, as its one session (see [`Relay`]). No config file is read and nothing is
//! opened beyond loopback.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::events::EventKind;
use crate::relay::{Relay, Upstream};
use crate::session::Shared;

/// How often the PC's listeners are listed while an agent is connected.
pub const SCAN_EVERY: Duration = Duration::from_secs(5);
/// A port that did not answer as MCP is asked again only after this long, or when a different process holds it.
const NOT_MCP_FOR: Duration = Duration::from_secs(120);
const PROBE_WAIT: Duration = Duration::from_secs(2);
/// A forwarded call waits this long for the server's answer.
pub const CALL_WAIT: Duration = Duration::from_secs(300);
const PATHS: [&str; 2] = ["/mcp", "/"];
/// Where the older HTTP+SSE transport opens its event stream: the MCP SDKs' default.
const SSE_PATH: &str = "/sse";
/// A server's answer larger than this is refused rather than held in memory.
const ANSWER_CAP: usize = 64 << 20;

#[derive(Clone, Debug, Serialize)]
pub struct Server {
    /// What it calls itself, made safe for Rig's server names: [a-z0-9_.-], at most 48 characters.
    pub name: String,
    pub port: u16,
    pub path: String,
    /// Its own words for itself: serverInfo's name and version.
    pub says: String,
    pub since_ms: u64,
    /// The session to a server that speaks only HTTP+SSE; none for streamable HTTP, which is asked per message.
    #[serde(rename = "transport", serialize_with = "transport_of")]
    relay: Option<Arc<Relay>>,
}

fn transport_of<S: serde::Serializer>(relay: &Option<Arc<Relay>>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(if relay.is_some() { "sse" } else { "streamable_http" })
}

impl Server {
    /// One client message to the server, answered as streamable HTTP would answer it.
    pub async fn call(&self, headers: &[(&str, &str)], body: &[u8]) -> Result<Answer, String> {
        let Some(relay) = &self.relay else {
            return post(self.port, &self.path, headers, body).await;
        };
        let message: Value = serde_json::from_slice(body).map_err(|_| "the body is not one JSON-RPC message".to_string())?;
        if !message.is_object() {
            return Err("send one JSON-RPC message, not a batch".into());
        }
        Ok(match relay.handle(message).await? {
            Some(answer) => Answer { status: 200, headers: vec![("content-type".into(), "application/json".into())], body: answer.to_string().into_bytes() },
            None => Answer { status: 202, headers: Vec::new(), body: Vec::new() },
        })
    }

    /// Why it is gone though its port still listens: its event stream closed.
    fn dropped(&self) -> Option<String> {
        self.relay.as_ref().filter(|r| r.ended()).map(|_| format!("its event stream on port {} closed", self.port))
    }

    fn close(&self) {
        if let Some(relay) = &self.relay {
            relay.end();
        }
    }
}

/// One TCP listener at loopback, with the process that holds it where the OS says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Listener {
    pub port: u16,
    pub holder: Option<u64>,
}

pub struct Found {
    servers: Mutex<BTreeMap<String, Server>>,
    not_mcp: Mutex<HashMap<Listener, Instant>>,
    on: AtomicBool,
    /// The agent API's own port and RustDesk's: never asked.
    skip: HashSet<u16>,
}

impl Found {
    /// `on` is the PC's MCP switch as it was left; on unless the person there turned it off.
    pub fn new(skip: impl IntoIterator<Item = u16>, on: bool) -> Arc<Self> {
        Arc::new(Found { servers: Mutex::default(), not_mcp: Mutex::default(), on: AtomicBool::new(on), skip: skip.into_iter().collect() })
    }

    pub fn on(&self) -> bool {
        self.on.load(Ordering::SeqCst)
    }

    /// The PC's MCP switch. Off, no port is asked and no server is offered; screen, input and shell stay on.
    /// Answers whether it changed; a change is an `Mcp` event, after the servers it closed.
    pub fn set_on(&self, on: bool, shared: &Shared) -> bool {
        if self.on.swap(on, Ordering::SeqCst) == on {
            return false;
        }
        if !on {
            self.forget_all("MCP was turned off on the PC", shared);
        }
        shared.events.push(EventKind::Mcp { on });
        true
    }

    pub fn servers(&self) -> Vec<Server> {
        self.servers.lock().unwrap_or_else(|p| p.into_inner()).values().cloned().collect()
    }

    pub fn get(&self, name: &str) -> Option<Server> {
        self.servers.lock().unwrap_or_else(|p| p.into_inner()).get(name).cloned()
    }

    fn forget_all(&self, why: &str, shared: &Shared) {
        let gone = std::mem::take(&mut *self.servers.lock().unwrap_or_else(|p| p.into_inner()));
        for server in gone.into_values() {
            server.close();
            shared.events.push(EventKind::Server { name: server.name, found: false, says: server.says, why: Some(why.to_string()) });
        }
    }

    /// Scan while the process lives: only while an agent is connected and MCP is on, so an idle PC is never probed.
    pub async fn watch(self: Arc<Self>, shared: Arc<Shared>) {
        loop {
            tokio::time::sleep(SCAN_EVERY).await;
            if !self.on() {
                continue;
            }
            if shared.standing().owner.is_none() {
                if !self.servers().is_empty() {
                    self.forget_all("the agent left", &shared);
                }
                continue;
            }
            let listening = match tokio::task::spawn_blocking(listeners).await {
                Ok(Ok(found)) => found,
                Ok(Err(why)) => {
                    log::warn!("mcp scan: cannot list listeners: {why}");
                    continue;
                }
                Err(_) => continue,
            };
            self.scan(&listening, &shared).await;
        }
    }

    async fn scan(&self, listening: &[Listener], shared: &Shared) {
        let ports: HashSet<u16> = listening.iter().map(|l| l.port).collect();
        let gone: Vec<(Server, String)> = {
            let mut servers = self.servers.lock().unwrap_or_else(|p| p.into_inner());
            let why: Vec<(String, String)> = servers
                .values()
                .filter_map(|s| if ports.contains(&s.port) { s.dropped() } else { Some(format!("nothing listens on port {} now", s.port)) }.map(|w| (s.name.clone(), w)))
                .collect();
            why.into_iter().filter_map(|(n, w)| servers.remove(&n).map(|s| (s, w))).collect()
        };
        for (server, why) in gone {
            server.close();
            shared.events.push(EventKind::Server { name: server.name, found: false, says: server.says, why: Some(why) });
        }
        let known: HashSet<u16> = self.servers().iter().map(|s| s.port).collect();
        let now = Instant::now();
        self.not_mcp.lock().unwrap_or_else(|p| p.into_inner()).retain(|l, at| ports.contains(&l.port) && now.duration_since(*at) < NOT_MCP_FOR);
        for listener in listening {
            if self.skip.contains(&listener.port) || known.contains(&listener.port) || self.not_mcp.lock().unwrap_or_else(|p| p.into_inner()).contains_key(listener) {
                continue;
            }
            match probe(listener.port).await {
                Some((path, info, relay)) => {
                    let server = self.named(listener.port, path, &info, relay);
                    log::info!("mcp scan: {} on port {} ({})", server.name, server.port, server.says);
                    shared.events.push(EventKind::Server { name: server.name.clone(), found: true, says: server.says.clone(), why: None });
                    self.servers.lock().unwrap_or_else(|p| p.into_inner()).insert(server.name.clone(), server);
                }
                None => {
                    self.not_mcp.lock().unwrap_or_else(|p| p.into_inner()).insert(*listener, now);
                }
            }
        }
    }

    fn named(&self, port: u16, path: &str, info: &Value, relay: Option<Arc<Relay>>) -> Server {
        let said = info.get("name").and_then(Value::as_str).unwrap_or("");
        let version = info.get("version").and_then(Value::as_str).unwrap_or("");
        let mut name = server_name(said);
        if name.is_empty() {
            name = format!("port{port}");
        }
        let servers = self.servers.lock().unwrap_or_else(|p| p.into_inner());
        if servers.contains_key(&name) {
            name = format!("{name}-{port}");
        }
        Server { name, port, path: path.to_string(), says: format!("{said} {version}").trim().to_string(), since_ms: crate::now_ms(), relay }
    }
}

/// A name fit for Rig's server names, from what the server calls itself.
pub fn server_name(said: &str) -> String {
    let mut name: String = said
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') { c } else { '-' })
        .collect();
    while name.contains("--") {
        name = name.replace("--", "-");
    }
    name.trim_matches(|c| c == '-' || c == '.').chars().take(48).collect()
}

fn opening() -> Value {
    json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
        "protocolVersion": crate::mcp::PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "rigdesk-scan", "version": env!("CARGO_PKG_VERSION")}}})
}

fn server_info(reply: &Value) -> Option<&Value> {
    reply.pointer("/result/serverInfo").filter(|_| reply.pointer("/result/protocolVersion").is_some())
}

/// Asks a port MCP's opening question; answers the path that answered, the server's serverInfo, and for an HTTP+SSE
/// server the session that stays open.
async fn probe(port: u16) -> Option<(&'static str, Value, Option<Arc<Relay>>)> {
    if let Some((path, info)) = probe_http(port).await {
        return Some((path, info, None));
    }
    let (relay, info) = probe_sse(port).await?;
    Some((SSE_PATH, info, Some(relay)))
}

async fn probe_http(port: u16) -> Option<(&'static str, Value)> {
    let ask = opening();
    for path in PATHS {
        let Ok(Ok(answer)) = tokio::time::timeout(PROBE_WAIT, post(port, path, &[], ask.to_string().as_bytes())).await else {
            continue;
        };
        if answer.status != 200 {
            continue;
        }
        let Some(reply) = answer.json_rpc() else { continue };
        if let Some(info) = server_info(&reply) {
            if let Some(session) = answer.header("mcp-session-id") {
                let _ = tokio::time::timeout(PROBE_WAIT, request(port, "DELETE", path, &[("mcp-session-id", session)], b"")).await;
            }
            return Some((path, info.clone()));
        }
    }
    None
}

/// Opens an HTTP+SSE server's event stream and initializes it; on success the stream is read for as long as the relay
/// lives, and the relay's `initialize` answer is kept for every client.
async fn probe_sse(port: u16) -> Option<(Arc<Relay>, Value)> {
    let (mut events, endpoint) = tokio::time::timeout(PROBE_WAIT, EventStream::open(port)).await.ok()?.ok()?;
    let relay = Relay::new(Upstream::Posted { port, endpoint });
    let reading = relay.clone();
    let reader = tokio::spawn(async move {
        while let Ok(Some((name, data))) = events.next().await {
            if name == "message" {
                if let Ok(message) = serde_json::from_str::<Value>(&data) {
                    reading.heard(message).await;
                }
            }
        }
        reading.end();
    });
    relay.set_reader(reader.abort_handle());
    let opened = tokio::time::timeout(PROBE_WAIT, relay.handle(opening())).await;
    match opened {
        Ok(Ok(Some(reply))) if server_info(&reply).is_some() => {
            let info = server_info(&reply).cloned()?;
            Some((relay, info))
        }
        _ => {
            relay.end();
            None
        }
    }
}

/// A server-sent event stream from a loopback port, its body read as it comes (chunked or not).
struct EventStream {
    stream: TcpStream,
    /// Bytes read but not yet decoded from the transfer encoding.
    raw: Vec<u8>,
    /// The body so far, not yet split into events.
    body: Vec<u8>,
    chunked: bool,
    /// Bytes left in the current chunk, then whether its closing CRLF is still to come.
    chunk_left: usize,
    chunk_trail: bool,
    ended: bool,
}

impl EventStream {
    /// GET /sse; answers the stream and the message endpoint its first event names.
    async fn open(port: u16) -> Result<(EventStream, String), String> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
        let head = format!("GET {SSE_PATH} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\naccept: text/event-stream\r\ncache-control: no-cache\r\n\r\n");
        stream.write_all(head.as_bytes()).await.map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        let mut chunk = [0u8; 4096];
        let split = loop {
            if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break at;
            }
            if raw.len() > 64 << 10 {
                return Err("headers too long".into());
            }
            let n = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("closed before its headers".into());
            }
            raw.extend_from_slice(&chunk[..n]);
        };
        let (status, headers) = parse_head(&raw[..split])?;
        let event_stream = headers.iter().any(|(n, v)| n == "content-type" && v.starts_with("text/event-stream"));
        if status != 200 || !event_stream {
            return Err(format!("{SSE_PATH} answered {status}, not an event stream"));
        }
        let chunked = headers.iter().any(|(n, v)| n == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
        let mut events = EventStream { stream, raw: raw.split_off(split + 4), body: Vec::new(), chunked, chunk_left: 0, chunk_trail: false, ended: false };
        let (name, data) = events.next().await?.ok_or("the stream ended before it named an endpoint")?;
        if name != "endpoint" {
            return Err(format!("the stream's first event is {name:?}, not endpoint"));
        }
        let endpoint = endpoint_path(&data, port).ok_or_else(|| format!("the endpoint {data:?} is not on this port"))?;
        Ok((events, endpoint))
    }

    /// The next event's name ("message" when unnamed) and data; None once the stream ends.
    async fn next(&mut self) -> Result<Option<(String, String)>, String> {
        loop {
            if let Some((event, rest)) = split_event(&self.body) {
                self.body.drain(..rest);
                if let Some(event) = event {
                    return Ok(Some(event));
                }
                continue;
            }
            if self.ended {
                return Ok(None);
            }
            if self.body.len() > ANSWER_CAP {
                return Err(format!("an event is over {} MiB", ANSWER_CAP >> 20));
            }
            let mut chunk = [0u8; 16 << 10];
            let n = self.stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
            if n == 0 {
                self.ended = true;
            }
            self.raw.extend_from_slice(&chunk[..n]);
            self.decode()?;
        }
    }

    /// Moves what has arrived from `raw` into `body`, undoing chunked transfer encoding as far as it can.
    fn decode(&mut self) -> Result<(), String> {
        if !self.chunked {
            self.body.append(&mut self.raw);
            return Ok(());
        }
        loop {
            if self.chunk_left > 0 {
                let n = self.chunk_left.min(self.raw.len());
                if n == 0 {
                    return Ok(());
                }
                self.body.extend(self.raw.drain(..n));
                self.chunk_left -= n;
                self.chunk_trail = self.chunk_left == 0;
                continue;
            }
            if self.chunk_trail {
                if self.raw.len() < 2 {
                    return Ok(());
                }
                self.raw.drain(..2);
                self.chunk_trail = false;
                continue;
            }
            let Some(end) = self.raw.windows(2).position(|w| w == b"\r\n") else { return Ok(()) };
            let line = std::str::from_utf8(&self.raw[..end]).map_err(|_| "a chunk size is not text")?;
            let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16).map_err(|_| format!("bad chunk size {line:?}"))?;
            self.raw.drain(..end + 2);
            if size == 0 {
                self.ended = true;
                return Ok(());
            }
            self.chunk_left = size;
        }
    }
}

/// The first whole event in `body`, and how many bytes it took. A block with no data (a comment, a keep-alive) is None.
fn split_event(body: &[u8]) -> Option<(Option<(String, String)>, usize)> {
    let mut start = 0;
    let mut lines = Vec::new();
    while let Some(nl) = body[start..].iter().position(|&b| b == b'\n') {
        let line = &body[start..start + nl];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        start += nl + 1;
        if line.is_empty() {
            let (mut name, mut data, mut has_data) = (String::from("message"), String::new(), false);
            for line in lines {
                let text = String::from_utf8_lossy(line);
                let (field, value) = text.split_once(':').map_or((text.as_ref(), ""), |(f, v)| (f, v.strip_prefix(' ').unwrap_or(v)));
                match field {
                    "event" => name = value.to_string(),
                    "data" => {
                        if has_data {
                            data.push('\n');
                        }
                        data.push_str(value);
                        has_data = true;
                    }
                    _ => {}
                }
            }
            return Some((has_data.then_some((name, data)), start));
        }
        lines.push(line);
    }
    None
}

/// The message endpoint an event stream named, as a path on this port: a path, or a loopback URL on the same port.
fn endpoint_path(data: &str, port: u16) -> Option<String> {
    let data = data.trim();
    let path = if data.starts_with('/') {
        data.to_string()
    } else {
        let rest = data.strip_prefix("http://")?;
        let (authority, path) = rest.split_at(rest.find('/')?);
        let local = [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")];
        if !local.iter().any(|l| l == authority) {
            return None;
        }
        path.to_string()
    };
    (!path.chars().any(|c| c.is_whitespace() || c.is_control())).then_some(path)
}

/// What a local server answered, read whole.
pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The JSON-RPC message in the body: the body itself, or the first `data:` of an event stream that parses.
    pub fn json_rpc(&self) -> Option<Value> {
        let text = std::str::from_utf8(&self.body).ok()?;
        if self.header("content-type").map_or(false, |t| t.starts_with("text/event-stream")) {
            return text.lines().filter_map(|l| l.strip_prefix("data:")).find_map(|d| serde_json::from_str::<Value>(d.trim()).ok());
        }
        serde_json::from_str(text).ok()
    }
}

/// POST a JSON-RPC body to a loopback port, as an MCP client does over streamable HTTP.
pub async fn post(port: u16, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Answer, String> {
    let mut all = vec![("content-type", "application/json"), ("accept", "application/json, text/event-stream")];
    all.extend(headers.iter().filter(|(n, _)| !n.eq_ignore_ascii_case("content-type") && !n.eq_ignore_ascii_case("accept")));
    request(port, "POST", path, &all, body).await
}

/// One HTTP/1.1 request to 127.0.0.1:port, the connection closed after it so the answer ends with the stream.
pub async fn request(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Answer, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| format!("cannot reach port {port}: {e}"))?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nconnection: close\r\ncontent-length: {}\r\n", body.len());
    for (name, value) in headers {
        if !value.contains(['\r', '\n']) {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.map_err(|e| e.to_string())?;
    stream.write_all(body).await.map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 16 << 10];
    loop {
        let n = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
        if raw.len() > ANSWER_CAP {
            return Err(format!("the answer from port {port} is over {} MiB", ANSWER_CAP >> 20));
        }
    }
    parse_answer(&raw)
}

fn parse_answer(raw: &[u8]) -> Result<Answer, String> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("the answer has no end of headers")?;
    let (status, headers) = parse_head(&raw[..split])?;
    let rest = &raw[split + 4..];
    let chunked = headers.iter().any(|(n, v)| n == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
    let body = if chunked { dechunk(rest)? } else { rest.to_vec() };
    Ok(Answer { status, headers, body })
}

fn parse_head(head: &[u8]) -> Result<(u16, Vec<(String, String)>), String> {
    let head = std::str::from_utf8(head).map_err(|_| "the answer's headers are not text")?;
    let mut lines = head.split("\r\n");
    let status = lines.next().and_then(|l| l.split_whitespace().nth(1)).and_then(|s| s.parse().ok()).ok_or("the answer has no status")?;
    let headers = lines.filter_map(|l| l.split_once(':')).map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    Ok((status, headers))
}

fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let end = rest.windows(2).position(|w| w == b"\r\n").ok_or("a chunk has no size line")?;
        let size_line = std::str::from_utf8(&rest[..end]).map_err(|_| "a chunk size is not text")?;
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16).map_err(|_| format!("bad chunk size {size_line:?}"))?;
        rest = &rest[end + 2..];
        if size == 0 {
            return Ok(body);
        }
        if rest.len() < size {
            // The stream ended inside a chunk: keep what came.
            body.extend_from_slice(rest);
            return Ok(body);
        }
        body.extend_from_slice(&rest[..size]);
        rest = rest.get(size + 2..).unwrap_or(&[]);
    }
}

/// The TCP ports the user's processes listen on at loopback (or every address, loopback with it).
#[cfg(target_os = "linux")]
pub fn listeners() -> Result<Vec<Listener>, String> {
    use std::os::unix::fs::MetadataExt;
    let me = std::fs::metadata("/proc/self").map_err(|e| e.to_string())?.uid();
    let mut found = HashSet::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else { continue };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (Some(local), Some(state), Some(uid), Some(inode)) = (fields.get(1), fields.get(3), fields.get(7), fields.get(9)) else { continue };
            if *state != "0A" || uid.parse::<u32>().ok() != Some(me) {
                continue;
            }
            let Some((addr, port)) = local.split_once(':') else { continue };
            if !loopback_hex(addr) {
                continue;
            }
            if let Ok(port) = u16::from_str_radix(port, 16) {
                found.insert(Listener { port, holder: inode.parse().ok() });
            }
        }
    }
    Ok(found.into_iter().collect())
}

/// /proc/net/tcp's address, in the kernel's byte order: 127.x.x.x, 0.0.0.0, ::1, :: or ::ffff:127.x.x.x.
#[cfg(target_os = "linux")]
fn loopback_hex(addr: &str) -> bool {
    match addr.len() {
        8 => addr == "00000000" || addr.ends_with("7F"),
        32 => {
            addr == "00000000000000000000000000000000"
                || addr == "00000000000000000000000001000000"
                || (addr.starts_with("0000000000000000FFFF0000") && addr.ends_with("7F"))
        }
        _ => false,
    }
}

/// The TCP ports listening at loopback (or every address), each with the process that holds it, from netstat.
#[cfg(target_os = "windows")]
pub fn listeners() -> Result<Vec<Listener>, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("netstat.exe").args(["-ano", "-p", "TCP"]).creation_flags(CREATE_NO_WINDOW).output().map_err(|e| e.to_string())?;
    let out6 = std::process::Command::new("netstat.exe").args(["-ano", "-p", "TCPv6"]).creation_flags(CREATE_NO_WINDOW).output().map_err(|e| e.to_string())?;
    let mut found = HashSet::new();
    for line in String::from_utf8_lossy(&out.stdout).lines().chain(String::from_utf8_lossy(&out6.stdout).lines()) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 5 || fields[3] != "LISTENING" {
            continue;
        }
        let Some((addr, port)) = fields[1].rsplit_once(':') else { continue };
        if !matches!(addr, "127.0.0.1" | "0.0.0.0" | "[::1]" | "[::]") {
            continue;
        }
        if let Ok(port) = port.parse() {
            found.insert(Listener { port, holder: fields[4].parse().ok() });
        }
    }
    Ok(found.into_iter().collect())
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn listeners() -> Result<Vec<Listener>, String> {
    Err("finding MCP servers is not built for this OS".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_fit_for_rig() {
        assert_eq!(server_name("Blender MCP"), "blender-mcp");
        assert_eq!(server_name("  @scope/server!  "), "scope-server");
        assert_eq!(server_name("a".repeat(80).as_str()).len(), 48);
    }

    #[test]
    fn a_chunked_event_stream_answer_is_read() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nMcp-Session-Id: s1\r\n\r\n\
1d\r\nevent: message\ndata: {\"a\":1}\n\n\r\n0\r\n\r\n";
        let answer = parse_answer(raw).unwrap();
        assert_eq!(answer.status, 200);
        assert_eq!(answer.header("mcp-session-id"), Some("s1"));
        assert_eq!(answer.json_rpc(), Some(json!({"a": 1})));
    }

    #[test]
    fn server_sent_events_are_split_as_they_arrive() {
        let body = b": keep-alive\r\n\r\nevent: endpoint\r\ndata: /messages/?session_id=1\r\n\r\ndata: {\"a\":\ndata: 1}\n\ndata: half";
        let (comment, used) = split_event(body).unwrap();
        assert_eq!(comment, None);
        let (endpoint, more) = split_event(&body[used..]).unwrap();
        assert_eq!(endpoint, Some(("endpoint".into(), "/messages/?session_id=1".into())));
        let (message, rest) = split_event(&body[used + more..]).unwrap();
        assert_eq!(message, Some(("message".into(), "{\"a\":\n1}".into())));
        assert_eq!(split_event(&body[used + more + rest..]), None);
    }

    #[test]
    fn an_endpoint_must_be_on_the_same_loopback_port() {
        assert_eq!(endpoint_path("/messages/?session_id=1", 9), Some("/messages/?session_id=1".into()));
        assert_eq!(endpoint_path("http://127.0.0.1:9/m?s=1", 9), Some("/m?s=1".into()));
        assert_eq!(endpoint_path("http://localhost:9/m", 9), Some("/m".into()));
        assert_eq!(endpoint_path("http://127.0.0.1:10/m", 9), None);
        assert_eq!(endpoint_path("http://evil.example:9/m", 9), None);
        assert_eq!(endpoint_path("https://127.0.0.1:9/m", 9), None);
        assert_eq!(endpoint_path("/m HTTP/1.1\r\nx: y", 9), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn loopback_addresses_in_kernel_order() {
        assert!(loopback_hex("0100007F"));
        assert!(loopback_hex("00000000"));
        assert!(!loopback_hex("0101A8C0"));
        assert!(loopback_hex("00000000000000000000000001000000"));
    }
}
