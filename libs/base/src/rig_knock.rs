//! RigDesk Android: a Rig agent's knock, the passwordless login that asks the person at the phone to let the agent in.
//!
//! RigDesk takes passwords only, so the phone's Allow exists for one login alone: a knock Rig signed with the key this
//! device was paired with (`invite::pair`). The signature covers this connection's greeting, whose nonce is new for every
//! connection, so a proof seen once opens nothing later. It also covers the address the knock comes from: Rig signs any
//! greeting a PC it dials shows it, and without the address another PC could show Rig this device's greeting and bring
//! the proof here itself. The knock says which address Rig signed, so a good signature from an unexpected address can be
//! told apart from a bad one.
//!
//! A knock may also name the ask the person already accepted on Rig Assistant's notification (`one_tap`): Rig then signs
//! the ask too, under its own domain, so a proof for one ask opens no other.
//!
//! Anything else that calls itself Rig gets no prompt, no Allow and keeps no name of its own on this device.

use crate::{invite, message_proto::{LoginRequest, RigKnock}};
use hbb_common::{
    rand::{thread_rng, RngCore},
    sodiumoxide::crypto::sign,
};
use lazy_static::lazy_static;
use std::{collections::HashSet, net::IpAddr, sync::Mutex};

const DOMAIN: &str = "rigdesk-knock-v1";
const DOMAIN_ASK: &str = "rigdesk-knock-ask-v1";
const NONCE_BYTES: usize = 32;
const CLAIMED_NAME: &str = "rig agent";
/// The login id of Rig's knock, which the phone's notification goes by.
pub const PEER_ID: &str = "rig";
/// What the device shows for a login that got in by password while calling itself Rig.
pub const NEUTRAL_PEER_ID: &str = "password login";
pub const NEUTRAL_NAME: &str = "Remote session";

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

impl Login<'_> {
    /// What Rig signs for this connection and this agent, and for the ask the person accepted when `ask` is not empty.
    pub fn message(&self, agent_id: &str, agent_title: &str, ask: &str) -> Vec<u8> {
        let dialer = plain(self.dialer).to_string();
        let domain = if ask.is_empty() { DOMAIN } else { DOMAIN_ASK };
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
        if !ask.is_empty() {
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
    /// The ask Rig signed, for a knock whose person already accepted it on Rig Assistant's notification.
    pub ask: Option<String>,
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
    if !sign::verify_detached(&signature, &signed.message(&knock.agent_id, &knock.agent_title, &knock.ask), key) {
        return Err(Refusal::BadProof);
    }
    if plain(dialer) != plain(login.dialer) {
        return Err(Refusal::OtherDialer);
    }
    let ask = (!knock.ask.is_empty()).then(|| knock.ask.clone());
    Ok(Proved { agent_id: knock.agent_id.clone(), name: format!("{} (Rig agent)", knock.agent_title), ask })
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

/// Whether the person's yes may let this login in: a proved knock always, anything else only where the approve mode
/// takes clicks, and never a login calling itself Rig without the proof.
pub fn may_authorize(proved: bool, claims_rig: bool, click_approves: bool) -> bool {
    proved || (click_approves && !claims_rig)
}

lazy_static! {
    static ref PROVED: Mutex<HashSet<i32>> = Mutex::new(HashSet::new());
}

pub fn mark_proved(conn: i32) {
    PROVED.lock().unwrap().insert(conn);
}

pub fn proved(conn: i32) -> bool {
    PROVED.lock().unwrap().contains(&conn)
}

pub fn left(conn: i32) {
    PROVED.lock().unwrap().remove(&conn);
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
        knock_for(key, login, "")
    }

    fn knock_for(key: &sign::SecretKey, login: &Login, ask: &str) -> RigKnock {
        RigKnock {
            signature: sign::sign_detached(&login.message("agent_a", "Builder", ask), key).to_bytes().to_vec().into(),
            agent_id: "agent_a".into(),
            agent_title: "Builder".into(),
            dialer: login.dialer.to_string(),
            ask: ask.into(),
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
        assert_eq!(proved, Proved { agent_id: "agent_a".into(), name: "Builder (Rig agent)".into(), ask: None });
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
        assert_eq!(check(Some(&public), Some(&good), &here).unwrap().ask.as_deref(), Some("desk_1"));
        let moved = RigKnock { ask: "desk_2".into(), ..good.clone() };
        assert_eq!(check(Some(&public), Some(&moved), &here), Err(Refusal::BadProof), "an ask Rig did not sign");
        let dropped = RigKnock { ask: Default::default(), ..good };
        assert_eq!(check(Some(&public), Some(&dropped), &here), Err(Refusal::BadProof), "the ask taken off, to ask the person instead");
        let plain_knock = knock(&secret, &here);
        let added = RigKnock { ask: "desk_1".into(), ..plain_knock };
        assert_eq!(check(Some(&public), Some(&added), &here), Err(Refusal::BadProof), "an ask put on a knock signed without one");
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
        mark_proved(CONN);
        assert!(proved(CONN) && !proved(CONN + 1));
        left(CONN);
        assert!(!proved(CONN));
    }
}
