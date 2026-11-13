//! MCP over JSON-RPC: initialize, ping, tools/list and tools/call. A refused call comes back as a tool result with
//! isError set, whose text says why, so the agent reads the reason rather than a transport failure.

use std::sync::Arc;

use base64::Engine;
use serde_json::{json, Value};

use crate::desk::{Content, Desk};
#[cfg(test)]
use crate::desk::Called;
use crate::events::EventKind;
use crate::journal::{Entry, Journal};
use crate::session::{Holder, Shared};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

pub struct Mcp {
    pub shared: Arc<Shared>,
    pub desk: Arc<dyn Desk>,
    pub journal: Arc<Journal>,
}

fn reply(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn fault(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

pub fn tool_error(words: &str) -> Value {
    json!({"content": [{"type": "text", "text": words}], "isError": true})
}

fn contents(items: Vec<Content>) -> Value {
    let content: Vec<Value> = items
        .into_iter()
        .map(|c| match c {
            Content::Text(text) => json!({"type": "text", "text": text}),
            Content::Png(png) => json!({"type": "image", "mimeType": "image/png", "data": base64::engine::general_purpose::STANDARD.encode(png)}),
        })
        .collect();
    json!({"content": content, "isError": false})
}

impl Mcp {
    /// Handle one JSON-RPC request. `token` is the session token from X-Desk-Session, absent on an unclaimed call.
    /// Returns None for a notification, which has no reply.
    pub fn handle(&self, request: &Value, token: Option<&str>) -> Option<Value> {
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            return None;
        };
        let result = match method {
            "initialize" => reply(&id, json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "rigdesk", "version": env!("CARGO_PKG_VERSION")},
            })),
            "ping" => reply(&id, json!({})),
            "tools/list" => {
                let tools: Vec<Value> = self
                    .desk
                    .tools()
                    .into_iter()
                    .map(|t| json!({"name": t.name, "description": t.description, "inputSchema": t.input_schema, "annotations": {"readOnlyHint": t.read_only}}))
                    .collect();
                reply(&id, json!({"tools": tools}))
            }
            "tools/call" => self.call(&id, &params, token),
            other => fault(&id, -32601, &format!("method not found: {other}")),
        };
        Some(result)
    }

    fn call(&self, id: &Value, params: &Value, token: Option<&str>) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        let Some(token) = token else {
            return reply(id, tool_error(&crate::session::Refusal::NoSession.words()));
        };
        let read_only = self.desk.tools().iter().find(|t| t.name == name).map_or(false, |t| t.read_only);
        if !read_only {
            let standing = self.shared.standing();
            if let (Some(Holder::Agent), Some(since)) = (standing.holder, standing.since_ms) {
                if self.desk.local_input_after(since) {
                    self.shared.human_input("local input");
                }
            }
        }
        let owner = match self.shared.admit(token, read_only) {
            Ok(owner) => owner,
            Err(refusal) => {
                self.journal.record(&Entry::new(
                    self.shared.owner_of(token).map(|o| o.who()).unwrap_or_else(|_| "unknown agent".into()),
                    format!("call {name}"),
                    "refused",
                    json!({"reason": refusal.words()}),
                ));
                return reply(id, tool_error(&refusal.words()));
            }
        };
        let (outcome, iserror, result) = match self.desk.call(name, &args) {
            Ok(called) => (called.outcome, false, contents(called.contents)),
            Err(err) => (format!("error: {err}"), true, tool_error(&err)),
        };
        self.journal.record(&Entry::new(owner.who(), format!("call {name}"), outcome, json!({"args": args})));
        self.shared.events.transient(EventKind::Action {
            agent_title: owner.title.clone(),
            tool: name.to_string(),
            summary: summarize(name, &args),
            ok: !iserror,
            x: args.get("x").and_then(Value::as_i64).map(|v| v as i32),
            y: args.get("y").and_then(Value::as_i64).map(|v| v as i32),
        });
        reply(id, result)
    }
}

