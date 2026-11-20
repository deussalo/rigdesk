//! RigDesk Android: one tap. The phone's side of an agent's request, as one machine: the consent desk (`rigdesk_consent`)
//! holds every request Rig sent over Rig Assistant's link, from Rig's join_request to the end of its session, and Rig
//! Assistant's Kotlin only draws what [`Render`] says: the notifications to post and take down, and the words to send Rig.
//!
//! The person's Accept on the notification is their yes for that one ask, so the knock Rig then makes for it, which names
//! the ask under Rig's signature (`rig_knock`), is let in with no second Allow. A knock for any other ask, another agent, or
//! after the request lapsed, was withdrawn or was used once finds no yes here and asks the person as before.

use lazy_static::lazy_static;
use rigdesk_consent::{Agent, Ask, Desk, EndReason, Event, Millis, Refusal, Request, Route, Scope, State, Step};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Mutex,
    time::Instant,
};

/// What Rig Assistant's Kotlin tells the desk, parsed once from its `{op, ...}` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    /// Rig's join_request frame: `{ask, nonce, agent: {id, title, graph}, operator, lapse_s}`.
    Request { ask: String, nonce: String, agent: Agent, operator: String, lapse_ms: u64 },
    /// The notification is posted.
    Shown(String),
    /// The notification could not be posted.
    Unshown { ask: String, why: String },
    /// The person tapped Accept.
    Accept(String),
    /// Whether the person's yes for the ask still stands.
    Allowed(String),
    /// The person tapped Deny, or cancelled Android's Start now after their Accept.
    Deny { ask: String, why: String },
    /// Sharing runs and the phone listens for Rig's knock.
    Ready(String),
    /// Sharing did not start, or the phone does not listen.
    NotReady { ask: String, why: String },
    /// Rig's join_end frame: Rig is done with the request, however it ended.
    End(String),
    /// Time passed: what lapsed goes.
    Tick,
}

impl Call {
    pub fn parse(text: &str) -> Result<Call, String> {
        let body: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
        let word = |key: &str| body.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        let ask = word("ask");
        match body.get("op").and_then(Value::as_str).unwrap_or("") {
            "request" => {
                let agent = body.get("agent").cloned().unwrap_or(Value::Null);
                let field = |key: &str| agent.get(key).and_then(Value::as_str).unwrap_or("").to_string();
                let id = field("id");
                let title = Some(field("title")).filter(|t| !t.trim().is_empty()).unwrap_or_else(|| id.clone());
                let lapse_s = body.get("lapse_s").and_then(Value::as_u64).unwrap_or(0);
                Ok(Call::Request {
                    ask,
                    nonce: word("nonce"),
                    agent: Agent { id, title, graph: field("graph") },
                    operator: word("operator"),
                    lapse_ms: lapse_s.saturating_mul(1000),
                })
            }
            "shown" => Ok(Call::Shown(ask)),
            "unshown" => Ok(Call::Unshown { ask, why: word("why") }),
            "accept" => Ok(Call::Accept(ask)),
            "allowed" => Ok(Call::Allowed(ask)),
            "deny" => Ok(Call::Deny { ask, why: word("why") }),
            "ready" => Ok(Call::Ready(ask)),
            "not_ready" => Ok(Call::NotReady { ask, why: word("why") }),
            "end" => Ok(Call::End(ask)),
            "tick" => Ok(Call::Tick),
            other => Err(format!("no such call: {other:?}")),
        }
    }
}

/// A notification to post: the request it asks the person about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    pub ask: String,
    pub agent_title: String,
    pub operator: String,
    pub left_ms: u64,
    /// Not yet said to be posted: the caller says `shown` (or `unshown`) once it posted it.
    pub fresh: bool,
    /// To post now: fresh, or Rig sent the request again after a reconnect. The others are drawn already.
    pub post: bool,
}

/// What the phone tells Rig of a request: `ready`, `deny` or `failed`, with why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Said {
    pub ask: String,
    pub nonce: String,
    pub said: &'static str,
    pub reason: String,
}

/// What the caller draws and sends after a call: every prompt that may show, the notifications to take down, what to
/// tell Rig, the request the person's Accept now stands for (its agent's title), and why a call was not taken.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Render {
    pub prompts: Vec<Prompt>,
    pub hide: Vec<String>,
    pub say: Vec<Said>,
    pub allowed: Option<String>,
    pub refused: Option<String>,
    resent: Option<String>,
}

