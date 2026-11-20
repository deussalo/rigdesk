//! RigDesk Android: one tap. The person's Accept on Rig Assistant's notification is their yes for that one ask, so the
//! knock Rig then makes for it, which names the ask under Rig's signature (`rig_knock`), is let in with no second Allow.
//!
//! The yes lives in the consent desk (`rigdesk_consent`): Accept files the request and the person's Allow, the knock's
//! login moves it to joined, and the connection closing ends it. A knock for any other ask, another agent, or after the
//! request lapsed, was withdrawn or was used once finds no yes here and asks the person as before.

use lazy_static::lazy_static;
use rigdesk_consent::{Agent, Ask, Desk, EndReason, Event, Millis, Request, Route, Scope, State, Step};
use std::{collections::HashMap, sync::Mutex, time::Instant};

/// What the person accepted on the notification, as Rig sent it to the phone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    pub ask: String,
    pub agent_id: String,
    pub agent_title: String,
    pub graph: String,
    pub operator: String,
    /// From now: past it, unused, the yes is gone.
    pub lapse_ms: u64,
}

/// Why an Accept was not taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    Invalid(&'static str),
    /// An agent's input reaches this device now, so it could have pressed Accept itself.
    AgentCanPress,
    /// The desk would not take it where the request stands (already accepted, say).
    NotTaken(String),
}

impl Refused {
    pub fn words(&self) -> String {
        match self {
            Refused::Invalid(why) => format!("the request is not one this phone can take: {why}"),
            Refused::AgentCanPress => "an agent's input reaches this phone now, so its Accept does not count: answer in Rig".into(),
            Refused::NotTaken(why) => format!("the phone did not take the Accept: {why}"),
        }
    }
}

/// The phone's consent desk and which connection carries which ask; time is the process's own monotonic clock.
pub struct OneTap {
    desk: Desk,
    carried: HashMap<i32, Ask>,
    started: Instant,
}

impl OneTap {
    fn new() -> Self {
        OneTap { desk: Desk::new(), carried: HashMap::new(), started: Instant::now() }
    }

    fn now(&self) -> Millis {
        Millis(self.started.elapsed().as_millis() as u64)
    }

    fn step(&mut self, event: Event) -> Step {
        let now = self.now();
        self.desk.step(now, event)
    }

    /// The person tapped Accept on the notification for this request.
    pub fn accept(&mut self, accepted: &Accepted, agent_can_press: bool) -> Result<(), Refused> {
        let ask = Ask::new(&accepted.ask).map_err(|_| Refused::Invalid("no ask"))?;
        let agent = Agent { id: accepted.agent_id.clone(), title: accepted.agent_title.clone(), graph: accepted.graph.clone() };
        let lapses_at = Millis(self.now().0.saturating_add(accepted.lapse_ms));
        let request = Request::new(ask.clone(), agent, &accepted.operator, Scope::Desktop, lapses_at).map_err(|invalid| {
            Refused::Invalid(match invalid {
                rigdesk_consent::Invalid::EmptyAgent => "no agent",
                rigdesk_consent::Invalid::EmptyOperator => "no operator",
                rigdesk_consent::Invalid::EmptyAsk => "no ask",
            })
        })?;
        if let Some(refused) = self.step(Event::Request(request)).refused {
            return Err(Refused::NotTaken(format!("{refused:?}")));
        }
        self.step(Event::Shown(ask.clone()));
        let allowed = self.step(Event::Allow { ask: ask.clone(), agent_can_press });
        match allowed.refused {
            None => Ok(()),
            Some(rigdesk_consent::Refusal::AgentCanPress(_)) => {
                self.step(Event::Withdraw(ask));
                Err(Refused::AgentCanPress)
            }
            Some(refused) => Err(Refused::NotTaken(format!("{refused:?}"))),
        }
    }

    /// Rig took the request back, or answered it another way: an unused yes is gone.
    pub fn withdraw(&mut self, ask: &str) {
        if let Ok(ask) = Ask::new(ask) {
            self.step(Event::Withdraw(ask));
        }
    }

    /// A knock Rig proved for this ask and agent, on connection `conn`: true when the person's yes for it stands, and the
    /// login is then joined on that yes. A yes counts once.
    pub fn admit(&mut self, conn: i32, ask: &str, agent_id: &str) -> bool {
        let Ok(ask) = Ask::new(ask) else { return false };
        let ours = self.desk.open().any(|(request, state)| {
            request.ask == ask && request.agent.id == agent_id && matches!(state, State::Allowed { .. })
        });
        if !ours {
            return false;
        }
        let step = self.step(Event::LoginOk { ask: ask.clone(), route: Route::Adopted });
        if step.refused.is_some() {
            return false;
        }
        self.carried.insert(conn, ask);
        true
    }

    /// The login admitted on connection `conn` did not get in after all.
    pub fn failed(&mut self, conn: i32) {
        self.ended(conn, EndReason::LoginFailed);
    }

    /// Connection `conn` closed: the session it carried has ended.
    pub fn left(&mut self, conn: i32) {
        self.ended(conn, EndReason::ConnectionLost);
    }

