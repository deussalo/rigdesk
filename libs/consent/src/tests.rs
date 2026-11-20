use super::*;
use std::collections::BTreeSet;

const T0: Millis = Millis(1_000);
const LAPSE: Millis = Millis(181_000);

fn agent(id: &str) -> Agent {
    Agent { id: id.into(), title: format!("Agent {id}"), graph: "g".into() }
}

fn ask(id: &str) -> Ask {
    Ask::new(id).unwrap()
}

fn request(id: &str, agent_id: &str) -> Event {
    Event::Request(Request::new(ask(id), agent(agent_id), "Arian", Scope::Desktop, LAPSE).unwrap())
}

fn prompting(desk: &mut Desk, id: &str, agent_id: &str) {
    desk.step(T0, request(id, agent_id));
    desk.step(T0, Event::Shown(ask(id)));
}

fn joined_by_allow(desk: &mut Desk, id: &str, agent_id: &str) {
    prompting(desk, id, agent_id);
    desk.step(T0, Event::Allow { ask: ask(id), agent_can_press: false });
    desk.step(T0, Event::LoginOk { ask: ask(id), route: Route::Adopted });
}

fn names(step: &Step) -> Vec<&'static str> {
    step.changes.iter().map(|c| c.state.name()).collect()
}

#[test]
fn constructors_refuse_empty_ids() {
    assert_eq!(Ask::new(" "), Err(Invalid::EmptyAsk));
    assert_eq!(Request::new(ask("a"), agent(""), "Arian", Scope::Desktop, LAPSE), Err(Invalid::EmptyAgent));
    assert_eq!(Request::new(ask("a"), agent("x"), "", Scope::Desktop, LAPSE), Err(Invalid::EmptyOperator));
}

#[test]
fn a_request_asks_the_device_to_show_and_prompts_once_shown() {
    let mut desk = Desk::new();
    let step = desk.step(T0, request("a", "x"));
    assert_eq!(names(&step), ["requested"]);
    assert_eq!(step.effects, [Effect::ShowPrompt(ask("a"))]);
    assert_eq!(desk.prompts().count(), 0);
    let step = desk.step(T0, Event::Shown(ask("a")));
    assert_eq!(names(&step), ["prompting"]);
    assert_eq!(desk.prompts().map(|r| r.operator.as_str()).collect::<Vec<_>>(), ["Arian"]);
}

#[test]
fn the_persons_allow_admits_and_joined_comes_only_with_the_login() {
    let mut desk = Desk::new();
    prompting(&mut desk, "a", "x");
    let step = desk.step(T0, Event::Allow { ask: ask("a"), agent_can_press: false });
    assert_eq!(names(&step), ["allowed"]);
    assert_eq!(step.effects, [Effect::HidePrompt(ask("a")), Effect::Admit { ask: ask("a"), by: By::Person }]);
    assert!(desk.joined().is_none());
    let step = desk.step(Millis(2_000), Event::LoginOk { ask: ask("a"), route: Route::Adopted });
    assert_eq!(step.changes[0].state, State::Joined { since: Millis(2_000), route: Route::Adopted });
    assert_eq!(desk.joined().unwrap().agent.id, "x");
}

#[test]
fn a_device_answer_an_agent_could_have_pressed_changes_nothing() {
    let mut desk = Desk::new();
    prompting(&mut desk, "a", "x");
    for event in [
        Event::Allow { ask: ask("a"), agent_can_press: true },
        Event::Dismiss { ask: ask("a"), agent_can_press: true },
    ] {
        let step = desk.step(T0, event);
        assert_eq!(step.refused, Some(Refusal::AgentCanPress(ask("a"))));
        assert!(step.changes.is_empty() && step.effects.is_empty());
    }
    assert_eq!(desk.prompts().count(), 1);
}

#[test]
fn the_persons_dismiss_denies_and_takes_the_prompt_down() {
    let mut desk = Desk::new();
    prompting(&mut desk, "a", "x");
    let step = desk.step(T0, Event::Dismiss { ask: ask("a"), agent_can_press: false });
    assert_eq!(step.changes[0].state, State::Denied { by: By::Person });
    assert_eq!(step.effects, [Effect::HidePrompt(ask("a"))]);
    assert_eq!(desk.open().count(), 0);
}

