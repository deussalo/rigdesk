//! `rustdesk --mcp [--port N] -- <command> [args…]`: puts an MCP server that a client would start and talk to over its
//! stdin and stdout on a loopback port, as streamable HTTP at `/mcp`, where RigDesk's scan finds it like any server that
//! listens. The agent or the human runs it in the shell; it lasts as long as the command does and ends with its status.
//!
//! One command serves every client. Its first `initialize` is the one it answers; later clients, RigDesk's scan among them,
//! get that same answer from here, since a stdio server is opened once. Request ids are the wrapper's own on the command's
//! side, so two clients' ids never meet. What the command asks of a client (sampling, roots, elicitation) has no one to go
//! to: it is answered as a method not found, and its notifications are dropped. Lines it prints that are not JSON go to
//! stderr, so a server that logs to stdout still works.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{ChildStdin, Command};
use tokio::sync::{oneshot, OnceCell};

/// A client's message larger than this is refused.
const BODY_CAP: usize = 16 << 20;

type Body = Full<Bytes>;
type Waiting = Mutex<HashMap<u64, oneshot::Sender<Value>>>;

/// What `--mcp` was given: the port to listen on (0 for any free one) and the command to run.
#[derive(Debug, PartialEq, Eq)]
pub struct Asked {
    pub port: u16,
    pub command: Vec<String>,
}

pub const USAGE: &str = "usage: rustdesk --mcp [--port N] -- <command> [args...]\n\
puts an MCP server that speaks over stdin and stdout on 127.0.0.1, at http://127.0.0.1:<port>/mcp";

/// Reads the words after `--mcp`.
pub fn asked(words: &[String]) -> Result<Asked, String> {
    let mut port = 0u16;
    let mut rest = words;
    loop {
        match rest {
            [flag, value, tail @ ..] if flag == "--port" => {
                port = value.parse().map_err(|_| format!("--port takes a port number, not {value:?}"))?;
                rest = tail;
            }
            [dashes, tail @ ..] if dashes == "--" => {
                rest = tail;
                break;
            }
            _ => break,
        }
    }
    if rest.is_empty() || rest[0].starts_with("--") {
        return Err(USAGE.to_string());
    }
    Ok(Asked { port, command: rest.to_vec() })
}

struct Wrapped {
    stdin: tokio::sync::Mutex<ChildStdin>,
    waiting: Waiting,
    next_id: AtomicU64,
    /// The command's answer to the first `initialize`, given to every later one.
    opened: OnceCell<Value>,
    initialized_sent: AtomicBool,
    gone: AtomicBool,
    port: u16,
}

/// Runs the command behind a loopback port until it ends; answers its exit code.
pub async fn run(asked: Asked) -> Result<i32, String> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], asked.port)))
        .await
        .map_err(|e| format!("cannot listen on 127.0.0.1:{}: {e}", asked.port))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let mut child = command(&asked.command)?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", asked.command[0]))?;
    let stdin = child.stdin.take().ok_or("the command has no stdin")?;
    let stdout = child.stdout.take().ok_or("the command has no stdout")?;
    let wrapped = Arc::new(Wrapped {
        stdin: tokio::sync::Mutex::new(stdin),
        waiting: Mutex::default(),
        next_id: AtomicU64::new(1),
        opened: OnceCell::new(),
        initialized_sent: AtomicBool::new(false),
        gone: AtomicBool::new(false),
        port,
    });
    eprintln!("rustdesk --mcp: {} serves MCP at http://127.0.0.1:{port}/mcp", asked.command.join(" "));
    tokio::spawn(wrapped.clone().read(stdout));
    tokio::spawn(wrapped.clone().serve(listener));
    let status = child.wait().await.map_err(|e| e.to_string())?;
    wrapped.gone.store(true, Ordering::SeqCst);
    wrapped.waiting.lock().unwrap_or_else(|p| p.into_inner()).clear();
    let code = status.code().unwrap_or(1);
    eprintln!("rustdesk --mcp: {} ended with status {code}", asked.command[0]);
    Ok(code)
}

#[cfg(not(windows))]
fn command(words: &[String]) -> Result<Command, String> {
    let mut command = Command::new(&words[0]);
    command.args(&words[1..]);
    Ok(command)
}

/// Windows starts only an .exe by its bare name; `npx` and its like are .cmd files, found here through PATH and PATHEXT.
#[cfg(windows)]
fn command(words: &[String]) -> Result<Command, String> {
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let named = Path::new(&words[0]);
    let found: PathBuf = if named.extension().is_some() || named.components().count() > 1 {
        named.to_path_buf()
    } else {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        let dirs = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&dirs)
            .flat_map(|dir| exts.split(';').filter(|e| !e.is_empty()).map(move |ext| dir.join(format!("{}{ext}", words[0]))))
            .find(|p| p.is_file())
            .ok_or_else(|| format!("{} is not on PATH", words[0]))?
    };
    let script = found.extension().map_or(false, |e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
    let mut command = if script {
        let mut c = Command::new("cmd.exe");
        c.arg("/d").arg("/c").arg(&found);
        c
    } else {
        Command::new(&found)
    };
    command.args(&words[1..]).creation_flags(CREATE_NO_WINDOW);
    Ok(command)
}

impl Wrapped {
    async fn write(&self, message: &Value) -> Result<(), String> {
        let mut line = message.to_string();
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await.map_err(|e| format!("the server's stdin is closed: {e}"))?;
        stdin.flush().await.map_err(|e| e.to_string())
    }

