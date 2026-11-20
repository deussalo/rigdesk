//! RigDesk Android: a Rig agent's knock, the passwordless login that asks the person at the phone to let the agent in.
//!
//! RigDesk takes passwords only, so the phone's Allow exists for one login alone: a knock Rig signed with the key this
//! device was paired with (`invite::pair`). The signature covers this connection's greeting, whose nonce is new for every
//! connection, so a proof seen once opens nothing later. It also covers the address the knock comes from: Rig signs any
//! greeting a PC it dials shows it, and without the address another PC could show Rig this device's greeting and bring
//! the proof here itself. The knock says which address Rig signed, so a good signature from an unexpected address can be
//! told apart from a bad one.
//!
//! A knock may also vouch for one more fact, under a domain of its own, so a proof for one fact proves no other
//! ([`Vouch`]): the ask the person already accepted on Rig Assistant's notification (`one_tap`), which opens that ask
//! alone, or that the operator assigned this phone to the agent, which the phone may let in with no tap while its person
//! allows it in their settings.
//!
//! Anything else that calls itself Rig gets no prompt, no Allow and keeps no name of its own on this device.
//!
//! Rig signs its saved-password login the same way, so a session it opens with the password names its agent, and the ask
//! the operator accepted, rather than going unnamed. Each session in is then told to the person as who joined and how
//! ([`Joined`]): never a "Do you accept?" for a login that is already in.

use crate::{invite, message_proto::{LoginRequest, RigKnock}};
use hbb_common::{
    rand::{thread_rng, RngCore},
    sodiumoxide::crypto::sign,
};
use lazy_static::lazy_static;
use std::{collections::HashMap, net::IpAddr, sync::Mutex};

const DOMAIN: &str = "rigdesk-knock-v1";
const DOMAIN_ASK: &str = "rigdesk-knock-ask-v1";
const DOMAIN_ASSIGNED: &str = "rigdesk-knock-assigned-v1";
const NONCE_BYTES: usize = 32;
const CLAIMED_NAME: &str = "rig agent";
/// The login id of Rig's knock, which the phone's notification goes by.
pub const PEER_ID: &str = "rig";
/// What the device shows for a login that got in by password while calling itself Rig.
pub const NEUTRAL_PEER_ID: &str = "password login";
pub const NEUTRAL_NAME: &str = "Remote session";
/// The person's own setting on this phone, a local option they alone change here, off unless "Y": an agent Rig says the
/// operator assigned this phone to joins with no tap while sharing runs. Rig reads it from Rig Assistant's hello.
pub const ASSIGNED_NO_TAP: &str = "rig-assigned-no-tap";

/// One connection's side of a knock: what the device greeted with and whom it is talking to.
pub struct Login<'a> {
    /// The name the login is addressed to, the phone's ID or the address Rig dials it at.
    pub device: &'a str,
    pub salt: &'a str,
    pub challenge: &'a str,
    pub nonce: &'a [u8],
    pub dialer: IpAddr,
}

/// A dual-stack socket reports an IPv4 peer in its mapped form, and the other end knows the plain one.
fn plain(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// What Rig vouches for in a knock beyond its agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Vouch {
    /// Nothing: the person at the phone is asked.
    Nothing,
    /// The ask the person accepted on Rig Assistant's notification, or the operator accepted in Rig for a password login.
    Accepted(String),
    /// The operator assigned this phone to the agent: a desk/join under its standing grant.
    Assigned,
}

impl Vouch {
    /// A knock's vouch, as its fields say it: an ask and assigned together are no knock Rig makes.
    pub fn of(knock: &RigKnock) -> Result<Vouch, Refusal> {
        match (knock.ask.is_empty(), knock.assigned) {
            (true, false) => Ok(Vouch::Nothing),
            (false, false) => Ok(Vouch::Accepted(knock.ask.clone())),
            (true, true) => Ok(Vouch::Assigned),
            (false, true) => Err(Refusal::BadProof),
        }
    }

    /// The knock's fields for this vouch: its ask and assigned.
    pub fn fields(&self) -> (String, bool) {
        match self {
            Vouch::Nothing => (String::new(), false),
            Vouch::Accepted(ask) => (ask.clone(), false),
            Vouch::Assigned => (String::new(), true),
        }
    }
}

