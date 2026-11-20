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
    /// it dials. `signs_for` is the agent it is for and the ask the operator accepted, empty for a standing grant: a PC that
    /// asks for Rig's proof is told who joined and how.
    fn log_in(&self, _signs_for: Option<(Owner, String)>) -> Result<(), NoLogin> {
        Err(NoLogin::Failed("this host is the PC itself, so there is no login to make".into()))
    }
}

/// Why no RustDesk login to the PC stands (`Switch::log_in`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoLogin {
    /// Rig keeps no password for the PC and the person there has allowed no connection.
    NoPassword,
    /// Nothing at the PC took the connection, or it closed before the PC asked for a login, so no login was tried and no
    /// password sent. `at` is the address or ID Rig dialed; `why` says what failed there and what to start where.
    Unreached { at: String, why: String },
    /// The PC answered the login with a refusal, in its own words.
    Refused(String),
    /// The login was asked for and did not finish, in its words.
    Failed(String),
}

impl NoLogin {
    pub fn code(&self) -> &'static str {
        match self {
            NoLogin::NoPassword => "no_password",
            NoLogin::Unreached { .. } => "unreachable",
            NoLogin::Refused(_) => "login_refused",
            NoLogin::Failed(_) => "login_failed",
        }
    }

    pub fn words(&self) -> String {
        match self {
            NoLogin::NoPassword => "Rig keeps no password for this PC".into(),
            NoLogin::Unreached { why, .. } | NoLogin::Refused(why) | NoLogin::Failed(why) => why.clone(),
        }
    }

    /// Where Rig dialed, when nothing there took the connection.
    pub fn at(&self) -> Option<&str> {
        match self {
            NoLogin::Unreached { at, .. } => Some(at),
            _ => None,
        }
    }
}

/// An agent's request shown at the PC: `nonce` is Rig's, carried back with the answer, so only Rig's own ask is settled by it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub owner: Owner,
    pub ask: String,
    pub nonce: String,
    /// The person already accepted this ask on Rig Assistant's notification: the knock names it, so the phone lets it in
    /// with no second Allow.
    pub accepted: bool,
}

/// The offer in an `/offer` body: `{agent, ask, nonce, accepted?}`.
pub fn offer_request(body: &Value) -> Result<Offer, String> {
    let (owner, ask) = request(body)?;
    let nonce = body.get("nonce").and_then(Value::as_str).filter(|nonce| nonce.len() >= 16).ok_or("offer wants a nonce of 16 characters or more")?;
    if ask.is_empty() {
        return Err("offer wants the ask it is for".into());
    }
    let accepted = match body.get("accepted") {
        None => false,
        Some(Value::Bool(accepted)) => *accepted,
        Some(_) => return Err("offer's accepted is true or false".into()),
    };
    Ok(Offer { owner, ask, nonce: nonce.to_string(), accepted })
}

/// The agent a `/login` is for and the operator's ask, from its body when Rig sends one: `{agent, ask}` as `/join`'s.
pub fn login_request(body: Option<&Value>) -> Option<(Owner, String)> {
    body.and_then(|body| request(body).ok())
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

/// How an operator-started session was let in, as the PC's notice says it and the journal keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinedBy {
    /// The operator's Join once in Rig, on a PC whose enrollment turned on sessions from Rig (`-AgentJoin on`).
    RigAccept,
    /// A standing grant: the operator's Always allow, or the PC assigned to the agent in Rig.
    StandingGrant,
    /// The person at the PC allowed the agent's request on the PC's own prompt.
    PersonAllowed,
}

impl JoinedBy {
    /// The `how` of a `/join` body, and of the tray's session line.
    pub fn code(self) -> &'static str {
        match self {
            JoinedBy::RigAccept => "accept",
            JoinedBy::StandingGrant => "standing_grant",
            JoinedBy::PersonAllowed => "person",
        }
    }

    pub fn parse(code: &str) -> Option<JoinedBy> {
        [JoinedBy::RigAccept, JoinedBy::StandingGrant, JoinedBy::PersonAllowed].into_iter().find(|how| how.code() == code)
    }

    /// The journal's `by` of the join.
    pub fn by(self) -> &'static str {
        match self {
            JoinedBy::RigAccept => "Rig, operator-approved",
            JoinedBy::StandingGrant => "Rig, standing grant",
            JoinedBy::PersonAllowed => "Rig, allowed by the person at the PC",
        }
    }
}