impl Render {
    pub fn to_json(&self) -> String {
        let prompts: Vec<Value> = self
            .prompts
            .iter()
            .map(|p| json!({"ask": p.ask, "agent_title": p.agent_title, "operator": p.operator, "left_ms": p.left_ms, "fresh": p.fresh, "post": p.post}))
            .collect();
        let say: Vec<Value> =
            self.say.iter().map(|s| json!({"ask": s.ask, "nonce": s.nonce, "said": s.said, "reason": s.reason})).collect();
        json!({"prompts": prompts, "hide": self.hide, "say": say, "allowed": self.allowed, "refused": self.refused}).to_string()
    }
}

fn words(refusal: &Refusal) -> String {
    match refusal {
        Refusal::NotOpen(_) => "that request has already ended".into(),
        Refusal::Duplicate(_) => "the phone holds that request already".into(),
        Refusal::AlreadyJoined(_) => "this agent is already in on this phone".into(),
        Refusal::AgentCanPress(_) => "an agent's input reaches this phone now, so its answer does not count: answer in Rig".into(),
        Refusal::OutOfTurn(_) => "the request is past that point".into(),
    }
}

/// The phone's consent desk, each open request's nonce, and which connection carries which ask; time is the process's
/// own monotonic clock.
pub struct OneTap {
    desk: Desk,
    nonces: BTreeMap<Ask, String>,
    carried: HashMap<i32, Ask>,
    started: Instant,
}

impl OneTap {
    fn new() -> Self {
        OneTap { desk: Desk::new(), nonces: BTreeMap::new(), carried: HashMap::new(), started: Instant::now() }
    }

    fn now(&self) -> Millis {
        Millis(self.started.elapsed().as_millis() as u64)
    }

    fn step(&mut self, event: Event, render: &mut Render) -> Step {
        let now = self.now();
        let step = self.desk.step(now, event);
        for effect in &step.effects {
            if let rigdesk_consent::Effect::HidePrompt(ask) = effect {
                render.hide.push(ask.as_str().to_string());
            }
        }
        step
    }

    fn say(&self, ask: &Ask, said: &'static str, reason: &str, render: &mut Render) {
        if let Some(nonce) = self.nonces.get(ask) {
            render.say.push(Said { ask: ask.as_str().into(), nonce: nonce.clone(), said, reason: reason.into() });
        }
    }

    /// Carries out one call from the Kotlin side. `agent_can_press` says whether an agent's input reaches the phone now,
    /// so that an answer the person seems to give counts for nothing.
    pub fn call(&mut self, call: Call, agent_can_press: bool) -> Render {
        let mut render = Render::default();
        if let Err(why) = self.apply(call, agent_can_press, &mut render) {
            render.refused = Some(why);
        }
        self.step(Event::Tick, &mut render);
        let open: Vec<Ask> = self.desk.open().map(|(r, _)| r.ask.clone()).collect();
        self.nonces.retain(|ask, _| open.contains(ask));
        let now = self.now();
        render.prompts = self
            .desk
            .open()
            .filter_map(|(r, state)| {
                let fresh = match state {
                    State::Requested { unshown: None, .. } => true,
                    State::Prompting { .. } => false,
                    _ => return None,
                };
                let left_ms = r.lapses_at.0.saturating_sub(now.0);
                let post = fresh || render.resent.as_deref() == Some(r.ask.as_str());
                let (ask, agent_title, operator) = (r.ask.as_str().into(), r.agent.title.clone(), r.operator.clone());
                Some(Prompt { ask, agent_title, operator, left_ms, fresh, post })
            })
            .collect();
        render
    }