impl Login<'_> {
    /// What Rig signs for this connection and this agent, and for what it vouches for.
    pub fn message(&self, agent_id: &str, agent_title: &str, vouch: &Vouch) -> Vec<u8> {
        let dialer = plain(self.dialer).to_string();
        let domain = match vouch {
            Vouch::Nothing => DOMAIN,
            Vouch::Accepted(_) => DOMAIN_ASK,
            Vouch::Assigned => DOMAIN_ASSIGNED,
        };
        let mut fields: Vec<&[u8]> = vec![
            domain.as_bytes(),
            self.device.as_bytes(),
            self.salt.as_bytes(),
            self.challenge.as_bytes(),
            self.nonce,
            dialer.as_bytes(),
            agent_id.as_bytes(),
            agent_title.as_bytes(),
        ];
        if let Vouch::Accepted(ask) = vouch {
            fields.push(ask.as_bytes());
        }
        let mut out = Vec::new();
        for field in fields {
            out.extend_from_slice(&(field.len() as u64).to_le_bytes());
            out.extend_from_slice(field);
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    NotPaired,
    NoProof,
    BadProof,
    /// Rig's signature is good, but for another address than this connection comes from.
    OtherDialer,
}

impl Refusal {
    pub fn words(self) -> &'static str {
        match self {
            Refusal::NotPaired => invite::Refusal::NotPaired.words(),
            Refusal::NoProof | Refusal::BadProof => "This login did not prove it comes from Rig",
            Refusal::OtherDialer => "This login comes from another address than the one Rig signed",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Proved {
    pub agent_id: String,
    /// Built from the title Rig signed.
    pub name: String,
    /// What Rig signed beyond the agent.
    pub vouch: Vouch,
}

/// What a RigDesk desktop answers a knock, before its person sees anything: it is never paired with Rig, so it could
/// show only a Dismiss, and it takes Rig's agents through Rig's own join, whose notice says how the agent got in.
pub const DESKTOP_REFUSAL: &str = "This PC runs RigDesk, which asks its person only about Rig's own join: \
set it up in Rig's settings as a RigDesk PC, with its install line, and desk/request reaches it with no knock";

pub fn fresh_nonce() -> Vec<u8> {
    let mut nonce = vec![0u8; NONCE_BYTES];
    thread_rng().fill_bytes(&mut nonce);
    nonce
}

pub fn claims_rig(peer_id: &str, name: &str) -> bool {
    peer_id.trim().eq_ignore_ascii_case(PEER_ID)
        || peer_id.to_ascii_lowercase().contains(CLAIMED_NAME)
        || name.to_ascii_lowercase().contains(CLAIMED_NAME)
}

/// A login with no password that calls itself Rig has only the proof to stand on. One with a password goes the ordinary
/// way, and the password decides.
pub fn must_prove(login: &LoginRequest) -> bool {
    login.password.is_empty() && (login.rig_knock.is_some() || claims_rig(&login.my_id, &login.my_name))
}

fn check(key: Option<&sign::PublicKey>, knock: Option<&RigKnock>, login: &Login) -> Result<Proved, Refusal> {
    let key = key.ok_or(Refusal::NotPaired)?;
    let knock = knock.ok_or(Refusal::NoProof)?;
    if login.nonce.is_empty() || knock.agent_id.is_empty() || knock.agent_title.trim().is_empty() {
        return Err(Refusal::BadProof);
    }
    let signature = sign::Signature::from_bytes(&knock.signature).map_err(|_| Refusal::BadProof)?;
    let dialer: IpAddr = knock.dialer.parse().map_err(|_| Refusal::BadProof)?;
    let signed = Login { dialer, ..*login };
    let vouch = Vouch::of(knock)?;
    if !sign::verify_detached(&signature, &signed.message(&knock.agent_id, &knock.agent_title, &vouch), key) {
        return Err(Refusal::BadProof);
    }
    if plain(dialer) != plain(login.dialer) {
        return Err(Refusal::OtherDialer);
    }
    Ok(Proved { agent_id: knock.agent_id.clone(), name: format!("{} (Rig agent)", knock.agent_title), vouch })
}

pub fn verify(knock: Option<&RigKnock>, login: &Login) -> Result<Proved, Refusal> {
    check(invite::paired_key().as_ref(), knock, login)
}

/// The id and name the device's connection list gets for a login, or None when it must not be listed at all: only a
/// proved login keeps "Rig" and an agent's name, and one that is not in yet is not shown.
pub fn listed_as(peer_id: String, name: String, authorized: bool, proved: bool) -> Option<(String, String)> {
    if proved || !claims_rig(&peer_id, &name) {
        return Some((peer_id, name));
    }
    authorized.then(|| (NEUTRAL_PEER_ID.to_owned(), NEUTRAL_NAME.to_owned()))
}

/// The phone's own state a no-tap admit is decided on, read when the knock comes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoTapHere {
    /// The person turned on [`ASSIGNED_NO_TAP`].
    pub setting_on: bool,
    /// Screen sharing runs: Android's capture is granted, so its privacy chip shows. Capture can stop while the listener
    /// stays up, and then nothing tells the person an agent is in.
    pub sharing: bool,
}

/// Whether a proved knock may get in with no tap on this phone's own say: Rig vouched that the operator assigned the phone
/// to its agent, the person turned on [`ASSIGNED_NO_TAP`] and sharing runs. RigDesk pins approve-mode to password and lets
/// a Rig knock in on the person's tap under it, so the approve mode is no choice of theirs and decides nothing here. Err
/// says why the person is asked instead. No other Rig agent may be in either, which [`reserve`] decides as it takes the slot.
pub fn admits_assigned(vouch: &Vouch, here: NoTapHere) -> Result<(), &'static str> {
    match () {
        _ if *vouch != Vouch::Assigned => Err("the knock is not vouched assigned"),
        _ if !here.setting_on => Err("the person's no-tap setting is off"),
        _ if !here.sharing => Err("screen sharing is off"),
        _ => Ok(()),
    }
}

/// Takes the one Rig agent slot for connection `conn` before its login is sent, so two knocks at once cannot both get in
/// as assigned: false while another connection's Rig agent is in or on its way in.
pub fn reserve(conn: i32, by: JoinedBy) -> bool {
    let Some(name) = PROVED.lock().unwrap().get(&conn).cloned() else {
        return false;
    };
    let mut joined = JOINED.lock().unwrap();
    if joined.keys().any(|other| *other != conn) {
        return false;
    }
    joined.insert(conn, Joined { name, by });
    true
}

/// The slot [`reserve`] took for `conn` goes: its login did not get in.
pub fn release(conn: i32) {
    JOINED.lock().unwrap().remove(&conn);
}

/// Whether any Rig agent's session is in on this device now.
pub fn anyone_in() -> bool {
    !JOINED.lock().unwrap().is_empty()
}

/// Whether the person's yes may let this login in: a proved knock always, anything else only where the approve mode
/// takes clicks, and never a login calling itself Rig without the proof.
pub fn may_authorize(proved: bool, claims_rig: bool, click_approves: bool) -> bool {
    proved || (click_approves && !claims_rig)
}

/// How a Rig agent's session on this device got in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinedBy {
    /// The person's Accept on Rig Assistant's notification (`one_tap`).
    OneTap,
    /// The person's Allow on this device's own prompt for the knock.
    Allowed,
    /// An invite this device made (`invite`).
    Invite,
    /// Rig's saved password, signed with the ask the operator accepted in Rig.
    RigAccept,
    /// Rig's saved password, signed with no ask: unattended access, nobody asked.
    Unattended,
    /// An agent the operator assigned this phone to, with no tap, as the person's own setting allows.
    Assigned,
}