fn summarize(name: &str, args: &Value) -> String {
    match name {
        "type" => args.get("text").and_then(Value::as_str).map(|t| format!("type {t:?}")).unwrap_or_else(|| name.into()),
        "click" | "move" | "drag" => {
            let x = args.get("x").and_then(Value::as_i64).unwrap_or_default();
            let y = args.get("y").and_then(Value::as_i64).unwrap_or_default();
            format!("{name} {x},{y}")
        }
        "key" | "shortcut" => args.get("keys").or_else(|| args.get("key")).and_then(Value::as_str).map(|k| format!("{name} {k}")).unwrap_or_else(|| name.into()),
        "app" => args.get("name").and_then(Value::as_str).map(|a| format!("app {a}")).unwrap_or_else(|| name.into()),
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desk::ToolSpec;
    use crate::events::Events;
    use crate::session::Owner;

    struct FakeDesk;
    impl Desk for FakeDesk {
        fn tools(&self) -> Vec<ToolSpec> {
            vec![
                ToolSpec { name: "screenshot", description: "a frame", input_schema: json!({"type": "object"}), read_only: true },
                ToolSpec { name: "type", description: "types text", input_schema: json!({"type": "object"}), read_only: false },
            ]
        }
        fn call(&self, tool: &str, args: &Value) -> Result<Called, String> {
            match tool {
                "screenshot" => Ok(vec![Content::Png(vec![1, 2, 3])].into()),
                "type" => Ok(vec![Content::Text(format!("typed {}", args.get("text").and_then(Value::as_str).unwrap_or("")))].into()),
                other => Err(format!("no such tool: {other}")),
            }
        }
    }

    struct TouchedDesk;
    impl Desk for TouchedDesk {
        fn tools(&self) -> Vec<ToolSpec> {
            FakeDesk.tools()
        }
        fn call(&self, tool: &str, args: &Value) -> Result<Called, String> {
            FakeDesk.call(tool, args)
        }
        fn local_input_after(&self, _since_ms: u64) -> bool {
            true
        }
    }

    #[test]
    fn local_input_takes_input_back_before_the_agents_next_action() {
        let mcp = Mcp { shared: Arc::new(Shared::new(Events::new(64))), desk: Arc::new(TouchedDesk), journal: Arc::new(Journal::memory()) };
        let link = mcp.shared.mint();
        let token = mcp.shared.claim(&link.grant, &link.secret, Owner { agent_id: "a1".into(), title: "t".into(), graph: "g".into() }).unwrap();
        let typed = mcp.handle(&call("type", json!({"text": "hi"})), Some(&token)).unwrap();
        assert!(typed["result"]["content"][0]["text"].as_str().unwrap().contains("is using the PC"));
        assert_eq!(mcp.shared.standing().holder, Some(Holder::Human));
    }

    fn mcp() -> Mcp {
        Mcp { shared: Arc::new(Shared::new(Events::new(64))), desk: Arc::new(FakeDesk), journal: Arc::new(Journal::memory()) }
    }

    fn call(name: &str, args: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}})
    }

    #[test]
    fn a_call_without_a_session_is_a_tool_error_not_a_transport_error() {
        let mcp = mcp();
        let reply = mcp.handle(&call("screenshot", json!({})), None).unwrap();
        assert!(reply.get("error").is_none());
        let result = &reply["result"];
        assert_eq!(result["isError"], json!(true));
        assert!(result["content"][0]["text"].as_str().unwrap().contains("no session"));
    }

    #[test]
    fn the_human_holding_input_refuses_actions_but_not_reads() {
        let mcp = mcp();
        let link = mcp.shared.mint();
        let token = mcp.shared.claim(&link.grant, &link.secret, Owner { agent_id: "a1".into(), title: "t".into(), graph: "g".into() }).unwrap();
        assert!(mcp.shared.human_input("viewer"));
        let typed = mcp.handle(&call("type", json!({"text": "hi"})), Some(&token)).unwrap();
        assert_eq!(typed["result"]["isError"], json!(true));
        assert!(typed["result"]["content"][0]["text"].as_str().unwrap().contains("is using the PC"));
        let shot = mcp.handle(&call("screenshot", json!({})), Some(&token)).unwrap();
        assert_eq!(shot["result"]["isError"], json!(false));
        assert_eq!(shot["result"]["content"][0]["type"], json!("image"));
    }

    #[test]
    fn a_held_call_runs_and_shows_as_an_action() {
        let mcp = mcp();
        let link = mcp.shared.mint();
        let token = mcp.shared.claim(&link.grant, &link.secret, Owner { agent_id: "a1".into(), title: "front desk".into(), graph: "g".into() }).unwrap();
        let mut rx = mcp.shared.events.subscribe();
        let reply = mcp.handle(&call("type", json!({"text": "hi"})), Some(&token)).unwrap();
        assert_eq!(reply["result"]["content"][0]["text"], json!("typed hi"));
        let action = rx.try_recv().unwrap();
        assert!(matches!(action.kind, EventKind::Action { ok: true, .. }));
    }
}
