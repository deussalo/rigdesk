//! The agent API over a stand-in desktop, for exercising a client such as the Rig plugin off Windows.
//!
//!   cargo run -p rigdesk-agent --example fake_desk -- <port> <bearer>
//!
//! It prints `listening <port>`, then reads the viewer's side from stdin, one command a line:
//! `mint` (prints `link <grant> <secret>`), `hand`, `human`, `block`, `unblock`, `revoke`, `msg <text>` (sent with one image).

use std::io::BufRead;
use std::sync::Arc;

use base64::Engine;
use rigdesk_agent::{Api, Called, Content, Desk, Events, Journal, Shared, ToolSpec};
use serde_json::{json, Value};

const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";

fn pixel() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(PIXEL).unwrap_or_default()
}

struct FakeDesk;

impl Desk for FakeDesk {
    fn tools(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "screenshot",
                description: "The primary display as a PNG.",
                input_schema: json!({"type": "object", "properties": {}}),
                read_only: true,
            },
            ToolSpec {
                name: "type",
                description: "Type text where the focus is.",
                input_schema: json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
                read_only: false,
            },
        ]
    }

    fn call(&self, tool: &str, args: &Value) -> Result<Called, String> {
        match tool {
            "screenshot" => Ok(vec![Content::Png(pixel())].into()),
            "type" => Ok(vec![Content::Text(format!("typed {}", args.get("text").and_then(Value::as_str).unwrap_or("")))].into()),
            other => Err(format!("no tool {other}")),
        }
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let bearer = args.next().unwrap_or_else(|| "fake".into());
    let shared = Arc::new(Shared::new(Events::new(256)));
    let api = Api::new(shared.clone(), Arc::new(FakeDesk), Arc::new(Journal::memory()), &bearer, Some(rigdesk_agent::Found::new([port])));
    let bound = api.serve(port).await?;
    println!("listening {bound}");
    let viewer = shared.clone();
    tokio::task::spawn_blocking(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let (verb, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
            let said = match verb {
                "mint" => {
                    let minted = viewer.mint();
                    format!("link {} {}", minted.grant, minted.secret)
                }
                "hand" => format!("ok {}", viewer.hand_to_agent()),
                "human" => format!("ok {}", viewer.human_input("viewer input")),
                "block" => format!("ok {}", viewer.block()),
                "unblock" => format!("ok {}", viewer.unblock()),
                "revoke" => format!("ok {}", viewer.revoke("the viewer revoked the share")),
                "msg" => format!("ok {}", viewer.events.message(rest.to_string(), vec![pixel()])),
                _ => format!("unknown {verb}"),
            };
            println!("{said}");
        }
    })
    .await
    .ok();
    Ok(())
}