/// Who joined and how, as the person at the device reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Joined {
    pub name: String,
    pub by: JoinedBy,
}

impl Joined {
    /// "<agent> joined": short enough that a collapsed notification row never cuts it.
    pub fn title(&self) -> String {
        format!("{} joined", self.name)
    }

    /// How it got in, as the sentence the notice opens with and the app's card shows after the title.
    pub fn said(&self) -> &'static str {
        match self.by {
            JoinedBy::OneTap => "By your Accept on this phone.",
            JoinedBy::Allowed => "By your Allow on this phone.",
            JoinedBy::Invite => "By this phone's invite.",
            JoinedBy::RigAccept => "By your Rig Accept, saved password.",
            JoinedBy::Unattended => "By saved password, nobody asked.",
            JoinedBy::Assigned => "With no tap: assigned in Rig, and your setting allows it.",
        }
    }
}

lazy_static! {
    /// Each proved connection, by the name Rig signed for it.
    static ref PROVED: Mutex<HashMap<i32, String>> = Mutex::new(HashMap::new());
    static ref JOINED: Mutex<HashMap<i32, Joined>> = Mutex::new(HashMap::new());
}

pub fn mark_proved(conn: i32, name: &str) {
    PROVED.lock().unwrap().insert(conn, name.to_owned());
}