    /// Every line the command prints: answers go to whoever waits on them; its own requests are answered here.
    async fn read(self: Arc<Self>, stdout: tokio::process::ChildStdout) {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                if !line.trim().is_empty() {
                    eprintln!("{line}");
                }
                continue;
            };
            let id = message.get("id").cloned();
            match (message.get("method").and_then(Value::as_str), id) {
                (None, Some(id)) => {
                    let waiter = id.as_u64().and_then(|n| self.waiting.lock().unwrap_or_else(|p| p.into_inner()).remove(&n));
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(message);
                    }
                }
                (Some("ping"), Some(id)) => {
                    let _ = self.write(&json!({"jsonrpc": "2.0", "id": id, "result": {}})).await;
                }
                (Some(method), Some(id)) => {
                    let words = format!("{method}: no client to ask; rustdesk --mcp carries only the client's requests");
                    let _ = self.write(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": words}})).await;
                }
                (_, None) => {}
            }
        }
        self.gone.store(true, Ordering::SeqCst);
        self.waiting.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    /// One request through to the command, under an id of the wrapper's; its answer with the client's id put back.
    async fn ask(&self, mut request: Value) -> Value {
        let theirs = request.get("id").cloned().unwrap_or(Value::Null);
        let ours = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (send, answered) = oneshot::channel();
        self.waiting.lock().unwrap_or_else(|p| p.into_inner()).insert(ours, send);
        request["id"] = json!(ours);
        if self.gone.load(Ordering::SeqCst) {
            return fault(&theirs, "the server has ended");
        }
        if let Err(e) = self.write(&request).await {
            self.waiting.lock().unwrap_or_else(|p| p.into_inner()).remove(&ours);
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

    async fn serve(self: Arc<Self>, listener: TcpListener) {
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            let wrapped = self.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let wrapped = wrapped.clone();
                    async move { Ok::<_, Infallible>(wrapped.route(req).await) }
                });
                let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
            });
        }
    }

    async fn route(&self, request: Request<Incoming>) -> Response<Body> {
        if let Some(refused) = self.foreign(&request) {
            return text(StatusCode::FORBIDDEN, refused);
        }
        if request.uri().path() != "/mcp" {
            return text(StatusCode::NOT_FOUND, "MCP is at /mcp");
        }
        if request.method() != hyper::Method::POST {
            return text(StatusCode::METHOD_NOT_ALLOWED, "this server has no stream of its own; POST to /mcp");
        }
        let body = match Limited::new(request.into_body(), BODY_CAP).collect().await {
            Ok(body) => body.to_bytes(),
            Err(e) => return text(StatusCode::PAYLOAD_TOO_LARGE, &format!("cannot read the message: {e}")),
        };
        let Ok(message) = serde_json::from_slice::<Value>(&body) else {
            return json_answer(StatusCode::BAD_REQUEST, &fault(&Value::Null, "the body is not one JSON-RPC message"));
        };
        if !message.is_object() {
            return json_answer(StatusCode::BAD_REQUEST, &fault(&Value::Null, "send one JSON-RPC message, not a batch"));
        }
        let method = message.get("method").and_then(Value::as_str).map(str::to_string);
        if message.get("id").is_none() || method.is_none() {
            // A notification, or the client's answer to a request it never got: passed on, nothing to answer.
            let first_initialized = method.as_deref() == Some("notifications/initialized") && !self.initialized_sent.swap(true, Ordering::SeqCst);
            if method.as_deref() != Some("notifications/initialized") || first_initialized {
                if let Err(e) = self.write(&message).await {
                    return text(StatusCode::BAD_GATEWAY, &e);
                }
            }
            return Response::builder().status(StatusCode::ACCEPTED).body(Full::new(Bytes::new())).unwrap_or_default();
        }
        let answer = if method.as_deref() == Some("initialize") { self.initialize(message).await } else { self.ask(message).await };
        json_answer(StatusCode::OK, &answer)
    }

    /// A browser page reaching loopback through DNS rebinding names its own host; only this port's loopback names pass.
    fn foreign(&self, request: &Request<Incoming>) -> Option<&'static str> {
        let host = request.headers().get("host").and_then(|h| h.to_str().ok()).unwrap_or("");
        let ours = [format!("127.0.0.1:{}", self.port), format!("localhost:{}", self.port)];
        if !ours.iter().any(|o| o == host) {
            return Some("only 127.0.0.1 and localhost on this port are served");
        }
        if let Some(origin) = request.headers().get("origin").and_then(|h| h.to_str().ok()) {
            let local = ["http://127.0.0.1", "http://localhost"].iter().any(|o| origin == *o || origin.starts_with(&format!("{o}:")));
            if !local {
                return Some("a web page from another origin is not served");
            }
        }
        None
    }
}

fn fault(id: &Value, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": message}})
}

fn text(status: StatusCode, body: &str) -> Response<Body> {
    Response::builder().status(status).body(Full::new(Bytes::from(body.to_string()))).unwrap_or_default()
}

fn json_answer(status: StatusCode, body: &Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn the_command_follows_the_dashes() {
        assert_eq!(asked(&words("-- npx -y some-mcp --port 9")).unwrap(), Asked { port: 0, command: words("npx -y some-mcp --port 9") });
        assert_eq!(asked(&words("--port 8123 -- uvx blender-mcp")).unwrap(), Asked { port: 8123, command: words("uvx blender-mcp") });
        assert_eq!(asked(&words("uvx blender-mcp")).unwrap().command, words("uvx blender-mcp"));
        assert!(asked(&words("--port x -- a")).is_err());
        assert!(asked(&words("--")).is_err());
        assert!(asked(&[]).is_err());
    }
}