    fn apply(&mut self, call: Call, agent_can_press: bool, render: &mut Render) -> Result<(), String> {
        let ask_of = |ask: &str| Ask::new(ask).map_err(|_| "no ask".to_string());
        match call {
            Call::Request { ask, nonce, agent, operator, lapse_ms } => {
                let ask = ask_of(&ask)?;
                if nonce.trim().is_empty() || agent.id.trim().is_empty() || lapse_ms == 0 {
                    return Err("join_request missing its nonce, agent or lapse".into());
                }
                let lapses_at = Millis(self.now().0.saturating_add(lapse_ms));
                let request = Request::new(ask.clone(), agent, &operator, Scope::Desktop, lapses_at).map_err(|invalid| format!("{invalid:?}"))?;
                self.nonces.entry(ask.clone()).or_insert(nonce);
                match self.step(Event::Request(request), render).refused {
                    None => Ok(()),
                    // A resend after a reconnect: the prompt is posted again where it may still show.
                    Some(Refusal::Duplicate(_)) => {
                        render.resent = Some(ask.as_str().into());
                        Ok(())
                    }
                    Some(refusal) => {
                        let why = words(&refusal);
                        self.say(&ask, "failed", &why, render);
                        Err(why)
                    }
                }
            }
            Call::Shown(ask) => {
                let ask = ask_of(&ask)?;
                self.step(Event::Shown(ask), render);
                Ok(())
            }
            Call::Unshown { ask, why } => {
                let ask = ask_of(&ask)?;
                self.step(Event::Unshown { ask: ask.clone(), reason: why.clone() }, render);
                self.say(&ask, "failed", &format!("the phone could not show the request: {why}"), render);
                Ok(())
            }
            Call::Accept(ask) => {
                let ask = ask_of(&ask)?;
                match self.step(Event::Allow { ask: ask.clone(), agent_can_press }, render).refused {
                    None => {
                        render.allowed = self.desk.open().find(|(r, _)| r.ask == ask).map(|(r, _)| r.agent.title.clone());
                        Ok(())
                    }
                    Some(refusal) => Err(words(&refusal)),
                }
            }
            Call::Allowed(ask) => {
                let ask = ask_of(&ask)?;
                self.step(Event::Tick, render);
                match self.desk.open().find(|(r, _)| r.ask == ask) {
                    Some((r, State::Allowed { .. })) => {
                        render.allowed = Some(r.agent.title.clone());
                        Ok(())
                    }
                    Some(_) => Err(words(&Refusal::OutOfTurn(ask))),
                    None => Err(words(&Refusal::NotOpen(ask))),
                }
            }
            Call::Deny { ask, why } => {
                let ask = ask_of(&ask)?;
                match self.step(Event::Dismiss { ask: ask.clone(), agent_can_press }, render).refused {
                    None => {
                        self.say(&ask, "deny", &why, render);
                        Ok(())
                    }
                    Some(refusal) => Err(words(&refusal)),
                }
            }
            Call::Ready(ask) => {
                let ask = ask_of(&ask)?;
                self.step(Event::Tick, render);
                match self.desk.state(&ask) {
                    Some(State::Allowed { .. }) => {
                        self.say(&ask, "ready", "", render);
                        Ok(())
                    }
                    Some(_) => Err(words(&Refusal::OutOfTurn(ask))),
                    None => Err(words(&Refusal::NotOpen(ask))),
                }
            }
            Call::NotReady { ask, why } => {
                let ask = ask_of(&ask)?;
                match self.step(Event::LoginFailed(ask.clone()), render).refused {
                    None => {
                        self.say(&ask, "failed", &why, render);
                        Ok(())
                    }
                    Some(refusal) => Err(words(&refusal)),
                }
            }
            Call::End(ask) => {
                let ask = ask_of(&ask)?;
                // A request already joined stays: its session ends with its connection, not with Rig's frame.
                if matches!(self.desk.state(&ask), Some(state) if !matches!(state, State::Joined { .. })) {
                    self.step(Event::Withdraw(ask), render);
                }
                Ok(())
            }
            Call::Tick => Ok(()),
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
        if self.step(Event::LoginOk { ask: ask.clone(), route: Route::Adopted }, &mut Render::default()).refused.is_some() {
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
            self.step(Event::End { ask, reason }, &mut Render::default());
        }
    }

    /// The ask's state, by its name in CONSENT.md, while the desk holds it.
    pub fn state(&mut self, ask: &str) -> Option<&'static str> {
        self.step(Event::Tick, &mut Render::default());
        let ask = Ask::new(ask).ok()?;
        self.desk.state(&ask).map(State::name)
    }
}

lazy_static! {
    static ref ONE_TAP: Mutex<OneTap> = Mutex::new(OneTap::new());
}