/// How a `/join` body says the session was let in.
pub fn joined_by(body: &Value) -> Result<JoinedBy, String> {
    let how = body.get("how").and_then(Value::as_str).unwrap_or("");
    JoinedBy::parse(how).ok_or_else(|| format!("join wants how: accept, standing_grant or person, not {how:?}; Rig's rigdesk plugin is older than this PC's RigDesk"))
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

/// The session as the PC's tray reads it over IPC: "<agent id>\t<how>\n<title>".
pub fn session_line(agent_id: &str, title: &str, how: JoinedBy) -> String {
    format!("{agent_id}\t{}\n{title}", how.code())
}

/// A `session_line`'s agent id, title, and how the session was let in.
pub fn parse_session(text: &str) -> Option<(&str, &str, JoinedBy)> {
    let (head, title) = text.split_once('\n')?;
    let (id, how) = head.split_once('\t')?;
    Some((id, title, JoinedBy::parse(how)?))
}

/// What the tray's end entry says.
pub fn disconnect(title: &str) -> String {
    format!("Disconnect {title}")
}

/// The title and text of the notification that a session started from Rig has begun; the title is also the tray's line.
pub fn words(title: &str, how: JoinedBy) -> (String, String) {
    let joined = match how {
        JoinedBy::RigAccept => "your Rig Accept, let in by this PC's -AgentJoin",
        JoinedBy::StandingGrant => "Always allow in Rig, let in by -AgentJoin",
        JoinedBy::PersonAllowed => "your Allow",
    };
    (
        format!("{title} (Rig agent) joined: {joined}"),
        format!("Your own mouse and keyboard still go first, and every action is logged. Disconnect is in the tray: {} on the RigDesk icon.", disconnect(title)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notification_says_how_the_agent_got_in_and_where_disconnect_is() {
        assert_eq!(words("Helper", JoinedBy::RigAccept).0, "Helper (Rig agent) joined: your Rig Accept, let in by this PC's -AgentJoin");
        assert_eq!(words("Helper", JoinedBy::StandingGrant).0, "Helper (Rig agent) joined: Always allow in Rig, let in by -AgentJoin");
        let (summary, body) = words("Helper", JoinedBy::PersonAllowed);
        assert_eq!(summary, "Helper (Rig agent) joined: your Allow");
        assert!(body.contains("Disconnect is in the tray: Disconnect Helper"), "{body}");
        assert!(!body.contains("Dismiss"));
    }

    #[test]
    fn an_offer_says_whether_the_person_accepted_its_ask_already() {
        let body = serde_json::json!({"agent": {"id": "a1", "title": "Helper", "graph": "g"}, "ask": "desk_1", "nonce": "0123456789abcdef"});
        assert!(!offer_request(&body).unwrap().accepted);
        let mut accepted = body.clone();
        accepted["accepted"] = true.into();
        assert!(offer_request(&accepted).unwrap().accepted);
        accepted["accepted"] = "yes".into();
        assert!(offer_request(&accepted).is_err());
    }

    #[test]
    fn the_tray_reads_how_the_session_was_let_in() {
        for how in [JoinedBy::RigAccept, JoinedBy::StandingGrant, JoinedBy::PersonAllowed] {
            assert_eq!(parse_session(&session_line("a1", "Helper\twith tab", how)), Some(("a1", "Helper\twith tab", how)));
        }
        assert_eq!(parse_session(""), None);
        assert_eq!(parse_session("a1\n"), None, "a line with no how is no session");
    }

    #[test]
    fn a_join_says_how_or_is_refused() {
        assert_eq!(joined_by(&json!({"how": "standing_grant"})), Ok(JoinedBy::StandingGrant));
        assert_eq!(joined_by(&json!({"how": "person"})), Ok(JoinedBy::PersonAllowed));
        assert!(joined_by(&json!({"answered_by": "person"})).unwrap_err().contains("older than this PC"));
        assert!(joined_by(&json!({"how": "always"})).is_err());
    }
}