#[test]
fn the_operators_accept_with_a_password_keeps_the_prompt_up_while_it_is_tried() {
    let mut desk = Desk::new();
    prompting(&mut desk, "a", "x");
    let step = desk.step(T0, Event::Accept { ask: ask("a"), admission: Admission::Password });
    assert_eq!(step.changes[0].state, State::Prompting { operator_said: Some(Password::Trying) });
    assert_eq!(step.effects, [Effect::TryPassword(ask("a"))]);
    assert_eq!(desk.prompts().count(), 1);
    let step = desk.step(T0, Event::LoginOk { ask: ask("a"), route: Route::Password });
    assert_eq!(names(&step), ["joined"]);
    assert_eq!(step.effects, [Effect::HidePrompt(ask("a"))]);
}

#[test]
fn a_refused_password_leaves_the_prompt_for_the_person_whose_allow_then_joins() {
    let mut desk = Desk::new();
    prompting(&mut desk, "a", "x");
    desk.step(T0, Event::Accept { ask: ask("a"), admission: Admission::Password });
    let step = desk.step(T0, Event::LoginRefused(ask("a")));
    assert_eq!(step.changes[0].state, State::Prompting { operator_said: Some(Password::Refused) });
    assert!(step.effects.is_empty());
    assert_eq!(desk.prompts().count(), 1);
    desk.step(T0, Event::Allow { ask: ask("a"), agent_can_press: false });
    let step = desk.step(T0, Event::LoginOk { ask: ask("a"), route: Route::Adopted });
    assert_eq!(names(&step), ["joined"]);
}

#[test]
fn with_no_password_kept_the_operators_accept_waits_for_the_person() {
    let mut desk = Desk::new();
    prompting(&mut desk, "a", "x");
    let step = desk.step(T0, Event::Accept { ask: ask("a"), admission: Admission::Neither });
    assert_eq!(step.changes[0].state, State::Prompting { operator_said: Some(Password::Missing) });
    assert!(step.effects.is_empty());
    assert!(desk.joined().is_none());
}

#[test]
fn the_persons_dismiss_after_the_operators_accept_wins_and_a_late_login_is_dropped() {
    let mut desk = Desk::new();
    prompting(&mut desk, "a", "x");
    desk.step(T0, Event::Accept { ask: ask("a"), admission: Admission::Password });
    let step = desk.step(T0, Event::Dismiss { ask: ask("a"), agent_can_press: false });
    assert_eq!(step.changes[0].state, State::Denied { by: By::Person });
    let step = desk.step(T0, Event::LoginOk { ask: ask("a"), route: Route::Password });
    assert_eq!(step.refused, Some(Refusal::NotOpen(ask("a"))));
    assert_eq!(step.effects, [Effect::DropLogin(ask("a"))]);
    assert!(desk.joined().is_none());
}

#[test]
fn a_device_that_will_not_show_says_why_and_joins_only_by_password() {
    let mut desk = Desk::new();
    desk.step(T0, request("a", "x"));
    let step = desk.step(T0, Event::Unshown { ask: ask("a"), reason: "bridge predates /offer".into() });
    assert_eq!(step.changes[0].state, State::Requested { unshown: Some("bridge predates /offer".into()), operator_said: None });
    assert_eq!(step.effects, [Effect::HidePrompt(ask("a"))]);
    let step = desk.step(T0, Event::Accept { ask: ask("a"), admission: Admission::Password });
    assert_eq!(step.effects, [Effect::TryPassword(ask("a"))]);
    let step = desk.step(T0, Event::LoginRefused(ask("a")));
    assert_eq!(step.changes[0].state, State::Ended { joined: false, reason: EndReason::LoginFailed });

    let mut desk = Desk::new();
    desk.step(T0, request("b", "x"));
    desk.step(T0, Event::Unshown { ask: ask("b"), reason: "unreachable".into() });
    let step = desk.step(T0, Event::Accept { ask: ask("b"), admission: Admission::Neither });
    assert_eq!(step.changes[0].state, State::Ended { joined: false, reason: EndReason::LoginFailed });
}

