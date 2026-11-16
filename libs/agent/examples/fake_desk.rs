//! The agent API over a stand-in desktop, for exercising a client such as the Rig plugin off Windows.
//!
//!   cargo run -p rigdesk-agent --example fake_desk -- <port> <bearer>
//!
//! It prints `listening <port>`, then reads the viewer's side from stdin, one command a line:
//! `mint` (prints `link <grant> <secret>`), `hand`, `human`, `block`, `unblock`, `revoke`, `msg <text>` (sent with one image),
//! `shell on|off` (the PC's agent-shell switch), `pause on|off` (the person's pause), `reboot` (a new boot id), `sample` (a health sample now).
//!
//! Its management lane runs commands through `sh` as the user running it. It keeps its journal, jobs and health samples in
//! `$FAKE_DESK_DIR` (a fresh temp directory if unset), so they outlive a restart, and samples every `$FAKE_DESK_HEALTH_S`
//! seconds (600 if unset) from `$FAKE_DESK_FIGURES`, a file of figures (a Windows PC's if unset).

use std::io::BufRead;
use std::sync::Arc;

use base64::Engine;
use rigdesk_agent::{
    health::{self, Collector, Samples},
    manage::{Lane, ShRunner},
    Api, Called, Content, Desk, Events, Journal, Shared, ToolSpec,
};
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
    let dir = std::env::var_os("FAKE_DESK_DIR").map(std::path::PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join(format!("fake-desk-{:08x}", rand::random::<u32>())));
    let journal = Arc::new(Journal::open(dir.join("actions.jsonl"), 8 << 20));
    let api = Api::new(shared.clone(), Arc::new(FakeDesk), journal.clone(), &bearer, Some(rigdesk_agent::Found::new([port], true)));
    let runner = Arc::new(ShRunner::new());
    let lane = Lane::new(runner.clone(), dir.join("shell"), shared.clone(), journal);
    let figures = match std::env::var_os("FAKE_DESK_FIGURES") {
        Some(path) => std::fs::canonicalize(path)?.display().to_string(),
        None => {
            let path = dir.join("figures.json");
            std::fs::write(&path, include_str!("../tests/fixtures/figures-windows.json"))?;
            path.display().to_string()
        }
    };
    let script: &'static str = Box::leak(format!("cat '{figures}'").into_boxed_str());
    let interval = std::env::var("FAKE_DESK_HEALTH_S").ok().and_then(|s| s.parse().ok()).map_or(health::INTERVAL, std::time::Duration::from_secs);
    let samples = Arc::new(Samples::open(Some(dir.join("health.jsonl"))));
    let collector = Arc::new(Collector { lane: lane.clone(), shared: shared.clone(), samples, script, dir: dir.join("health"), rigdesk: "fake".into(), interval });
    api.manage(lane, Some(collector.clone()));
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
                "shell" => {
                    runner.on.store(rest == "on", std::sync::atomic::Ordering::SeqCst);
                    format!("ok {}", rest == "on")
                }
                "pause" => {
                    runner.paused.store(rest == "on", std::sync::atomic::Ordering::SeqCst);
                    format!("ok {}", rest == "on")
                }
                "reboot" => {
                    *runner.boot.lock().unwrap() = format!("boot-{:08x}", rand::random::<u32>());
                    "ok true".into()
                }
                "sample" => format!("ok {}", collector.take().id),
                _ => format!("unknown {verb}"),
            };
            println!("{said}");
        }
    })
    .await
    .ok();
    Ok(())
}
