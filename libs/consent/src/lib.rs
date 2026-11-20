//! RigDesk's consent: an agent's request to use a desk, as one state from Rig's ask to the end of its session.
//!
//! Pure: the caller feeds [`Desk::step`] what happened (Rig's answer, the person's, the transport's) with the time, and
//! carries out the effects it returns. The prompt is up exactly while a request is [`State::Prompting`]; a request is
//! [`State::Joined`] only once a real login carried it; a device answer counts only when no agent could have pressed it;
//! at most one agent is joined, and a second one joining ends the first's session as switched.

use std::collections::BTreeMap;

/// Milliseconds on the caller's clock; only compared, never read from a clock here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Millis(pub u64);

/// Rig's id for the operator's ask, which names the request everywhere.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ask(String);

impl Ask {
    pub fn new(id: &str) -> Result<Self, Invalid> {
        if id.trim().is_empty() {
            return Err(Invalid::EmptyAsk);
        }
        Ok(Self(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    pub id: String,
    pub title: String,
    pub graph: String,
}

/// What the agent asks to do, as the prompt names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// See the screen and use the pointer and keyboard.
    Desktop,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::Desktop => "desktop",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub ask: Ask,
    pub agent: Agent,
    /// The Rig operator the agent works for, shown on the prompt.
    pub operator: String,
    pub scope: Scope,
    /// Past this with no session, the request lapses.
    pub lapses_at: Millis,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invalid {
    EmptyAsk,
    EmptyAgent,
    EmptyOperator,
}

impl Request {
    pub fn new(ask: Ask, agent: Agent, operator: &str, scope: Scope, lapses_at: Millis) -> Result<Self, Invalid> {
        if agent.id.trim().is_empty() {
            return Err(Invalid::EmptyAgent);
        }
        if operator.trim().is_empty() {
            return Err(Invalid::EmptyOperator);
        }
        Ok(Self { ask, agent, operator: operator.to_owned(), scope, lapses_at })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    Person,
    Operator,
}

/// How a session came to carry the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// The connection the person allowed carries the session (a stock PC's knock).
    Adopted,
    /// The saved password logged in.
    Password,
    /// The RigDesk PC admitted Rig's join.
    Join,
    /// The person's invite was claimed.
    Invite,
    /// The session another agent held was handed on.
    Handover,
}

/// The operator accepted; where the saved password stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Password {
    Trying,
    Refused,
    /// None is kept for this desk.
    Missing,
}

/// What this desk can admit on the operator's Accept alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// A saved password, tried once.
    Password,
    /// A RigDesk desktop's switch for sessions started from Rig.
    JoinSwitch,
    Neither,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Withdrawal {
    ByRig,
    /// The same agent asked again for this desk.
    Replaced,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndReason {
    Left,
    RevokedByPerson,
    RevokedByOperator,
    ConnectionLost,
    SwitchedTo(Agent),
    ServiceStopped,
    /// A yes came, but no login carried it and the device cannot be asked.
    LoginFailed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Asked; the device has not said it shows the prompt. `unshown` is why it will not.
    Requested { unshown: Option<String>, operator_said: Option<Password> },
    Prompting { operator_said: Option<Password> },
    /// A yes is in and the login is under way.
    Allowed { by: By },
    Joined { since: Millis, route: Route },
    Denied { by: By },
    Withdrawn { why: Withdrawal },
    Lapsed,
    Ended { joined: bool, reason: EndReason },
}

impl State {
    pub fn name(&self) -> &'static str {
        match self {
            State::Requested { .. } => "requested",
            State::Prompting { .. } => "prompting",
            State::Allowed { .. } => "allowed",
            State::Joined { .. } => "joined",
            State::Denied { .. } => "denied",
            State::Withdrawn { .. } => "withdrawn",
            State::Lapsed => "lapsed",
            State::Ended { .. } => "ended",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, State::Denied { .. } | State::Withdrawn { .. } | State::Lapsed | State::Ended { .. })
    }

    /// The device may be showing the prompt: it shows, or showing it is under way.
    fn prompt_may_show(&self) -> bool {
        matches!(self, State::Prompting { .. } | State::Requested { unshown: None, .. })
    }

    fn before_join(&self) -> bool {
        matches!(self, State::Requested { .. } | State::Prompting { .. } | State::Allowed { .. })
    }

    fn operator_said(&self) -> Option<Password> {
        match self {
            State::Requested { operator_said, .. } | State::Prompting { operator_said } => *operator_said,
            _ => None,
        }
    }

    fn with_operator_said(&self, said: Password) -> State {
        match self {
            State::Requested { unshown, .. } => State::Requested { unshown: unshown.clone(), operator_said: Some(said) },
            _ => State::Prompting { operator_said: Some(said) },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Request(Request),
    Shown(Ask),
    Unshown { ask: Ask, reason: String },
    /// The person's Allow. `agent_can_press`: an agent's input reaches this device now, so it could have pressed it.
    Allow { ask: Ask, agent_can_press: bool },
    Dismiss { ask: Ask, agent_can_press: bool },
    Accept { ask: Ask, admission: Admission },
    Deny(Ask),
    Withdraw(Ask),
    LoginOk { ask: Ask, route: Route },
    /// The saved password was refused.
    LoginRefused(Ask),
    LoginFailed(Ask),
    End { ask: Ask, reason: EndReason },
    /// Only the time moved.
    Tick,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    ShowPrompt(Ask),
    HidePrompt(Ask),
    TryPassword(Ask),
    /// Let the agent in on this yes: adopt the allowed connection, join, or hand the held session on.
    Admit { ask: Ask, by: By },
    EndSession { ask: Ask, reason: EndReason },
    /// A login came for a request no longer wanting one: close it.
    DropLogin(Ask),
}

/// A request's new state, for whoever tells Rig, the agent and the person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub ask: Ask,
    pub agent: Agent,
    pub state: State,
    pub at: Millis,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No such request open: never asked, or already settled.
    NotOpen(Ask),
    Duplicate(Ask),
    AlreadyJoined(Ask),
    /// A device answer while an agent's input reaches the device.
    AgentCanPress(Ask),
    /// The answer does not fit where the request stands.
    OutOfTurn(Ask),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Step {
    pub changes: Vec<Change>,
    pub effects: Vec<Effect>,
    pub refused: Option<Refusal>,
}

#[derive(Clone, Debug)]
struct Entry {
    request: Request,
    state: State,
}

/// One device's open requests. Settled ones are dropped once their change is returned.
#[derive(Clone, Debug, Default)]
pub struct Desk {
    open: BTreeMap<Ask, Entry>,
}

impl Desk {
    pub fn new() -> Self {
        Self::default()
    }

    /// The requests whose prompt shows.
    pub fn prompts(&self) -> impl Iterator<Item = &Request> {
        self.open.values().filter(|e| matches!(e.state, State::Prompting { .. })).map(|e| &e.request)
    }

    pub fn open(&self) -> impl Iterator<Item = (&Request, &State)> {
        self.open.values().map(|e| (&e.request, &e.state))
    }

    pub fn state(&self, ask: &Ask) -> Option<&State> {
        self.open.get(ask).map(|e| &e.state)
    }

    pub fn joined(&self) -> Option<&Request> {
        self.open.values().find(|e| matches!(e.state, State::Joined { .. })).map(|e| &e.request)
    }

    pub fn step(&mut self, now: Millis, event: Event) -> Step {
        let mut step = Step::default();
        self.lapse(now, &mut step);
        if let Err(refusal) = self.apply(now, event, &mut step) {
            step.refused = Some(refusal);
        }
        self.lapse(now, &mut step);
        step
    }

    fn apply(&mut self, now: Millis, event: Event, step: &mut Step) -> Result<(), Refusal> {
        match event {
            Event::Tick => Ok(()),
            Event::Request(request) => self.request(now, request, step),
            Event::Shown(ask) => match self.current(&ask)? {
                State::Requested { operator_said, .. } => {
                    self.set(now, &ask, State::Prompting { operator_said }, step);
                    Ok(())
                }
                _ => Err(Refusal::OutOfTurn(ask)),
            },
            Event::Unshown { ask, reason } => match self.current(&ask)? {
                State::Requested { operator_said, .. } => {
                    let next = match operator_said {
                        Some(Password::Refused | Password::Missing) => {
                            State::Ended { joined: false, reason: EndReason::LoginFailed }
                        }
                        _ => State::Requested { unshown: Some(reason), operator_said },
                    };
                    self.set(now, &ask, next, step);
                    Ok(())
                }
                _ => Err(Refusal::OutOfTurn(ask)),
            },
            Event::Allow { ask, agent_can_press } => {
                let state = self.current(&ask)?;
                if !state.prompt_may_show() {
                    return Err(Refusal::OutOfTurn(ask));
                }
                if agent_can_press {
                    return Err(Refusal::AgentCanPress(ask));
                }
                self.set(now, &ask, State::Allowed { by: By::Person }, step);
                step.effects.push(Effect::Admit { ask, by: By::Person });
                Ok(())
            }
            Event::Dismiss { ask, agent_can_press } => {
                let state = self.current(&ask)?;
                if !state.prompt_may_show() {
                    return Err(Refusal::OutOfTurn(ask));
                }
                if agent_can_press {
                    return Err(Refusal::AgentCanPress(ask));
                }
                self.set(now, &ask, State::Denied { by: By::Person }, step);
                Ok(())
            }
            Event::Accept { ask, admission } => self.accept(now, ask, admission, step),
            Event::Deny(ask) => {
                let state = self.current(&ask)?;
                if !state.before_join() {
                    return Err(Refusal::OutOfTurn(ask));
                }
                self.drop_login_if_under_way(&ask, &state, step);
                self.set(now, &ask, State::Denied { by: By::Operator }, step);
                Ok(())
            }
            Event::Withdraw(ask) => {
                let state = self.current(&ask)?;
                if !state.before_join() {
                    return Err(Refusal::OutOfTurn(ask));
                }
                self.drop_login_if_under_way(&ask, &state, step);
                self.set(now, &ask, State::Withdrawn { why: Withdrawal::ByRig }, step);
                Ok(())
            }
            Event::LoginOk { ask, route } => {
                let wanted = match self.open.get(&ask).map(|e| &e.state) {
                    Some(State::Allowed { .. }) => true,
                    Some(state) => state.operator_said() == Some(Password::Trying),
                    None => false,
                };
                if !wanted {
                    step.effects.push(Effect::DropLogin(ask.clone()));
                    return Err(if self.open.contains_key(&ask) { Refusal::OutOfTurn(ask) } else { Refusal::NotOpen(ask) });
                }
                self.join(now, &ask, route, step);
                Ok(())
            }
            Event::LoginRefused(ask) | Event::LoginFailed(ask) => {
                let state = self.current(&ask)?;
                let next = match &state {
                    State::Allowed { .. } => State::Ended { joined: false, reason: EndReason::LoginFailed },
                    State::Requested { unshown: Some(_), operator_said: Some(Password::Trying) } => {
                        State::Ended { joined: false, reason: EndReason::LoginFailed }
                    }
                    s if s.operator_said() == Some(Password::Trying) => s.with_operator_said(Password::Refused),
                    _ => return Err(Refusal::OutOfTurn(ask)),
                };
                self.set(now, &ask, next, step);
                Ok(())
            }
            Event::End { ask, reason } => match self.current(&ask)? {
                State::Joined { .. } => {
                    self.set(now, &ask, State::Ended { joined: true, reason }, step);
                    Ok(())
                }
                _ => Err(Refusal::OutOfTurn(ask)),
            },
        }
    }

    fn request(&mut self, now: Millis, request: Request, step: &mut Step) -> Result<(), Refusal> {
        let ask = request.ask.clone();
        if self.open.contains_key(&ask) {
            return Err(Refusal::Duplicate(ask));
        }
        if self.joined().is_some_and(|joined| joined.agent.id == request.agent.id) {
            return Err(Refusal::AlreadyJoined(ask));
        }
        let replaced: Vec<(Ask, State)> = self
            .open
            .values()
            .filter(|e| e.request.agent.id == request.agent.id && e.state.before_join())
            .map(|e| (e.request.ask.clone(), e.state.clone()))
            .collect();
        for (old, state) in replaced {
            self.drop_login_if_under_way(&old, &state, step);
            self.set(now, &old, State::Withdrawn { why: Withdrawal::Replaced }, step);
        }
        let agent = request.agent.clone();
        let state = State::Requested { unshown: None, operator_said: None };
        self.open.insert(ask.clone(), Entry { request, state: state.clone() });
        step.changes.push(Change { ask: ask.clone(), agent, state, at: now });
        step.effects.push(Effect::ShowPrompt(ask));
        Ok(())
    }

    fn accept(&mut self, now: Millis, ask: Ask, admission: Admission, step: &mut Step) -> Result<(), Refusal> {
        let state = self.current(&ask)?;
        if !matches!(state, State::Requested { .. } | State::Prompting { .. }) || state.operator_said().is_some() {
            return Err(Refusal::OutOfTurn(ask));
        }
        // With an agent joined, the operator's yes alone never hands its session on (R3): neither the switch nor the password
        // admits, and the person's Allow, once no agent can press it, is the only way the session moves.
        let held = self.joined().is_some();
        if admission == Admission::JoinSwitch && !held {
            self.set(now, &ask, State::Allowed { by: By::Operator }, step);
            step.effects.push(Effect::Admit { ask, by: By::Operator });
            return Ok(());
        }
        let next = match (admission, &state) {
            (Admission::Password, _) if !held => {
                step.effects.push(Effect::TryPassword(ask.clone()));
                state.with_operator_said(Password::Trying)
            }
            (_, State::Requested { unshown: Some(_), .. }) => State::Ended { joined: false, reason: EndReason::LoginFailed },
            _ => state.with_operator_said(Password::Missing),
        };
        self.set(now, &ask, next, step);
        Ok(())
    }

    fn join(&mut self, now: Millis, ask: &Ask, route: Route, step: &mut Step) {
        let Some(agent) = self.open.get(ask).map(|e| e.request.agent.clone()) else {
            return;
        };
        let previous: Vec<Ask> = self
            .open
            .values()
            .filter(|e| matches!(e.state, State::Joined { .. }))
            .map(|e| e.request.ask.clone())
            .collect();
        for old in previous {
            let reason = EndReason::SwitchedTo(agent.clone());
            step.effects.push(Effect::EndSession { ask: old.clone(), reason: reason.clone() });
            self.set(now, &old, State::Ended { joined: true, reason }, step);
        }
        self.set(now, ask, State::Joined { since: now, route }, step);
    }

    fn lapse(&mut self, now: Millis, step: &mut Step) {
        let due: Vec<(Ask, State)> = self
            .open
            .values()
            .filter(|e| e.state.before_join() && e.request.lapses_at <= now)
            .map(|e| (e.request.ask.clone(), e.state.clone()))
            .collect();
        for (ask, state) in due {
            self.drop_login_if_under_way(&ask, &state, step);
            self.set(now, &ask, State::Lapsed, step);
        }
    }

    fn drop_login_if_under_way(&self, ask: &Ask, state: &State, step: &mut Step) {
        if matches!(state, State::Allowed { .. }) || state.operator_said() == Some(Password::Trying) {
            step.effects.push(Effect::DropLogin(ask.clone()));
        }
    }

    fn current(&self, ask: &Ask) -> Result<State, Refusal> {
        self.open.get(ask).map(|e| e.state.clone()).ok_or_else(|| Refusal::NotOpen(ask.clone()))
    }

    /// Moves a request on: the prompt goes when it can no longer be answered, and a settled request is dropped.
    fn set(&mut self, now: Millis, ask: &Ask, next: State, step: &mut Step) {
        let Some(entry) = self.open.get_mut(ask) else {
            return;
        };
        if entry.state.prompt_may_show() && !next.prompt_may_show() {
            step.effects.push(Effect::HidePrompt(ask.clone()));
        }
        entry.state = next.clone();
        step.changes.push(Change { ask: ask.clone(), agent: entry.request.agent.clone(), state: next.clone(), at: now });
        if next.is_terminal() {
            self.open.remove(ask);
        }
    }
}

#[cfg(test)]
mod tests;