#[test]
fn a_rigdesk_desktops_switch_admits_on_the_operators_accept() {
    let mut desk = Desk::new();
    prompting(&mut desk, "a", "x");
    let step = desk.step(T0, Event::Accept { ask: ask("a"), admission: Admission::JoinSwitch });
    assert_eq!(names(&step), ["allowed"]);
    assert_eq!(step.effects, [Effect::HidePrompt(ask("a")), Effect::Admit { ask: ask("a"), by: By::Operator }]);
    desk.step(T0, Event::LoginOk { ask: ask("a"), route: Route::Join });
    assert!(desk.joined().is_some());
}

#[test]
fn a_failed_login_after_a_yes_ends_the_request_unjoined() {
    let mut desk = Desk::new();
    prompting(&mut desk, "a", "x");
    desk.step(T0, Event::Allow { ask: ask("a"), agent_can_press: false });
    let step = desk.step(T0, Event::LoginFailed(ask("a")));
    assert_eq!(step.changes[0].state, State::Ended { joined: false, reason: EndReason::LoginFailed });
}

#[test]
fn the_operators_deny_and_rigs_withdrawal_take_the_prompt_down() {
    for (event, state) in [
        (Event::Deny(ask("a")), State::Denied { by: By::Operator }),
        (Event::Withdraw(ask("a")), State::Withdrawn { why: Withdrawal::ByRig }),
    ] {
        let mut desk = Desk::new();
        prompting(&mut desk, "a", "x");
        let step = desk.step(T0, event);
        assert_eq!(step.changes[0].state, state);
        assert_eq!(step.effects, [Effect::HidePrompt(ask("a"))]);
    }
}

#[test]
fn a_withdrawal_while_the_login_is_under_way_drops_it() {
    let mut desk = Desk::new();
    prompting(&mut desk, "a", "x");
    desk.step(T0, Event::Allow { ask: ask("a"), agent_can_press: false });
    let step = desk.step(T0, Event::Withdraw(ask("a")));
    assert_eq!(step.effects, [Effect::DropLogin(ask("a"))]);
}

#[test]
fn an_unanswered_request_lapses_on_time_and_the_prompt_goes() {
    let mut desk = Desk::new();
    prompting(&mut desk, "a", "x");
    assert!(desk.step(Millis(LAPSE.0 - 1), Event::Tick).changes.is_empty());
    let step = desk.step(LAPSE, Event::Tick);
    assert_eq!(step.changes[0].state, State::Lapsed);
    assert_eq!(step.effects, [Effect::HidePrompt(ask("a"))]);
    let step = desk.step(LAPSE, Event::Allow { ask: ask("a"), agent_can_press: false });
    assert_eq!(step.refused, Some(Refusal::NotOpen(ask("a"))));
}

#[test]
fn a_joined_session_does_not_lapse_and_ends_with_its_reason() {
    let mut desk = Desk::new();
    joined_by_allow(&mut desk, "a", "x");
    assert!(desk.step(Millis(LAPSE.0 * 2), Event::Tick).changes.is_empty());
    let step = desk.step(Millis(LAPSE.0 * 2), Event::End { ask: ask("a"), reason: EndReason::RevokedByPerson });
    assert_eq!(step.changes[0].state, State::Ended { joined: true, reason: EndReason::RevokedByPerson });
    assert!(desk.joined().is_none());
}

#[test]
fn answers_to_a_settled_request_are_late_and_change_nothing() {
    let mut desk = Desk::new();
    prompting(&mut desk, "a", "x");
    desk.step(T0, Event::Deny(ask("a")));
    for event in [
        Event::Allow { ask: ask("a"), agent_can_press: false },
        Event::Dismiss { ask: ask("a"), agent_can_press: false },
        Event::Accept { ask: ask("a"), admission: Admission::JoinSwitch },
        Event::Deny(ask("a")),
        Event::Withdraw(ask("a")),
        Event::Shown(ask("a")),
        Event::LoginRefused(ask("a")),
        Event::End { ask: ask("a"), reason: EndReason::Left },
    ] {
        let step = desk.step(T0, event);
        assert_eq!(step.refused, Some(Refusal::NotOpen(ask("a"))));
        assert!(step.changes.is_empty() && step.effects.is_empty());
    }
}

