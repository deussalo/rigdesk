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

/// Who has the input while the agent is not blocked; a block is the agent's, not the session's (see [`Block`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hands {
    Human,
    Agent,
}

struct Session {
    owner: Owner,
    token: [u8; 32],
    hands: Hands,
    since_ms: u64,
    last_input_ms: u64,
}

/// The human's Block or Stop, on an agent: it outlives that agent's session, so leaving and joining again (a new link,
/// or Rig's own rejoin of an assigned PC) finds it still blocked. Only the human's unblock lifts it.
struct Block {
    agent_id: String,
    since_ms: u64,
}

#[derive(Default)]
struct State {
    grant: Option<Grant>,
    session: Option<Session>,
    block: Option<Block>,
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

impl State {
    /// The session's holder: blocked when its agent is, else whoever has the input; with when it began.
    fn holder(&self) -> Option<(Holder, u64)> {
        let session = self.session.as_ref()?;
        Some(match &self.block {
            Some(block) if block.agent_id == session.owner.agent_id => (Holder::Blocked, block.since_ms),
            _ => (if session.hands == Hands::Human { Holder::Human } else { Holder::Agent }, session.since_ms),
        })
    }

    fn blocked(&self) -> bool {
        matches!(self.holder(), Some((Holder::Blocked, _)))
    }
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
        state.session = Some(Session { owner: owner.clone(), token: digest(&token), hands: Hands::Agent, since_ms: now_ms(), last_input_ms: 0 });
        let holder = state.holder().map_or(Holder::Agent, |(holder, _)| holder);
        self.events.push(EventKind::Claimed { owner });
        self.events.push(EventKind::Holder { holder, by: "share".into() });
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
    fn settle(&self, state: &mut State) {
        if state.blocked() {
            return;
        }
        let Some(session) = state.session.as_mut() else { return };
        if session.hands == Hands::Human && now_ms().saturating_sub(session.last_input_ms) >= QUIET_MS {
            session.hands = Hands::Agent;
            session.since_ms = now_ms();
            self.events.push(EventKind::Holder { holder: Holder::Agent, by: "quiet".into() });
        }
    }

    /// Whether the session's holder may run a call now: a block stops everything, a human using the PC stops actions.
    pub fn admit(&self, token: &str, read_only: bool) -> Result<Owner, Refusal> {
        let mut state = self.state.lock().unwrap();
        let owner = Self::held(&mut state, token)?.owner.clone();
        self.settle(&mut state);
        match state.holder() {
            Some((Holder::Blocked, since_ms)) => Err(Refusal::Blocked { since_ms }),
            Some((Holder::Human, since_ms)) if !read_only => Err(Refusal::HumanHolds { since_ms }),
            _ => Ok(owner),
        }
    }

    pub fn owner_of(&self, token: &str) -> Result<Owner, Refusal> {
        let mut state = self.state.lock().unwrap();
        Self::held(&mut state, token).map(|session| session.owner.clone())
    }

    /// Moves the input between the human and an agent that is not blocked.
    fn hand(&self, to: Hands, by: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.blocked() {
            return false;
        }
        let Some(session) = state.session.as_mut() else { return false };
        if session.hands == to {
            return false;
        }
        session.hands = to;
        session.since_ms = now_ms();
        let holder = if to == Hands::Human { Holder::Human } else { Holder::Agent };
        self.events.push(EventKind::Holder { holder, by: by.into() });
        true
    }

    /// Viewer or local input: the human goes before the agent until their input pauses (see `settle`). A block stays.
    pub fn human_input(&self, by: &str) -> bool {
        {
            let mut state = self.state.lock().unwrap();
            if state.blocked() {
                return false;
            }
            let Some(session) = state.session.as_mut() else { return false };
            session.last_input_ms = now_ms();
        }
        self.hand(Hands::Human, by)
    }

    /// A viewer's pointer event, by RustDesk's mask (button bits over the event type): a press, release, wheel or drag
    /// takes input from the agent, as `human_input`; a plain move while the agent holds input is kept from the PC, so
    /// watching never disturbs the agent. Answers whether the event goes on to the PC.
    pub fn viewer_pointer(&self, mask: i32, by: &str) -> bool {
        let hover = mask == 0;
        if !hover {
            self.human_input(by);
            return true;
        }
        self.state.lock().unwrap().holder().map_or(true, |(holder, _)| holder != Holder::Agent)
    }

