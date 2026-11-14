//! MCP servers already running on the PC, found while an agent is connected and offered to it through the agent API.
//!
//! Every few seconds RigDesk lists the TCP ports the user's processes listen on at loopback and asks each new one MCP's
//! opening question (`initialize`, streamable HTTP, at `/mcp` then `/`). One that answers as MCP is a server, named by
//! what it calls itself. Calls to it are forwarded from `/servers/<name>/mcp` to its port, under the same holder and
//! Block rules as the desk tools. No config file is read and nothing is opened beyond loopback.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::events::EventKind;
use crate::session::Shared;

/// How often the PC's listeners are listed while an agent is connected.
pub const SCAN_EVERY: Duration = Duration::from_secs(5);
/// A port that did not answer as MCP is asked again only after this long, or when a different process holds it.
const NOT_MCP_FOR: Duration = Duration::from_secs(120);
const PROBE_WAIT: Duration = Duration::from_secs(2);
/// A forwarded call waits this long for the server's answer.
pub const CALL_WAIT: Duration = Duration::from_secs(300);
const PATHS: [&str; 2] = ["/mcp", "/"];
/// A server's answer larger than this is refused rather than held in memory.
const ANSWER_CAP: usize = 64 << 20;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Server {
    /// What it calls itself, made safe for Rig's server names: [a-z0-9_.-], at most 48 characters.
    pub name: String,
    pub port: u16,
    pub path: String,
    /// Its own words for itself: serverInfo's name and version.
    pub says: String,
    pub since_ms: u64,
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
    pub fn new(skip: impl IntoIterator<Item = u16>) -> Arc<Self> {
        Arc::new(Found { servers: Mutex::default(), not_mcp: Mutex::default(), on: AtomicBool::new(true), skip: skip.into_iter().collect() })
    }

    pub fn on(&self) -> bool {
        self.on.load(Ordering::SeqCst)
    }

    /// The PC's MCP switch. Off, no port is asked and no server is offered; screen, input and shell stay on.
    pub fn set_on(&self, on: bool, shared: &Shared) {
        if self.on.swap(on, Ordering::SeqCst) != on && !on {
            self.forget_all("MCP was turned off on the PC", shared);
        }
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
        let gone: Vec<Server> = {
            let mut servers = self.servers.lock().unwrap_or_else(|p| p.into_inner());
            let names: Vec<String> = servers.values().filter(|s| !ports.contains(&s.port)).map(|s| s.name.clone()).collect();
            names.into_iter().filter_map(|n| servers.remove(&n)).collect()
        };
        for server in gone {
            shared.events.push(EventKind::Server { name: server.name, found: false, says: server.says, why: Some(format!("nothing listens on port {} now", server.port)) });
        }
        let known: HashSet<u16> = self.servers().iter().map(|s| s.port).collect();
        let now = Instant::now();
        self.not_mcp.lock().unwrap_or_else(|p| p.into_inner()).retain(|l, at| ports.contains(&l.port) && now.duration_since(*at) < NOT_MCP_FOR);
        for listener in listening {
            if self.skip.contains(&listener.port) || known.contains(&listener.port) || self.not_mcp.lock().unwrap_or_else(|p| p.into_inner()).contains_key(listener) {
                continue;
            }
            match probe(listener.port).await {
                Some((path, info)) => {
                    let server = self.named(listener.port, path, &info);
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

    fn named(&self, port: u16, path: &str, info: &Value) -> Server {
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
        Server { name, port, path: path.to_string(), says: format!("{said} {version}").trim().to_string(), since_ms: crate::now_ms() }
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

/// Asks a port MCP's opening question; answers the path that answered and the server's serverInfo.
async fn probe(port: u16) -> Option<(&'static str, Value)> {
    let ask = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
        "protocolVersion": crate::mcp::PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "rigdesk-scan", "version": env!("CARGO_PKG_VERSION")}}});
    for path in PATHS {
        let Ok(Ok(answer)) = tokio::time::timeout(PROBE_WAIT, post(port, path, &[], ask.to_string().as_bytes())).await else {
            continue;
        };
        if answer.status != 200 {
            continue;
        }
        let Some(reply) = answer.json_rpc() else { continue };
        if let Some(info) = reply.pointer("/result/serverInfo").filter(|_| reply.pointer("/result/protocolVersion").is_some()) {
            if let Some(session) = answer.header("mcp-session-id") {
                let _ = tokio::time::timeout(PROBE_WAIT, request(port, "DELETE", path, &[("mcp-session-id", session)], b"")).await;
            }
            return Some((path, info.clone()));
        }
    }
    None
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
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| "the answer's headers are not text")?;
    let mut lines = head.split("\r\n");
    let status = lines.next().and_then(|l| l.split_whitespace().nth(1)).and_then(|s| s.parse().ok()).ok_or("the answer has no status")?;
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let rest = &raw[split + 4..];
    let chunked = headers.iter().any(|(n, v)| n == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
    let body = if chunked { dechunk(rest)? } else { rest.to_vec() };
    Ok(Answer { status, headers, body })
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

    #[cfg(target_os = "linux")]
    #[test]
    fn loopback_addresses_in_kernel_order() {
        assert!(loopback_hex("0100007F"));
        assert!(loopback_hex("00000000"));
        assert!(!loopback_hex("0101A8C0"));
        assert!(loopback_hex("00000000000000000000000001000000"));
    }
}