#[test]
fn the_operator_answers_once() {
    let mut desk = Desk::new();
    prompting(&mut desk, "a", "x");
    desk.step(T0, Event::Accept { ask: ask("a"), admission: Admission::Neither });
    let step = desk.step(T0, Event::Accept { ask: ask("a"), admission: Admission::Password });
    assert_eq!(step.refused, Some(Refusal::OutOfTurn(ask("a"))));
}

#[test]
fn the_same_agent_asking_again_replaces_its_open_request() {
    let mut desk = Desk::new();
    prompting(&mut desk, "a", "x");
    let step = desk.step(T0, request("b", "x"));
    assert_eq!(step.changes[0].state, State::Withdrawn { why: Withdrawal::Replaced });
    assert_eq!(step.effects, [Effect::HidePrompt(ask("a")), Effect::ShowPrompt(ask("b"))]);
    assert_eq!(desk.open().count(), 1);
    assert_eq!(desk.step(T0, request("b", "y")).refused, Some(Refusal::Duplicate(ask("b"))));
}

#[test]
fn an_agent_already_joined_cannot_ask_again() {
    let mut desk = Desk::new();
    joined_by_allow(&mut desk, "a", "x");
    assert_eq!(desk.step(T0, request("b", "x")).refused, Some(Refusal::AlreadyJoined(ask("b"))));
}

#[test]
fn several_agents_prompt_at_once_and_the_first_yes_leaves_the_others_up() {
    let mut desk = Desk::new();
    prompting(&mut desk, "a", "x");
    prompting(&mut desk, "b", "y");
    assert_eq!(desk.prompts().count(), 2);
    desk.step(T0, Event::Allow { ask: ask("a"), agent_can_press: false });
    assert_eq!(desk.prompts().map(|r| r.agent.id.as_str()).collect::<Vec<_>>(), ["y"]);
}

#[test]
fn switching_hands_the_session_on_with_one_operator_accept_and_tells_the_old_holder() {
    let mut desk = Desk::new();
    joined_by_allow(&mut desk, "a", "x");
    prompting(&mut desk, "b", "y");
    // The operator's word alone moves the session: it already carries a yes.
    let step = desk.step(T0, Event::Accept { ask: ask("b"), admission: Admission::Neither });
    assert_eq!(step.effects, [Effect::HidePrompt(ask("b")), Effect::Admit { ask: ask("b"), by: By::Operator }]);
    assert_eq!(desk.joined().unwrap().agent.id, "x", "the old holder keeps the session until the new one is in");
    let step = desk.step(Millis(5_000), Event::LoginOk { ask: ask("b"), route: Route::Handover });
    assert_eq!(step.effects, [Effect::EndSession { ask: ask("a"), reason: EndReason::SwitchedTo(agent("y")) }]);
    assert_eq!(
        step.changes.iter().map(|c| (c.agent.id.as_str(), c.state.name())).collect::<Vec<_>>(),
        [("x", "ended"), ("y", "joined")]
    );
    assert_eq!(desk.joined().unwrap().agent.id, "y");
}

#[test]
fn a_switch_whose_login_fails_leaves_the_old_holder_in() {
    let mut desk = Desk::new();
    joined_by_allow(&mut desk, "a", "x");
    prompting(&mut desk, "b", "y");
    desk.step(T0, Event::Accept { ask: ask("b"), admission: Admission::Neither });
    desk.step(T0, Event::LoginFailed(ask("b")));
    assert_eq!(desk.joined().unwrap().agent.id, "x");
}

#[test]
fn the_person_approves_a_switch_only_once_no_agent_can_press() {
    let mut desk = Desk::new();
    joined_by_allow(&mut desk, "a", "x");
    prompting(&mut desk, "b", "y");
    let step = desk.step(T0, Event::Allow { ask: ask("b"), agent_can_press: true });
    assert_eq!(step.refused, Some(Refusal::AgentCanPress(ask("b"))));
    // The person took control: x's input no longer reaches the device.
    let step = desk.step(T0, Event::Allow { ask: ask("b"), agent_can_press: false });
    assert_eq!(names(&step), ["allowed"]);
}

