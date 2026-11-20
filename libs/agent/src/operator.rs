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

    /// Whether `offer` shows the person at the PC a prompt, as `/health` says it.
    fn offers(&self) -> bool {
        false
    }

    /// A RustDesk login to the PC that has actually succeeded, made now unless one is up, for a host that logs in to the PC
    /// rather than being it: Rig asks for it on the operator's Accept, before the agent is told it joined. It blocks while
    /// it dials.
    fn log_in(&self) -> Result<(), NoLogin> {
        Err(NoLogin::Failed("this host is the PC itself, so there is no login to make".into()))
    }
}

/// Why no RustDesk login to the PC stands (`Switch::log_in`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoLogin {
    /// Rig keeps no password for the PC and the person there has allowed no connection.
    NoPassword,
    /// The PC refused the saved password, or did not answer, in its own words.
    Failed(String),
}

impl NoLogin {
    pub fn code(&self) -> &'static str {
        match self {
            NoLogin::NoPassword => "no_password",
            NoLogin::Failed(_) => "login_failed",
        }
    }

    pub fn words(&self) -> String {
        match self {
            NoLogin::NoPassword => "Rig keeps no password for this PC".into(),
            NoLogin::Failed(why) => why.clone(),
        }
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

const PERSON_ALLOWED: &str = "\tperson";

/// The session as the PC's tray reads it over IPC: "<agent id>\n<title>", the id marked when the person at the PC allowed it.
pub fn session_line(agent_id: &str, title: &str, person_allowed: bool) -> String {
    format!("{agent_id}{}\n{title}", if person_allowed { PERSON_ALLOWED } else { "" })
}

/// A `session_line`'s agent id, title, and whether the person at the PC allowed the session.
pub fn parse_session(text: &str) -> Option<(&str, &str, bool)> {
    let (id, title) = text.split_once('\n')?;
    Some(match id.strip_suffix(PERSON_ALLOWED) {
        Some(id) => (id, title, true),
        None => (id, title, false),
    })
}

/// The title and text of the notification that a session started from Rig has begun.
pub fn words(title: &str, person_allowed: bool) -> (String, String) {
    let allowed = if person_allowed { "The person at this PC allowed it." } else { "The operator approved it in Rig." };
    (
        format!("Rig agent {title} is using this computer"),
        format!("{allowed} Your own mouse and keyboard still go first, and every action is logged. End it from the RigDesk tray icon."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notification_says_who_allowed_the_session() {
        assert!(words("Helper", false).1.starts_with("The operator approved it in Rig."));
        let (summary, body) = words("Helper", true);
        assert_eq!(summary, "Rig agent Helper is using this computer");
        assert!(body.starts_with("The person at this PC allowed it."));
        assert!(!body.contains("operator"));
    }

    #[test]
    fn the_tray_reads_who_allowed_the_session() {
        assert_eq!(parse_session(&session_line("a1", "Helper", false)), Some(("a1", "Helper", false)));
        assert_eq!(parse_session(&session_line("a1", "Helper", true)), Some(("a1", "Helper", true)));
        assert_eq!(parse_session(""), None);
    }
}