pub fn proved(conn: i32) -> bool {
    PROVED.lock().unwrap().contains_key(&conn)
}

/// A proved connection is in, by `by`. A connection that proved nothing is never told as Rig's.
pub fn joined(conn: i32, by: JoinedBy) {
    let Some(name) = PROVED.lock().unwrap().get(&conn).cloned() else {
        return;
    };
    JOINED.lock().unwrap().entry(conn).or_insert(Joined { name, by });
}

/// What the person is told of this connection once it is in; None for any login that is not a proved Rig one.
pub fn joined_how(conn: i32) -> Option<Joined> {
    JOINED.lock().unwrap().get(&conn).cloned()
}

pub fn left(conn: i32) {
    PROVED.lock().unwrap().remove(&conn);
    JOINED.lock().unwrap().remove(&conn);
}

#[cfg(test)]
mod tests {
    use super::*;

    const RIG: &str = "100.64.0.1";
    const NONCE: &[u8] = b"this connection's nonce";

    fn login<'a>(nonce: &'a [u8], dialer: &str) -> Login<'a> {
        Login { device: "moto-g57:21118", salt: "salt", challenge: "challenge", nonce, dialer: dialer.parse().unwrap() }
    }

    fn knock(key: &sign::SecretKey, login: &Login) -> RigKnock {
        vouching(key, login, Vouch::Nothing)
    }

    fn knock_for(key: &sign::SecretKey, login: &Login, ask: &str) -> RigKnock {
        vouching(key, login, Vouch::Accepted(ask.into()))
    }

    fn vouching(key: &sign::SecretKey, login: &Login, vouch: Vouch) -> RigKnock {
        let (ask, assigned) = vouch.fields();
        RigKnock {
            signature: sign::sign_detached(&login.message("agent_a", "Builder", &vouch), key).to_bytes().to_vec().into(),
            agent_id: "agent_a".into(),
            agent_title: "Builder".into(),
            dialer: login.dialer.to_string(),
            ask,
            assigned,
            ..Default::default()
        }
    }

    fn claiming_rig(password: &[u8]) -> LoginRequest {
        LoginRequest { my_id: "rig".into(), my_name: "Mallory (Rig agent)".into(), password: password.to_vec().into(), ..Default::default() }
    }

    #[test]
    fn the_knock_rig_signed_for_this_connection_passes_under_the_signed_name() {
        let (public, secret) = sign::gen_keypair();
        let here = login(NONCE, RIG);
        let proved = check(Some(&public), Some(&knock(&secret, &here)), &here).unwrap();
        assert_eq!(proved, Proved { agent_id: "agent_a".into(), name: "Builder (Rig agent)".into(), vouch: Vouch::Nothing });
        let mapped = login(NONCE, "::ffff:100.64.0.1");
        assert!(check(Some(&public), Some(&knock(&secret, &here)), &mapped).is_ok(), "the same dialer, as a dual-stack socket reports it");
    }

    #[test]
    fn a_forged_knock_is_refused_without_a_key_with_a_wrong_key_and_when_replayed_or_relayed() {
        let (public, secret) = sign::gen_keypair();
        let (_, other) = sign::gen_keypair();
        let here = login(NONCE, RIG);
        let good = knock(&secret, &here);

        assert_eq!(check(Some(&public), None, &here), Err(Refusal::NoProof), "no proof at all");
        assert_eq!(check(Some(&public), Some(&knock(&other, &here)), &here), Err(Refusal::BadProof), "another key");

        let earlier = knock(&secret, &login(b"another connection's nonce", RIG));
        assert_eq!(check(Some(&public), Some(&earlier), &here), Err(Refusal::BadProof), "replayed from another connection");
        assert_eq!(check(Some(&public), Some(&good), &login(b"", RIG)), Err(Refusal::BadProof), "a nonce already spent");

        let elsewhere = login(NONCE, "100.64.0.2");
        let relayed = check(Some(&public), Some(&good), &elsewhere).unwrap_err();
        assert_eq!(relayed, Refusal::OtherDialer, "brought here by another PC");
        assert_ne!(relayed.words(), Refusal::BadProof.words(), "Rig and the log can tell this from a wrong key");
        let readdressed = RigKnock { dialer: "100.64.0.2".into(), ..good.clone() };
        assert_eq!(check(Some(&public), Some(&readdressed), &elsewhere), Err(Refusal::BadProof), "an address Rig did not sign");

        let renamed = RigKnock { agent_title: "Mallory".into(), ..good };
        assert_eq!(check(Some(&public), Some(&renamed), &here), Err(Refusal::BadProof), "a name Rig did not sign");
    }

    #[test]
    fn a_knock_for_an_accepted_ask_proves_that_ask_alone() {
        let (public, secret) = sign::gen_keypair();
        let here = login(NONCE, RIG);
        let good = knock_for(&secret, &here, "desk_1");
        assert_eq!(check(Some(&public), Some(&good), &here).unwrap().vouch, Vouch::Accepted("desk_1".into()));
        let moved = RigKnock { ask: "desk_2".into(), ..good.clone() };
        assert_eq!(check(Some(&public), Some(&moved), &here), Err(Refusal::BadProof), "an ask Rig did not sign");
        let dropped = RigKnock { ask: Default::default(), ..good };
        assert_eq!(check(Some(&public), Some(&dropped), &here), Err(Refusal::BadProof), "the ask taken off, to ask the person instead");
        let plain_knock = knock(&secret, &here);
        let added = RigKnock { ask: "desk_1".into(), ..plain_knock };
        assert_eq!(check(Some(&public), Some(&added), &here), Err(Refusal::BadProof), "an ask put on a knock signed without one");
    }

    #[test]
    fn a_knock_for_an_assigned_agent_proves_the_assignment_alone_and_no_ask() {
        let (public, secret) = sign::gen_keypair();
        let here = login(NONCE, RIG);
        let good = vouching(&secret, &here, Vouch::Assigned);
        assert_eq!(check(Some(&public), Some(&good), &here).unwrap().vouch, Vouch::Assigned);
        let dropped = RigKnock { assigned: false, ..good.clone() };
        assert_eq!(check(Some(&public), Some(&dropped), &here), Err(Refusal::BadProof), "the assignment taken off");
        let added = RigKnock { assigned: true, ..knock(&secret, &here) };
        assert_eq!(check(Some(&public), Some(&added), &here), Err(Refusal::BadProof), "assigned put on a knock signed without it");
        let to_ask = RigKnock { assigned: false, ask: "desk_1".into(), ..good.clone() };
        assert_eq!(check(Some(&public), Some(&to_ask), &here), Err(Refusal::BadProof), "an assignment's proof read as an accepted ask");
        let both = RigKnock { assigned: true, ..knock_for(&secret, &here, "desk_1") };
        assert_eq!(check(Some(&public), Some(&both), &here), Err(Refusal::BadProof), "an ask and an assignment together");
        let elsewhere = vouching(&secret, &login(b"another connection's nonce", RIG), Vouch::Assigned);
        assert_eq!(check(Some(&public), Some(&elsewhere), &here), Err(Refusal::BadProof), "replayed from another connection");
    }

    #[test]
    fn only_an_assigned_knock_gets_in_with_no_tap_and_only_while_the_person_allows_it_and_sharing_runs() {
        let here = NoTapHere { setting_on: true, sharing: true };
        assert_eq!(admits_assigned(&Vouch::Assigned, here), Ok(()));
        let off = NoTapHere { setting_on: false, ..here };
        assert_eq!(admits_assigned(&Vouch::Assigned, off), Err("the person's no-tap setting is off"), "off, as it is by default");
        let stopped = NoTapHere { sharing: false, ..here };
        assert_eq!(admits_assigned(&Vouch::Assigned, stopped), Err("screen sharing is off"), "capture stopped, the listener up");
        assert!(admits_assigned(&Vouch::Nothing, here).is_err(), "a plain knock asks the person");
        assert!(admits_assigned(&Vouch::Accepted("desk_1".into()), here).is_err(), "an accepted ask goes by one_tap alone");
    }

    #[test]
    fn one_rig_agent_slot_is_taken_before_the_login_so_two_assigned_knocks_cannot_both_get_in() {
        let (first, second) = (9101, 9102);
        mark_proved(first, "Builder (Rig agent)");
        mark_proved(second, "Other (Rig agent)");
        assert!(reserve(first, JoinedBy::Assigned));
        assert!(!reserve(second, JoinedBy::Assigned), "the first knock's slot is taken while its login is under way");
        assert!(anyone_in());
        release(first);
        assert!(reserve(second, JoinedBy::Assigned), "a released slot is free again");
        assert!(!reserve(9103, JoinedBy::Assigned), "an unproved connection takes no slot");
        left(second);
        left(first);
    }

    #[test]
    fn a_device_with_no_pairing_refuses_every_knock_as_not_paired() {
        let (_, secret) = sign::gen_keypair();
        let here = login(NONCE, RIG);
        let refused = check(None, Some(&knock(&secret, &here)), &here).unwrap_err();
        assert_eq!(refused, Refusal::NotPaired);
        assert_eq!(refused.words(), "This device is not paired with Rig");
    }

    #[test]
    fn id_rig_with_neither_proof_nor_password_must_prove_and_gets_no_prompt() {
        let login = claiming_rig(b"");
        assert!(must_prove(&login));
        assert!(must_prove(&LoginRequest { my_id: "someone".into(), my_name: "a RIG AGENT".into(), ..Default::default() }));
        assert_eq!(listed_as(login.my_id, login.my_name, false, false), None);
        assert!(!must_prove(&LoginRequest { my_id: "123456789".into(), my_name: "Alice".into(), ..Default::default() }), "an ordinary login");
    }

    #[test]
    fn id_rig_with_the_correct_password_logs_in_as_usual_under_a_neutral_name() {
        let login = claiming_rig(b"the password's proof");
        assert!(!must_prove(&login), "the password decides");
        let listed = listed_as(login.my_id.clone(), login.my_name.clone(), true, false).unwrap();
        assert_eq!(listed, (NEUTRAL_PEER_ID.to_owned(), NEUTRAL_NAME.to_owned()));
        assert!(!claims_rig(&listed.0, &listed.1));
        assert_eq!(listed_as(login.my_id, login.my_name, false, false), None, "a wrong password is not listed either");
    }

    #[test]
    fn only_a_proved_login_keeps_its_rig_name_and_other_logins_are_untouched() {
        let proved = listed_as(PEER_ID.into(), "Builder (Rig agent)".into(), false, true);
        assert_eq!(proved, Some((PEER_ID.to_owned(), "Builder (Rig agent)".to_owned())));
        let ordinary = listed_as("123456789".into(), "Alice".into(), false, false);
        assert_eq!(ordinary, Some(("123456789".to_owned(), "Alice".to_owned())));
    }

    #[test]
    fn a_session_in_is_told_by_who_and_how_and_only_for_a_proved_connection() {
        mark_proved(9001, "Builder (Rig agent)");
        joined(9001, JoinedBy::RigAccept);
        joined(9001, JoinedBy::Allowed);
        let first = joined_how(9001).unwrap();
        assert_eq!((first.title().as_str(), first.said()), ("Builder (Rig agent) joined", "By your Rig Accept, saved password."), "the first way in stands");
        joined(9002, JoinedBy::OneTap);
        assert_eq!(joined_how(9002), None, "an unproved login is never told as Rig's");
        left(9001);
        assert_eq!(joined_how(9001), None);
        assert!(!proved(9001));
        let words = [JoinedBy::OneTap, JoinedBy::Allowed, JoinedBy::Invite, JoinedBy::Unattended, JoinedBy::Assigned]
            .iter()
            .copied()
            .map(|by| Joined { name: "B".into(), by }.said())
            .collect::<Vec<_>>();
        assert_eq!(words, vec!["By your Accept on this phone.", "By your Allow on this phone.", "By this phone's invite.", "By saved password, nobody asked.",
                               "With no tap: assigned in Rig, and your setting allows it."]);
    }

    #[test]
    fn the_persons_yes_lets_in_a_proved_knock_alone_where_passwords_rule() {
        assert!(may_authorize(true, true, false));
        assert!(!may_authorize(false, true, false), "calls itself Rig, no proof");
        assert!(!may_authorize(false, false, false), "password-only");
        assert!(!may_authorize(false, true, true), "clicks do not stand in for the proof");
        assert!(may_authorize(false, false, true), "an ordinary login where clicks approve");
    }

    #[test]
    fn a_connection_counts_as_proved_until_it_closes() {
        const CONN: i32 = 7_000_001;
        assert!(!proved(CONN));
        mark_proved(CONN, "B");
        assert!(proved(CONN) && !proved(CONN + 1));
        left(CONN);
        assert!(!proved(CONN));
    }
}
