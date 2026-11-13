use std::sync::Mutex;

use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::events::{EventKind, Events};
use crate::now_ms;

/// How long the human's input must pause before input goes back to the agent by itself.
pub const QUIET_MS: u64 = 3_000;

/// Who may drive the desktop now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Holder {
    Human,
    Agent,
    Blocked,
}

/// The Rig agent a share link was given to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub agent_id: String,
    pub title: String,
    pub graph: String,
}

impl Owner {
    /// How the action log names this agent.
    pub fn who(&self) -> String {
        format!("{} ({})", self.title, self.agent_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    NoSession,
    WrongGrant,
    Blocked { since_ms: u64 },
    HumanHolds { since_ms: u64 },
}

impl Refusal {
    pub fn words(&self) -> String {
        match self {
            Refusal::NoSession => "no session: this desktop is not shared with you; ask the human for a share link and call desk/join".into(),
            Refusal::WrongGrant => "that share link is not valid here: it was already used, replaced or never minted; ask the human for a new one".into(),
            Refusal::Blocked { since_ms } => format!("the human blocked the agent at {}; nothing runs until they unblock it", clock(*since_ms)),
            Refusal::HumanHolds { since_ms } => format!(
                "the human is using the PC (since {}); reads still work, and actions work again once their input pauses for {} s: look again, then retry",
                clock(*since_ms),
                QUIET_MS / 1000
            ),
        }
    }
}

/// A moment as the agent reads it, in UTC: the PC's own zone is not known to the agent.
fn clock(at_ms: u64) -> String {
    let day = (at_ms / 1000) % 86_400;
    format!("{:02}:{:02}:{:02} UTC", day / 3600, day / 60 % 60, day % 60)
}

struct Grant {
    id: String,
    secret: [u8; 32],
}

struct Session {
    owner: Owner,
    token: [u8; 32],
    holder: Holder,
    since_ms: u64,
    last_input_ms: u64,
}

#[derive(Default)]
struct State {
    grant: Option<Grant>,
    session: Option<Session>,
    /// Why the PC behind this host cannot be reached now; a host on the PC itself never sets it.
    down: Option<String>,
}

/// The one share of this desktop: at most one open link, and at most one agent holding it.
pub struct Shared {
    state: Mutex<State>,
    pub events: Events,
}

fn digest(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

fn fresh() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A minted link's parts: the grant names it, the secret proves it. The viewer shows them as rigdesk://<host>/<grant>#<secret>.
#[derive(Clone, Debug)]
pub struct Minted {
    pub grant: String,
    pub secret: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Standing {
    pub owner: Option<Owner>,
    pub holder: Option<Holder>,
    pub since_ms: Option<u64>,
    pub link_open: bool,
}

impl Shared {
    pub fn new(events: Events) -> Self {
        Shared { state: Mutex::new(State::default()), events }
    }

    /// A new link, replacing any open one; the session already held stays until the link is claimed.
    pub fn mint(&self) -> Minted {
        let minted = Minted { grant: fresh(), secret: fresh() };
        self.state.lock().unwrap().grant = Some(Grant { id: minted.grant.clone(), secret: digest(&minted.secret) });
        minted
    }

    /// The link's first claim wins it: the claimant becomes the owner, replacing any earlier one, and holds input.
    pub fn claim(&self, grant: &str, secret: &str, owner: Owner) -> Result<String, Refusal> {
        let mut state = self.state.lock().unwrap();
        let fits = state.grant.as_ref().map_or(false, |held| {
            bool::from(held.id.as_bytes().ct_eq(grant.as_bytes())) & bool::from(held.secret.ct_eq(&digest(secret)))
        });
        if !fits {
            return Err(Refusal::WrongGrant);
        }
        state.grant = None;
        if let Some(previous) = state.session.take() {
            self.events.push(EventKind::Revoked { agent_id: previous.owner.agent_id, reason: "the desktop was shared with another agent".into() });
        }
        let token = fresh();
        let since_ms = now_ms();
        state.session = Some(Session { owner: owner.clone(), token: digest(&token), holder: Holder::Agent, since_ms, last_input_ms: 0 });
        self.events.push(EventKind::Claimed { owner });
        self.events.push(EventKind::Holder { holder: Holder::Agent, by: "share".into() });
        Ok(token)
    }

    fn held<'a>(state: &'a mut State, token: &str) -> Result<&'a mut Session, Refusal> {
        let wanted = digest(token);
        match state.session.as_mut() {
            Some(session) if bool::from(session.token.ct_eq(&wanted)) => Ok(session),
            _ => Err(Refusal::NoSession),
        }
    }

    /// Input the human took by using the PC goes back to the agent once their input has paused for `QUIET_MS`.
    fn settle(&self, session: &mut Session) {
        if session.holder == Holder::Human && now_ms().saturating_sub(session.last_input_ms) >= QUIET_MS {
            session.holder = Holder::Agent;
            session.since_ms = now_ms();
            self.events.push(EventKind::Holder { holder: Holder::Agent, by: "quiet".into() });
        }
    }

    /// Whether the session's holder may run a call now: a block stops everything, a human using the PC stops actions.
    pub fn admit(&self, token: &str, read_only: bool) -> Result<Owner, Refusal> {
        let mut state = self.state.lock().unwrap();
        let session = Self::held(&mut state, token)?;
        self.settle(session);
        match session.holder {
            Holder::Blocked => Err(Refusal::Blocked { since_ms: session.since_ms }),
            Holder::Human if !read_only => Err(Refusal::HumanHolds { since_ms: session.since_ms }),
            _ => Ok(session.owner.clone()),
        }
    }

    pub fn owner_of(&self, token: &str) -> Result<Owner, Refusal> {
        let mut state = self.state.lock().unwrap();
        Self::held(&mut state, token).map(|session| session.owner.clone())
    }

    fn set(&self, to: Holder, by: &str, allowed: impl Fn(Holder) -> bool) -> bool {
        let mut state = self.state.lock().unwrap();
        let Some(session) = state.session.as_mut() else { return false };
        if session.holder == to || !allowed(session.holder) {
            return false;
        }
        session.holder = to;
        session.since_ms = now_ms();
        self.events.push(EventKind::Holder { holder: to, by: by.into() });
        true
    }

    /// Viewer or local input: the human goes before the agent until their input pauses (see `settle`). A block stays.
    pub fn human_input(&self, by: &str) -> bool {
        {
            let mut state = self.state.lock().unwrap();
            let Some(session) = state.session.as_mut() else { return false };
            if session.holder == Holder::Blocked {
                return false;
            }
            session.last_input_ms = now_ms();
        }
        self.set(Holder::Human, by, |now| now == Holder::Agent)
    }

    /// The human's Stop: the agent is blocked until they resume it, and is told to stop even if it was already, so the
    /// bridge can cut its turn short. False with no agent joined.
    pub fn stop(&self) -> bool {
        if self.set(Holder::Blocked, "stop", |now| now != Holder::Blocked) {
            return true;
        }
        let state = self.state.lock().unwrap();
        let Some(session) = state.session.as_ref() else { return false };
        self.events.push(EventKind::Holder { holder: session.holder, by: "stop".into() });
        true
    }

    pub fn hand_to_agent(&self) -> bool {
        self.set(Holder::Agent, "hand to agent", |now| now == Holder::Human)
    }

    pub fn block(&self) -> bool {
        self.set(Holder::Blocked, "block", |_| true)
    }

    /// Unblocking resumes the agent; the human's next input still goes first.
    pub fn unblock(&self) -> bool {
        self.set(Holder::Agent, "unblock", |now| now == Holder::Blocked)
    }

    pub fn revoke(&self, reason: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        state.grant = None;
        let Some(session) = state.session.take() else { return false };
        self.events.push(EventKind::Revoked { agent_id: session.owner.agent_id, reason: reason.into() });
        true
    }

    /// Why the PC cannot be reached now, as the host last found; `None` while it can.
    pub fn down(&self) -> Option<String> {
        self.state.lock().unwrap().down.clone()
    }

    /// Records whether the PC can be reached; a change is an event.
    pub fn set_down(&self, why: Option<String>) {
        let mut state = self.state.lock().unwrap();
        if state.down != why {
            state.down = why.clone();
            drop(state);
            self.events.push(EventKind::Reach { down: why });
        }
    }

    pub fn standing(&self) -> Standing {
        let mut state = self.state.lock().unwrap();
        if let Some(session) = state.session.as_mut() {
            self.settle(session);
        }
        Standing {
            owner: state.session.as_ref().map(|s| s.owner.clone()),
            holder: state.session.as_ref().map(|s| s.holder),
            since_ms: state.session.as_ref().map(|s| s.since_ms),
            link_open: state.grant.is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_names_the_time_of_day_it_began() {
        let words = Refusal::HumanHolds { since_ms: 1_794_128_922_806 }.words();
        assert!(words.contains("since 09:08:42 UTC"), "{words}");
    }

    fn owner(id: &str) -> Owner {
        Owner { agent_id: id.into(), title: "front desk".into(), graph: "clients".into() }
    }

    #[test]
    fn a_link_is_claimed_once_and_the_claimant_holds_input() {
        let shared = Shared::new(Events::new(64));
        let link = shared.mint();
        let token = shared.claim(&link.grant, &link.secret, owner("a1")).unwrap();
        assert_eq!(shared.admit(&token, false).unwrap().agent_id, "a1");
        assert_eq!(shared.claim(&link.grant, &link.secret, owner("a2")), Err(Refusal::WrongGrant));
    }

    #[test]
    fn a_wrong_secret_wins_nothing() {
        let shared = Shared::new(Events::new(64));
        let link = shared.mint();
        assert_eq!(shared.claim(&link.grant, "nope", owner("a1")), Err(Refusal::WrongGrant));
        assert!(shared.claim(&link.grant, &link.secret, owner("a1")).is_ok());
    }

    #[test]
    fn human_input_stops_actions_but_not_reads_and_a_block_stops_both() {
        let shared = Shared::new(Events::new(64));
        let link = shared.mint();
        let token = shared.claim(&link.grant, &link.secret, owner("a1")).unwrap();
        assert!(shared.human_input("viewer mouse"));
        assert!(matches!(shared.admit(&token, false), Err(Refusal::HumanHolds { .. })));
        assert!(shared.admit(&token, true).is_ok());
        assert!(shared.hand_to_agent());
        assert!(shared.admit(&token, false).is_ok());
        assert!(shared.block());
        assert!(matches!(shared.admit(&token, true), Err(Refusal::Blocked { .. })));
        assert!(!shared.human_input("viewer mouse"), "input while blocked leaves the block");
        assert!(!shared.hand_to_agent(), "a block is lifted by unblock alone");
        assert!(shared.unblock());
        assert!(shared.admit(&token, false).is_ok(), "unblocking resumes the agent");
    }

    #[test]
    fn input_goes_back_to_the_agent_once_the_human_pauses_and_stop_blocks_it_until_resumed() {
        let shared = Shared::new(Events::new(64));
        let link = shared.mint();
        let token = shared.claim(&link.grant, &link.secret, owner("a1")).unwrap();
        shared.human_input("viewer input");
        assert!(matches!(shared.admit(&token, false), Err(Refusal::HumanHolds { .. })));
        shared.state.lock().unwrap().session.as_mut().unwrap().last_input_ms -= QUIET_MS;
        assert!(shared.admit(&token, false).is_ok(), "the human paused, so the agent acts again");
        assert!(shared.stop());
        shared.human_input("viewer input");
        shared.state.lock().unwrap().session.as_mut().unwrap().last_input_ms -= QUIET_MS;
        assert!(matches!(shared.admit(&token, false), Err(Refusal::Blocked { .. })), "Stop holds until the human resumes the agent");
        assert!(shared.unblock());
        assert!(shared.admit(&token, false).is_ok());
    }

    #[test]
    fn a_new_claim_replaces_the_owner_and_revokes_the_old_token() {
        let shared = Shared::new(Events::new(64));
        let first = shared.mint();
        let old = shared.claim(&first.grant, &first.secret, owner("a1")).unwrap();
        let second = shared.mint();
        let new = shared.claim(&second.grant, &second.secret, owner("a2")).unwrap();
        assert_eq!(shared.admit(&old, true), Err(Refusal::NoSession));
        assert_eq!(shared.admit(&new, true).unwrap().agent_id, "a2");
        assert!(shared.events.since(0).iter().any(|e| matches!(&e.kind, EventKind::Revoked { agent_id, .. } if agent_id == "a1")));
    }

    #[test]
    fn revoke_ends_the_session_and_closes_the_link() {
        let shared = Shared::new(Events::new(64));
        let link = shared.mint();
        let token = shared.claim(&link.grant, &link.secret, owner("a1")).unwrap();
        let open = shared.mint();
        assert!(shared.revoke("ended in the viewer"));
        assert_eq!(shared.admit(&token, true), Err(Refusal::NoSession));
        assert_eq!(shared.claim(&open.grant, &open.secret, owner("a2")), Err(Refusal::WrongGrant));
    }
}
