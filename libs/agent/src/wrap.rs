//! `rustdesk --mcp [--port N] -- <command> [args…]`: puts an MCP server that a client would start and talk to over its
//! stdin and stdout on a loopback port, as streamable HTTP at `/mcp`, where RigDesk's scan finds it like any server that
//! listens. The agent or the human runs it in the shell; it lasts as long as the command does and ends with its status.
//!
//! One command serves every client, through one [`Relay`] session (RigDesk's scan is a client too). Lines it prints that
//! are not JSON go to stderr, so a server that logs to stdout still works.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;

use crate::relay::{fault, Relay, Upstream};

/// A client's message larger than this is refused.
const BODY_CAP: usize = 16 << 20;

type Body = Full<Bytes>;

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
    relay: Arc<Relay>,
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
    let relay = Relay::new(Upstream::Stdin(tokio::sync::Mutex::new(stdin)));
    let wrapped = Arc::new(Wrapped { relay: relay.clone(), port });
    eprintln!("rustdesk --mcp: {} serves MCP at http://127.0.0.1:{port}/mcp", asked.command.join(" "));
    tokio::spawn(read(relay.clone(), stdout));
    tokio::spawn(wrapped.serve(listener));
    let status = child.wait().await.map_err(|e| e.to_string())?;
    relay.end();
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

/// Every line the command prints goes to the relay; one that is not JSON goes to stderr.
async fn read(relay: Arc<Relay>, stdout: tokio::process::ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match serde_json::from_str::<Value>(&line) {
            Ok(message) => relay.heard(message).await,
            Err(_) if !line.trim().is_empty() => eprintln!("{line}"),
            Err(_) => {}
        }
    }
    relay.end();
}

impl Wrapped {
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
        match self.relay.handle(message).await {
            Ok(Some(answer)) => json_answer(StatusCode::OK, &answer),
            Ok(None) => Response::builder().status(StatusCode::ACCEPTED).body(Full::new(Bytes::new())).unwrap_or_default(),
            Err(e) => text(StatusCode::BAD_GATEWAY, &e),
        }
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