    fn ended(&mut self, conn: i32, reason: EndReason) {
        if let Some(ask) = self.carried.remove(&conn) {
            self.step(Event::End { ask, reason });
        }
    }

    /// The ask's state, by its name in CONSENT.md, while the desk holds it.
    pub fn state(&mut self, ask: &str) -> Option<&'static str> {
        self.step(Event::Tick);
        let ask = Ask::new(ask).ok()?;
        self.desk.state(&ask).map(State::name)
    }
}

lazy_static! {
    static ref ONE_TAP: Mutex<OneTap> = Mutex::new(OneTap::new());
}

pub fn accept(accepted: &Accepted, agent_can_press: bool) -> Result<(), Refused> {
    ONE_TAP.lock().unwrap().accept(accepted, agent_can_press)
}

pub fn withdraw(ask: &str) {
    ONE_TAP.lock().unwrap().withdraw(ask)
}

pub fn admit(conn: i32, ask: &str, agent_id: &str) -> bool {
    ONE_TAP.lock().unwrap().admit(conn, ask, agent_id)
}

pub fn failed(conn: i32) {
    ONE_TAP.lock().unwrap().failed(conn)
}

pub fn left(conn: i32) {
    ONE_TAP.lock().unwrap().left(conn)
}

pub fn state(ask: &str) -> Option<&'static str> {
    ONE_TAP.lock().unwrap().state(ask)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepted(ask: &str, lapse_ms: u64) -> Accepted {
        Accepted {
            ask: ask.into(),
            agent_id: "agent_a".into(),
            agent_title: "Builder".into(),
            graph: "g".into(),
            operator: "Arian".into(),
            lapse_ms,
        }
    }

    #[test]
    fn the_knock_for_the_accepted_ask_is_let_in_once_and_its_session_ends_with_the_connection() {
        let mut tap = OneTap::new();
        tap.accept(&accepted("desk_1", 60_000), false).unwrap();
        assert_eq!(tap.state("desk_1"), Some("allowed"));
        assert!(tap.admit(7, "desk_1", "agent_a"));
        assert_eq!(tap.state("desk_1"), Some("joined"));
        assert!(!tap.admit(8, "desk_1", "agent_a"), "a yes counts once");
        tap.left(7);
        assert_eq!(tap.state("desk_1"), None, "ended and dropped");
        assert!(!tap.admit(9, "desk_1", "agent_a"));
    }

    #[test]
    fn a_knock_for_another_ask_or_agent_or_with_no_accept_finds_no_yes() {
        let mut tap = OneTap::new();
        assert!(!tap.admit(1, "desk_1", "agent_a"), "nothing accepted");
        tap.accept(&accepted("desk_1", 60_000), false).unwrap();
        assert!(!tap.admit(1, "desk_2", "agent_a"), "another ask");
        assert!(!tap.admit(1, "desk_1", "agent_b"), "another agent");
        assert_eq!(tap.state("desk_1"), Some("allowed"), "still there for its own knock");
    }

    #[test]
    fn withdrawal_and_lapse_take_the_yes_away() {
        let mut tap = OneTap::new();
        tap.accept(&accepted("desk_1", 60_000), false).unwrap();
        tap.withdraw("desk_1");
        assert!(!tap.admit(1, "desk_1", "agent_a"));

        tap.accept(&accepted("desk_2", 0), false).unwrap_err();
        tap.accept(&accepted("desk_3", 30), false).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(60));
        assert!(!tap.admit(1, "desk_3", "agent_a"), "lapsed");
        assert_eq!(tap.state("desk_3"), None);
    }

    #[test]
    fn an_accept_an_agent_could_have_pressed_counts_for_nothing() {
        let mut tap = OneTap::new();
        assert_eq!(tap.accept(&accepted("desk_1", 60_000), true), Err(Refused::AgentCanPress));
        assert!(!tap.admit(1, "desk_1", "agent_a"));
        assert_eq!(tap.state("desk_1"), None);
        tap.accept(&accepted("desk_1", 60_000), false).unwrap();
        assert!(tap.admit(1, "desk_1", "agent_a"), "the person's own Accept afterwards");
    }

    #[test]
    fn a_login_that_then_fails_ends_the_request_and_frees_nothing_else() {
        let mut tap = OneTap::new();
        tap.accept(&accepted("desk_1", 60_000), false).unwrap();
        assert!(tap.admit(4, "desk_1", "agent_a"));
        tap.failed(4);
        assert_eq!(tap.state("desk_1"), None);
        tap.left(4);
        assert!(!tap.admit(5, "desk_1", "agent_a"));
    }

    #[test]
    fn an_accept_missing_who_asks_is_refused() {
        let mut tap = OneTap::new();
        assert_eq!(tap.accept(&Accepted { ask: " ".into(), ..accepted("x", 1_000) }, false), Err(Refused::Invalid("no ask")));
        assert_eq!(tap.accept(&Accepted { agent_id: "".into(), ..accepted("x", 1_000) }, false), Err(Refused::Invalid("no agent")));
        assert_eq!(tap.accept(&Accepted { operator: "".into(), ..accepted("x", 1_000) }, false), Err(Refused::Invalid("no operator")));
    }
}