/// Every sequence of events keeps the invariants: the prompt shows only while prompting, at most one agent is joined,
/// joined comes only from a login, a device answer an agent could press changes nothing, and nothing settled moves.
#[test]
fn random_sequences_keep_the_invariants() {
    let asks = ["a", "b", "c"];
    let agents = ["x", "y"];
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let (mut joins, mut switches, mut pressed, mut lapses) = (0, 0, 0, 0);
    for _ in 0..2_000 {
        let mut desk = Desk::new();
        let mut may_show: BTreeSet<Ask> = BTreeSet::new();
        let mut settled: BTreeSet<Ask> = BTreeSet::new();
        let mut now = 0u64;
        for _ in 0..60 {
            now += next(4_000);
            let a = ask(asks[next(3) as usize]);
            let reason = [EndReason::Left, EndReason::RevokedByOperator, EndReason::ConnectionLost][next(3) as usize].clone();
            let admission = [Admission::Password, Admission::JoinSwitch, Admission::Neither][next(3) as usize];
            let route = [Route::Adopted, Route::Password, Route::Join, Route::Handover][next(4) as usize];
            let press = next(2) == 0;
            let event = match next(13) {
                0 | 1 => Event::Request(
                    Request::new(a.clone(), agent(agents[next(2) as usize]), "Arian", Scope::Desktop, Millis(now + 30_000 + next(300_000)))
                        .unwrap(),
                ),
                2 => Event::Shown(a.clone()),
                3 => Event::Unshown { ask: a.clone(), reason: "r".into() },
                4 => Event::Allow { ask: a.clone(), agent_can_press: press },
                5 => Event::Dismiss { ask: a.clone(), agent_can_press: press },
                6 => Event::Accept { ask: a.clone(), admission },
                7 => Event::Deny(a.clone()),
                8 => Event::Withdraw(a.clone()),
                9 => Event::LoginOk { ask: a.clone(), route },
                10 => Event::LoginRefused(a.clone()),
                11 => Event::End { ask: a.clone(), reason },
                _ => Event::Tick,
            };
            let device_answer_pressable = matches!(
                event,
                Event::Allow { agent_can_press: true, .. } | Event::Dismiss { agent_can_press: true, .. }
            );
            let is_login = matches!(event, Event::LoginOk { .. });
            let is_request = matches!(event, Event::Request(_));
            let before: Vec<(Ask, State)> = desk.open().map(|(r, s)| (r.ask.clone(), s.clone())).collect();
            let step = desk.step(Millis(now), event);
            for effect in &step.effects {
                match effect {
                    Effect::ShowPrompt(x) => {
                        may_show.insert(x.clone());
                    }
                    Effect::HidePrompt(x) => {
                        may_show.remove(x);
                    }
                    _ => {}
                }
            }
            for change in &step.changes {
                // A settled ask's id may name a new request: its first state starts it afresh.
                if is_request && step.refused.is_none() && change.ask == a && change.state == (State::Requested { unshown: None, operator_said: None }) {
                    settled.remove(&a);
                }
                if change.state.is_terminal() {
                    settled.insert(change.ask.clone());
                }
                match &change.state {
                    State::Joined { .. } => {
                        assert!(is_login, "joined without a login: {change:?}");
                        joins += 1;
                    }
                    State::Ended { reason: EndReason::SwitchedTo(_), .. } => switches += 1,
                    State::Lapsed => lapses += 1,
                    _ => {}
                }
                if settled.contains(&change.ask) && !change.state.is_terminal() {
                    panic!("a settled request moved: {change:?}");
                }
            }
            if device_answer_pressable && matches!(step.refused, Some(Refusal::AgentCanPress(_))) {
                pressed += 1;
                let after: Vec<(Ask, State)> = desk.open().map(|(r, s)| (r.ask.clone(), s.clone())).collect();
                let lapsed_only = step.changes.iter().all(|c| c.state == State::Lapsed);
                assert!(lapsed_only, "a pressable answer changed something: {before:?} -> {after:?}");
            }
            for shown in &may_show {
                let state = desk.state(shown);
                assert!(state.is_some_and(State::prompt_may_show), "prompt for {shown:?} outlived its request: {state:?}");
            }
            assert!(desk.open().filter(|(_, s)| matches!(s, State::Joined { .. })).count() <= 1);
        }
    }
    assert!(joins > 0 && switches > 0 && pressed > 0 && lapses > 0, "{joins} {switches} {pressed} {lapses}");
}