/// One call from Rig Assistant's Kotlin, as JSON, answered with the [`Render`] as JSON.
pub fn call(text: &str, agent_can_press: bool) -> String {
    match Call::parse(text) {
        Ok(call) => ONE_TAP.lock().unwrap().call(call, agent_can_press).to_json(),
        Err(why) => Render { refused: Some(why), ..Render::default() }.to_json(),
    }
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

    fn request(ask: &str, lapse_s: u64) -> Call {
        Call::parse(
            &json!({"op": "request", "ask": ask, "nonce": format!("nonce-{ask}-0123456789"), "operator": "Arian", "lapse_s": lapse_s,
                    "agent": {"id": "agent_a", "title": "Builder", "graph": "g"}})
            .to_string(),
        )
        .unwrap()
    }

    fn op(op: &str, ask: &str) -> Call {
        Call::parse(&json!({"op": op, "ask": ask, "why": "because"}).to_string()).unwrap()
    }

    /// A request posted and accepted: the person's yes stands.
    fn accepted(tap: &mut OneTap, ask: &str) -> Render {
        tap.call(request(ask, 60), false);
        tap.call(op("shown", ask), false);
        tap.call(op("accept", ask), false)
    }

    #[test]
    fn a_request_is_drawn_until_it_is_posted_then_shown_and_the_accept_takes_it_down() {
        let mut tap = OneTap::new();
        let drawn = tap.call(request("desk_1", 60), false);
        assert_eq!(drawn.prompts.len(), 1);
        let prompt = &drawn.prompts[0];
        assert!(prompt.fresh && prompt.agent_title == "Builder" && prompt.operator == "Arian" && prompt.left_ms > 59_000);
        assert!(prompt.post);
        let shown = tap.call(op("shown", "desk_1"), false);
        assert!(!shown.prompts[0].fresh && !shown.prompts[0].post, "posted once");
        let resent = tap.call(request("desk_1", 60), false);
        assert!(resent.prompts[0].post && !resent.prompts[0].fresh, "a resend posts it again");
        assert!(!tap.call(Call::Tick, false).prompts[0].post);
        let yes = tap.call(op("accept", "desk_1"), false);
        assert_eq!((yes.prompts.len(), yes.hide.clone(), yes.allowed.clone()), (0, vec!["desk_1".to_string()], Some("Builder".to_string())));
        assert_eq!(tap.call(op("allowed", "desk_1"), false).allowed.as_deref(), Some("Builder"));
        let ready = tap.call(op("ready", "desk_1"), false);
        assert_eq!(ready.say, [Said { ask: "desk_1".into(), nonce: "nonce-desk_1-0123456789".into(), said: "ready", reason: "".into() }]);
    }

    #[test]
    fn the_knock_for_the_accepted_ask_is_let_in_once_and_its_session_ends_with_the_connection() {
        let mut tap = OneTap::new();
        accepted(&mut tap, "desk_1");
        assert_eq!(tap.state("desk_1"), Some("allowed"));
        assert!(tap.admit(7, "desk_1", "agent_a"));
        assert_eq!(tap.state("desk_1"), Some("joined"));
        assert!(!tap.admit(8, "desk_1", "agent_a"), "a yes counts once");
        tap.call(op("end", "desk_1"), false);
        assert_eq!(tap.state("desk_1"), Some("joined"), "Rig's join_end leaves a joined session in");
        tap.left(7);
        assert_eq!(tap.state("desk_1"), None, "ended and dropped");
        assert!(!tap.admit(9, "desk_1", "agent_a"));
    }

    #[test]
    fn a_knock_for_another_ask_or_agent_or_with_no_accept_finds_no_yes() {
        let mut tap = OneTap::new();
        assert!(!tap.admit(1, "desk_1", "agent_a"), "nothing accepted");
        tap.call(request("desk_1", 60), false);
        tap.call(op("shown", "desk_1"), false);
        assert!(!tap.admit(1, "desk_1", "agent_a"), "posted, not accepted");
        tap.call(op("accept", "desk_1"), false);
        assert!(!tap.admit(1, "desk_2", "agent_a"), "another ask");
        assert!(!tap.admit(1, "desk_1", "agent_b"), "another agent");
        assert_eq!(tap.state("desk_1"), Some("allowed"), "still there for its own knock");
    }

    #[test]
    fn deny_says_no_with_the_nonce_and_rigs_end_and_the_lapse_take_the_notification_down_saying_nothing() {
        let mut tap = OneTap::new();
        tap.call(request("desk_1", 60), false);
        tap.call(op("shown", "desk_1"), false);
        let no = tap.call(op("deny", "desk_1"), false);
        assert_eq!((no.hide.clone(), no.say[0].said, no.say[0].nonce.as_str()), (vec!["desk_1".to_string()], "deny", "nonce-desk_1-0123456789"));
        assert_eq!(tap.call(op("ready", "desk_1"), false).refused.as_deref(), Some("that request has already ended"));
        assert_eq!(tap.call(op("allowed", "desk_1"), false).allowed, None);

        tap.call(request("desk_2", 60), false);
        tap.call(op("shown", "desk_2"), false);
        let ended = tap.call(op("end", "desk_2"), false);
        assert_eq!((ended.hide, ended.say.len(), ended.prompts.len()), (vec!["desk_2".to_string()], 0, 0));

        let Call::Request { ask, nonce, agent, operator, .. } = request("desk_3", 1) else { unreachable!() };
        tap.call(Call::Request { ask, nonce, agent, operator, lapse_ms: 30 }, false);
        tap.call(op("shown", "desk_3"), false);
        std::thread::sleep(std::time::Duration::from_millis(60));
        let lapsed = tap.call(Call::Tick, false);
        assert_eq!((lapsed.hide, lapsed.say.len()), (vec!["desk_3".to_string()], 0));
        assert!(!tap.admit(1, "desk_3", "agent_a"));
        assert!(tap.nonces.is_empty(), "no nonce outlives its request");
    }

    #[test]
    fn an_answer_an_agent_could_have_pressed_counts_for_nothing_and_the_notification_stays() {
        let mut tap = OneTap::new();
        tap.call(request("desk_1", 60), false);
        tap.call(op("shown", "desk_1"), false);
        for answer in ["accept", "deny"] {
            let pressed = tap.call(op(answer, "desk_1"), true);
            assert!(pressed.refused.unwrap().starts_with("an agent's input reaches this phone"));
            assert!(pressed.say.is_empty() && pressed.hide.is_empty() && pressed.prompts.len() == 1);
        }
        assert!(!tap.admit(1, "desk_1", "agent_a"));
        tap.call(op("accept", "desk_1"), false);
        assert!(tap.admit(1, "desk_1", "agent_a"), "the person's own Accept afterwards");
    }

    #[test]
    fn cancelling_start_now_after_accept_says_no_and_a_phone_that_cannot_listen_says_why() {
        let mut tap = OneTap::new();
        accepted(&mut tap, "desk_1");
        let cancelled = tap.call(op("deny", "desk_1"), false);
        assert_eq!((cancelled.say[0].said, cancelled.say[0].reason.as_str()), ("deny", "because"));
        assert!(!tap.admit(1, "desk_1", "agent_a"));

        accepted(&mut tap, "desk_2");
        let failed = tap.call(op("not_ready", "desk_2"), false);
        assert_eq!((failed.say[0].said, failed.say[0].reason.as_str()), ("failed", "because"));
        assert!(!tap.admit(1, "desk_2", "agent_a"));
    }

    #[test]
    fn a_login_that_then_fails_ends_the_request_and_frees_nothing_else() {
        let mut tap = OneTap::new();
        accepted(&mut tap, "desk_1");
        assert!(tap.admit(4, "desk_1", "agent_a"));
        tap.failed(4);
        assert_eq!(tap.state("desk_1"), None);
        tap.left(4);
        assert!(!tap.admit(5, "desk_1", "agent_a"));
    }

    #[test]
    fn a_request_missing_who_asks_or_its_nonce_is_refused_and_an_agent_in_already_is_told_so() {
        let mut tap = OneTap::new();
        let bad = |body: Value| Call::parse(&body.to_string()).unwrap();
        let base = json!({"op": "request", "ask": "x", "nonce": "nonce-0123456789abc", "operator": "Arian", "lapse_s": 60, "agent": {"id": "a"}});
        for (key, value) in [("ask", json!(" ")), ("nonce", json!("")), ("operator", json!("")), ("lapse_s", json!(0))] {
            let mut body = base.clone();
            body[key] = value;
            let render = tap.call(bad(body), false);
            assert!(render.refused.is_some() && render.prompts.is_empty(), "{}", key);
        }
        assert!(Call::parse("{\"op\": \"pressed\"}").is_err());

        accepted(&mut tap, "desk_1");
        assert!(tap.admit(1, "desk_1", "agent_a"));
        let again = tap.call(request("desk_2", 60), false);
        assert_eq!((again.say[0].said, again.say[0].reason.as_str()), ("failed", "this agent is already in on this phone"));
        assert!(again.prompts.is_empty());
    }
}
