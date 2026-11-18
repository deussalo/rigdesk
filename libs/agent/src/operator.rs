//! Sessions the operator starts from Rig, with no share link: Rig's bearer asks `POST /join` for an agent once the
//! operator has accepted that agent's request in Rig. The PC's own switch admits it, so a fault in Rig cannot open a PC
//! that is switched off; the share admits it only while no agent holds the session and that agent is not blocked.

use serde_json::{json, Value};

use crate::session::Owner;

/// Why this PC takes no operator-started session now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Closed {
    /// The PC's switch is off, with how an administrator turns it on.
    Off(String),
    /// Paused from the PIN-locked settings on the PC.
    Paused,
}

/// The PC's switch for operator-started sessions, filled by the host.
pub trait Switch: Send + Sync {
    fn open(&self) -> Result<(), Closed>;

    /// Asks the person at the PC, while the operator's ask stands in Rig, whether the agent may join: the host shows its own
    /// accept prompt and pushes their answer as an `offer` event with the offer's ask and nonce. Err says why it shows none.
    fn offer(&self, _offer: Offer) -> Result<(), String> {
        Err("this PC's RigDesk shows no prompt for an agent's request".into())
    }

    /// Takes the prompt for this ask away; false when none shows.
    fn offer_end(&self, _ask: &str) -> bool {
        false
    }
}

/// An agent's request shown at the PC: `nonce` is Rig's, carried back with the answer, so only Rig's own ask is settled by it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub owner: Owner,
    pub ask: String,
    pub nonce: String,
}

/// The offer in an `/offer` body: `{agent, ask, nonce}`.
pub fn offer_request(body: &Value) -> Result<Offer, String> {
    let (owner, ask) = request(body)?;
    let nonce = body.get("nonce").and_then(Value::as_str).filter(|nonce| nonce.len() >= 16).ok_or("offer wants a nonce of 16 characters or more")?;
    if ask.is_empty() {
        return Err("offer wants the ask it is for".into());
    }
    Ok(Offer { owner, ask, nonce: nonce.to_string() })
}

/// Why a `/join` was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    Closed(Closed),
    Held(Owner),
    Blocked { since_ms: u64 },
}

impl Refused {
    pub fn code(&self) -> &'static str {
        match self {
            Refused::Closed(Closed::Off(_)) => "join_off",
            Refused::Closed(Closed::Paused) => "join_paused",
            Refused::Held(_) => "held",
            Refused::Blocked { .. } => "blocked",
        }
    }

    pub fn words(&self) -> String {
        match self {
            Refused::Closed(Closed::Off(how)) => format!("this PC takes no sessions started from Rig: {how}"),
            Refused::Closed(Closed::Paused) => "the person at this PC paused sessions started from Rig, in RigDesk's PIN-locked settings".into(),
            Refused::Held(owner) => format!("{} holds this PC's session now; it frees when they leave or the share ends", owner.title),
            Refused::Blocked { since_ms } => crate::session::Refusal::Blocked { since_ms: *since_ms }.words(),
        }
    }
}

/// The switch as `/health` says it: `{open, code, words}`.
pub fn said(switch: &dyn Switch) -> Value {
    match switch.open() {
        Ok(()) => json!({"open": true, "code": null, "words": null}),
        Err(closed) => {
            let refused = Refused::Closed(closed);
            json!({"open": false, "code": refused.code(), "words": refused.words()})
        }
    }
}

/// The joining agent and the operator's ask, from a `/join` body.
pub fn request(body: &Value) -> Result<(Owner, String), String> {
    let agent = body.get("agent").ok_or("join wants agent: {id, title, graph}")?;
    let owner = Owner {
        agent_id: agent.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).ok_or("join wants agent.id")?.to_string(),
        title: agent.get("title").and_then(Value::as_str).unwrap_or("agent").to_string(),
        graph: agent.get("graph").and_then(Value::as_str).unwrap_or("").to_string(),
    };
    let ask = body.get("ask").and_then(Value::as_str).unwrap_or("").to_string();
    Ok((owner, ask))
}
