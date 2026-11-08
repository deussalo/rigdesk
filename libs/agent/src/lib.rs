//! RigDesk's agent API. A human shares their session with one Rig agent through a link; that agent then drives the
//! desktop with Windows-MCP-like tools over MCP, while the human can take input back at any moment or block it outright.
//! This crate holds what decides who may act, and serves it; the tools themselves are the host's, behind [`Desk`].

pub mod desk;
pub mod events;
pub mod http;
pub mod journal;
pub mod mcp;
pub mod session;

pub use desk::{Content, Desk, ToolSpec};
pub use events::{Event, EventKind, Events};
pub use session::{Holder, Owner, Refusal, Shared};

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