    /// Blocks the session's agent; false with no agent joined, or one already blocked.
    fn block_by(&self, by: &str) -> Result<bool, ()> {
        let mut state = self.state.lock().unwrap();
        let Some(session) = state.session.as_ref() else { return Err(()) };
        if state.blocked() {
            return Ok(false);
        }
        let agent_id = session.owner.agent_id.clone();
        state.block = Some(Block { agent_id, since_ms: now_ms() });
        self.events.push(EventKind::Holder { holder: Holder::Blocked, by: by.into() });
        Ok(true)
    }

    /// The human's Stop: the agent is blocked until they resume it, and is told to stop even if it was already, so the
    /// bridge can cut its turn short. False with no agent joined.
    pub fn stop(&self) -> bool {
        match self.block_by("stop") {
            Err(()) => false,
            Ok(true) => true,
            Ok(false) => {
                self.events.push(EventKind::Holder { holder: Holder::Blocked, by: "stop".into() });
                true
            }
        }
    }

    pub fn hand_to_agent(&self) -> bool {
        let human = self.state.lock().unwrap().holder().map_or(false, |(holder, _)| holder == Holder::Human);
        human && self.hand(Hands::Agent, "hand to agent")
    }

    pub fn block(&self) -> bool {
        self.block_by("block") == Ok(true)
    }

    /// Unblocking resumes the agent; the human's next input still goes first. It lifts a block on an agent that has
    /// since left too, so it does not come back blocked.
    pub fn unblock(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        let Some(block) = state.block.take() else { return false };
        if let Some(session) = state.session.as_mut().filter(|s| s.owner.agent_id == block.agent_id) {
            session.hands = Hands::Agent;
            session.since_ms = now_ms();
            self.events.push(EventKind::Holder { holder: Holder::Agent, by: "unblock".into() });
        }
        true
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
        self.settle(&mut state);
        let holder = state.holder();
        Standing {
            owner: state.session.as_ref().map(|s| s.owner.clone()),
            holder: holder.map(|(holder, _)| holder),
            since_ms: holder.map(|(_, since)| since),
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
    fn a_hover_while_the_agent_holds_input_is_kept_from_the_pc_and_a_click_takes_input_back() {
        let shared = Shared::new(Events::new(64));
        assert!(shared.viewer_pointer(0, "viewer input"), "no agent: the pointer goes to the PC");
        let link = shared.mint();
        let token = shared.claim(&link.grant, &link.secret, owner("a1")).unwrap();
        assert!(!shared.viewer_pointer(0, "viewer input"), "a hover is kept from the PC");
        assert!(shared.admit(&token, false).is_ok(), "and leaves the agent its input");
        assert!(shared.viewer_pointer(1 | (1 << 3), "viewer input"), "a click goes to the PC");
        assert!(matches!(shared.admit(&token, false), Err(Refusal::HumanHolds { .. })));
        assert!(shared.viewer_pointer(0, "viewer input"), "while the human holds input, moves go to the PC");
    }

    #[test]
    fn a_block_is_the_agents_and_outlives_its_session_until_unblocked() {
        let shared = Shared::new(Events::new(64));
        let link = shared.mint();
        let token = shared.claim(&link.grant, &link.secret, owner("a1")).unwrap();
        assert!(shared.block());
        assert!(shared.revoke("the agent left"));
        let again = shared.mint();
        let token2 = shared.claim(&again.grant, &again.secret, owner("a1")).unwrap();
        assert_ne!(token, token2);
        assert!(matches!(shared.admit(&token2, true), Err(Refusal::Blocked { .. })), "joining again does not lift the block");
        assert_eq!(shared.standing().holder, Some(Holder::Blocked));
        assert!(!shared.block(), "already blocked");
        let other = shared.mint();
        let token3 = shared.claim(&other.grant, &other.secret, owner("a2")).unwrap();
        assert!(shared.admit(&token3, false).is_ok(), "another agent is not blocked by a1's block");
        assert!(shared.unblock(), "the human lifts a1's block though a1 has left");
        let back = shared.mint();
        let token4 = shared.claim(&back.grant, &back.secret, owner("a1")).unwrap();
        assert!(shared.admit(&token4, false).is_ok());
        assert!(!shared.unblock(), "nothing left to lift");
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
