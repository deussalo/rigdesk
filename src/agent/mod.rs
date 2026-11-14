//! RigDesk's agent API host (Windows and Linux). The pure share/holder logic, the event log and the HTTP surface live in the
//! `rigdesk-agent` crate; this module fills its one seam -- the [`Desk`] of capture, input, clipboard and shell -- and
//! starts the server inside the user-session `--server` process, the one that owns capture and input.
//!
//! The holder state lives here, on the controlled side, so it holds even when Rig is down. Viewer and local input
//! take control back from the agent through [`human_took_over`]; the viewer mints links through [`mint_link`] and
//! drives block/hand-back through the functions below. Nothing here reaches the network except the bound server.

#![cfg(any(target_os = "windows", target_os = "linux"))]

use std::{path::PathBuf, sync::Arc};

use hbb_common::{log, tokio};
use rigdesk_agent::{Events, Journal, Minted, Shared, Standing};

mod capture;
mod desk;
#[cfg(target_os = "windows")]
pub mod enroll;
mod input;
mod shell;
pub mod viewer;

/// The agent conn id. handle_mouse/handle_key key their per-connection state by it; it is distinct from any real
/// peer's so agent input and a viewer's never share tracking state.
pub const AGENT_CONN_ID: i32 = -777;

/// How many events the ring keeps, and how large the action log grows before it rotates.
const EVENT_RING: usize = 512;
const JOURNAL_BYTES: u64 = 8 << 20;

struct Host {
    shared: Arc<Shared>,
    journal: Arc<Journal>,
}

lazy_static::lazy_static! {
    static ref HOST: Host = {
        let journal = match data_dir() {
            Some(dir) => Journal::open(dir.join("actions.jsonl"), JOURNAL_BYTES),
            None => {
                log::warn!("agent: no data dir; the action log is kept in the process log only");
                Journal::memory()
            }
        };
        Host { shared: Arc::new(Shared::new(Events::new(EVENT_RING))), journal: Arc::new(journal) }
    };
}

fn data_dir() -> Option<PathBuf> {
    let dir = hbb_common::config::Config::path("");
    let dir = if dir.as_os_str().is_empty() { None } else { Some(dir) };
    dir.map(|d| d.join("agent"))
}

/// The share state, for the viewer-facing hooks and the server.
pub fn shared() -> &'static Arc<Shared> {
    &HOST.shared
}

/// Read the bearer token for the agent API from its 0600 file next to the config. Absent means the API does not start.
fn bearer() -> Option<String> {
    let path = data_dir()?.join("api.token");
    let token = std::fs::read_to_string(&path).ok()?;
    let token = token.trim().to_string();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// The API's loopback port. `tailscale serve` publishes it on the tailnet; only the tailnet reaches it.
const API_PORT: u16 = 21119;

/// Start the agent API in the current (user-session server) process, once. A missing token file leaves it off, so a
/// PC that was never enrolled for the agent serves nothing.
pub fn start() {
    let Some(bearer) = bearer() else {
        log::info!("agent: no api.token; the agent API is off on this PC");
        return;
    };
    let host = &*HOST;
    let desk: Arc<dyn rigdesk_agent::Desk> = Arc::new(desk::PcDesk::new());
    // The MCP servers already running on the PC come with the share; RustDesk's own ports and this API are never asked.
    let found = rigdesk_agent::Found::new([API_PORT].into_iter().chain(21114..=21119));
    let api = rigdesk_agent::Api::new(host.shared.clone(), desk, host.journal.clone(), &bearer, Some(found));
    tokio::spawn(async move {
        match api.serve(API_PORT).await {
            Ok(port) => log::info!("agent API listening on 127.0.0.1:{port}"),
            Err(e) => log::error!("agent API failed to bind {API_PORT}: {e}"),
        }
    });
}

/// Mint a share link for the viewer's "Share with agent". The caller shows it as rigdesk://<host>/<grant>#<secret>.
pub fn mint_link() -> Minted {
    HOST.shared.mint()
}

/// Viewer or local input arrived: the human holds input again, before the agent's next action. No-op unless the
/// agent currently holds it. Called from the input path for every human-driven event.
pub fn human_took_over(by: &str) -> bool {
    HOST.shared.human_input(by)
}

pub fn hand_to_agent() -> bool {
    HOST.shared.hand_to_agent()
}

pub fn block_agent() -> bool {
    HOST.shared.block()
}

pub fn unblock_agent() -> bool {
    HOST.shared.unblock()
}

pub fn revoke(reason: &str) -> bool {
    HOST.shared.revoke(reason)
}

/// A human message from the viewer's message box, with the PNGs (full frame or clip, annotated) it carried.
pub fn human_message(text: String, pngs: Vec<Vec<u8>>) -> u64 {
    HOST.shared.events.message(text, pngs)
}

pub fn standing() -> Standing {
    HOST.shared.standing()
}
