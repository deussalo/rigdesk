//! RigDesk: an invite the person at this device makes for a Rig agent, redeemed inside RustDesk's login.
//!
//! The link holds a grant and a secret, and the device keeps only the secret's hash. The login presents Rig's signature
//! over the grant, that hash, this connection's salt and challenge and the agent Rig names, so neither the secret nor
//! Rig's key crosses the wire, and someone holding the link without the key has nothing to present.

use crate::{config::keys::OPTION_RIG_INVITE_KEY, message_proto::RigInvite};
use hbb_common::{
    base64::{engine::general_purpose::STANDARD, Engine},
    config::{Config, LocalConfig},
    get_time,
    rand::{thread_rng, RngCore},
    sha2::{Digest, Sha256},
    sodiumoxide::crypto::sign,
};
use lazy_static::lazy_static;
use serde_derive::Serialize;
use std::sync::Mutex;

const LIFETIME_MS: i64 = 15 * 60 * 1000;
const PROOF_DOMAIN: &str = "rigdesk-invite-v1";

/// What Rig signs for one login.
pub struct Claim<'a> {
    pub device_id: &'a str,
    pub grant: &'a str,
    pub secret_hash: [u8; 32],
    pub salt: &'a str,
    pub challenge: &'a str,
    pub agent_id: &'a str,
    pub agent_title: &'a str,
}

impl Claim<'_> {
    pub fn message(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let fields: [&[u8]; 8] = [
            PROOF_DOMAIN.as_bytes(),
            self.device_id.as_bytes(),
            self.grant.as_bytes(),
            &self.secret_hash,
            self.salt.as_bytes(),
            self.challenge.as_bytes(),
            self.agent_id.as_bytes(),
            self.agent_title.as_bytes(),
        ];
        for field in fields {
            out.extend_from_slice(&(field.len() as u64).to_le_bytes());
            out.extend_from_slice(field);
        }
        out
    }
}

pub fn secret_hash(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    NotPaired,
    NoInvite,
    BadProof,
    ForOtherAgent,
}

impl Refusal {
    pub fn words(self) -> &'static str {
        match self {
            Refusal::NotPaired => "This device is not paired with Rig",
            Refusal::NoInvite => "This invite has lapsed, was revoked or was already used",
            Refusal::BadProof => "This invite can only be redeemed by Rig",
            Refusal::ForOtherAgent => "This invite is for another Rig agent",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Minted {
    pub link: String,
    pub until_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Holder {
    pub agent: String,
    pub title: String,
    #[serde(skip)]
    conn: i32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OpenInvite {
    pub until_ms: i64,
    pub agent: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Refused {
    pub at_ms: i64,
    pub reason: &'static str,
}

/// What the app shows: the open invite with its Revoke, the agent using the device with its End, the last refusal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct State {
    pub open: Option<OpenInvite>,
    pub holder: Option<Holder>,
    pub refused: Option<Refused>,
}

struct Open {
    grant: String,
    secret_hash: [u8; 32],
    until_ms: i64,
    agent: Option<String>,
}

/// One login's redemption attempt.
pub struct Login<'a> {
    pub invite: &'a RigInvite,
    pub device_id: &'a str,
    pub salt: &'a str,
    pub challenge: &'a str,
    pub conn: i32,
}

#[derive(Default)]
struct Invites {
    open: Option<Open>,
    holder: Option<Holder>,
    refused: Option<Refused>,
}

impl Invites {
    fn lapse(&mut self, now_ms: i64) {
        if self.open.as_ref().map_or(false, |open| open.until_ms <= now_ms) {
            self.open = None;
        }
    }

    /// A new link replaces an open one. None is made while an agent uses the device, so one invite means one agent.
    fn mint(&mut self, device_id: &str, agent: Option<&str>, now_ms: i64) -> Result<Minted, &'static str> {
        if self.holder.is_some() {
            return Err("A Rig agent is using this device");
        }
        let (grant, secret) = (fresh(), fresh());
        let until_ms = now_ms + LIFETIME_MS;
        let link = format!("rigdesk://id.{device_id}/{grant}#{secret}");
        self.open = Some(Open { grant, secret_hash: secret_hash(&secret), until_ms, agent: agent.map(str::to_owned) });
        self.refused = None;
        Ok(Minted { link, until_ms })
    }

    /// The signature is checked before the invite is spent, so a failed attempt leaves it open for Rig.
    fn redeem(&mut self, key: Option<&sign::PublicKey>, login: &Login, now_ms: i64) -> Result<Holder, Refusal> {
        self.lapse(now_ms);
        let result = self.check(key, login);
        match &result {
            Ok(holder) => {
                self.open = None;
                self.holder = Some(holder.clone());
            }
            Err(refusal) => self.refused = Some(Refused { at_ms: now_ms, reason: refusal.words() }),
        }
        result
    }

    fn check(&self, key: Option<&sign::PublicKey>, login: &Login) -> Result<Holder, Refusal> {
        let key = key.ok_or(Refusal::NotPaired)?;
        let invite = login.invite;
        let open = self.open.as_ref().filter(|open| open.grant == invite.grant).ok_or(Refusal::NoInvite)?;
        let claim = Claim {
            device_id: login.device_id,
            grant: &open.grant,
            secret_hash: open.secret_hash,
            salt: login.salt,
            challenge: login.challenge,
            agent_id: &invite.agent_id,
            agent_title: &invite.agent_title,
        };
        let signature = sign::Signature::from_bytes(&invite.signature).map_err(|_| Refusal::BadProof)?;
        if !sign::verify_detached(&signature, &claim.message(), key) {
            return Err(Refusal::BadProof);
        }
        if open.agent.as_ref().map_or(false, |agent| *agent != invite.agent_id) {
            return Err(Refusal::ForOtherAgent);
        }
        Ok(Holder { agent: invite.agent_id.clone(), title: invite.agent_title.clone(), conn: login.conn })
    }

    fn state(&mut self, now_ms: i64) -> State {
        self.lapse(now_ms);
        State {
            open: self.open.as_ref().map(|open| OpenInvite { until_ms: open.until_ms, agent: open.agent.clone() }),
            holder: self.holder.clone(),
            refused: self.refused.clone(),
        }
    }

    fn left(&mut self, conn: i32) {
        if self.holder.as_ref().map_or(false, |holder| holder.conn == conn) {
            self.holder = None;
        }
    }
}

fn fresh() -> String {
    let mut bytes = [0u8; 16];
    thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_key(text: &str) -> Option<sign::PublicKey> {
    sign::PublicKey::from_slice(&STANDARD.decode(text.trim()).ok()?)
}

fn paired_key() -> Option<sign::PublicKey> {
    parse_key(&LocalConfig::get_option(OPTION_RIG_INVITE_KEY))
}

lazy_static! {
    static ref INVITES: Mutex<Invites> = Mutex::new(Invites::default());
}

/// Keeps Rig's public key; false if the text is not one.
pub fn pair(key: &str) -> bool {
    if parse_key(key).is_none() {
        return false;
    }
    LocalConfig::set_option(OPTION_RIG_INVITE_KEY.to_owned(), key.trim().to_owned());
    true
}

/// With `agent`, the Rig agent id the person chose, only that agent can redeem the invite; without it, the first agent
/// Rig names does.
pub fn mint(agent: Option<&str>) -> Result<Minted, &'static str> {
    if paired_key().is_none() {
        return Err(Refusal::NotPaired.words());
    }
    let agent = agent.map(str::trim);
    if agent == Some("") {
        return Err("Name the Rig agent this invite is for");
    }
    INVITES.lock().unwrap().mint(&Config::get_id(), agent, get_time())
}

pub fn revoke() -> bool {
    INVITES.lock().unwrap().open.take().is_some()
}

pub fn state() -> State {
    INVITES.lock().unwrap().state(get_time())
}

/// The connection of the agent using this device, for the app's End.
pub fn holder_conn() -> Option<i32> {
    INVITES.lock().unwrap().holder.as_ref().map(|holder| holder.conn)
}

pub fn redeem(login: &Login) -> Result<Holder, Refusal> {
    INVITES.lock().unwrap().redeem(paired_key().as_ref(), login, get_time())
}

pub fn left(conn: i32) {
    INVITES.lock().unwrap().left(conn);
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: &str = "123456789";
    const NOW: i64 = 1_000_000;

    fn parts(link: &str) -> (String, String) {
        let rest = link.strip_prefix(&format!("rigdesk://id.{DEVICE}/")).unwrap();
        let (grant, secret) = rest.split_once('#').unwrap();
        (grant.to_owned(), secret.to_owned())
    }

    fn invite(link: &str, key: &sign::SecretKey, salt: &str, challenge: &str, agent: &str) -> RigInvite {
        let (grant, secret) = parts(link);
        let claim = Claim {
            device_id: DEVICE,
            grant: &grant,
            secret_hash: secret_hash(&secret),
            salt,
            challenge,
            agent_id: agent,
            agent_title: "Builder",
        };
        RigInvite {
            grant: grant.clone(),
            signature: sign::sign_detached(&claim.message(), key).to_bytes().to_vec().into(),
            agent_id: agent.to_owned(),
            agent_title: "Builder".to_owned(),
            ..Default::default()
        }
    }

    fn login(invite: &RigInvite) -> Login<'_> {
        Login { invite, device_id: DEVICE, salt: "salt", challenge: "challenge", conn: 7 }
    }

    #[test]
    fn rigs_signature_redeems_once_and_names_the_agent() {
        let (public, secret) = sign::gen_keypair();
        let mut invites = Invites::default();
        let minted = invites.mint(DEVICE, None, NOW).unwrap();
        assert_eq!(minted.until_ms, NOW + LIFETIME_MS);
        let invite = invite(&minted.link, &secret, "salt", "challenge", "agent_a");

        let holder = invites.redeem(Some(&public), &login(&invite), NOW + 1).unwrap();
        assert_eq!((holder.agent.as_str(), holder.title.as_str()), ("agent_a", "Builder"));
        assert_eq!(invites.redeem(Some(&public), &login(&invite), NOW + 2), Err(Refusal::NoInvite));

        let state = invites.state(NOW + 3);
        assert_eq!(state.open, None);
        assert_eq!(state.holder, Some(holder));
        assert_eq!(state.refused.map(|refused| refused.reason), Some(Refusal::NoInvite.words()));
    }

    #[test]
    fn without_rigs_key_the_link_is_refused_and_stays_open_for_rig() {
        let (public, secret) = sign::gen_keypair();
        let (_, other) = sign::gen_keypair();
        let mut invites = Invites::default();
        let link = invites.mint(DEVICE, None, NOW).unwrap().link;

        let forged = invite(&link, &other, "salt", "challenge", "agent_a");
        assert_eq!(invites.redeem(Some(&public), &login(&forged), NOW), Err(Refusal::BadProof));
        assert_eq!(invites.redeem(None, &login(&forged), NOW), Err(Refusal::NotPaired));
        let stale = invite(&link, &secret, "salt", "an earlier challenge", "agent_a");
        assert_eq!(invites.redeem(Some(&public), &login(&stale), NOW), Err(Refusal::BadProof));
        let mut renamed = invite(&link, &secret, "salt", "challenge", "agent_a");
        renamed.agent_id = "agent_b".to_owned();
        assert_eq!(invites.redeem(Some(&public), &login(&renamed), NOW), Err(Refusal::BadProof));

        let good = invite(&link, &secret, "salt", "challenge", "agent_a");
        assert!(invites.redeem(Some(&public), &login(&good), NOW).is_ok());
    }

    #[test]
    fn an_invite_lapses_after_fifteen_minutes_and_is_replaced_by_a_new_one() {
        let (public, secret) = sign::gen_keypair();
        let mut invites = Invites::default();
        let first = invites.mint(DEVICE, None, NOW).unwrap().link;
        let second = invites.mint(DEVICE, None, NOW).unwrap().link;
        let replaced = invite(&first, &secret, "salt", "challenge", "agent_a");
        assert_eq!(invites.redeem(Some(&public), &login(&replaced), NOW), Err(Refusal::NoInvite));

        let late = invite(&second, &secret, "salt", "challenge", "agent_a");
        assert_eq!(invites.state(NOW + LIFETIME_MS - 1).open, Some(OpenInvite { until_ms: NOW + LIFETIME_MS, agent: None }));
        assert_eq!(invites.redeem(Some(&public), &login(&late), NOW + LIFETIME_MS), Err(Refusal::NoInvite));
        assert_eq!(invites.state(NOW + LIFETIME_MS).open, None);
    }

    #[test]
    fn a_new_invite_waits_until_the_agent_has_left() {
        let (public, secret) = sign::gen_keypair();
        let mut invites = Invites::default();
        let link = invites.mint(DEVICE, None, NOW).unwrap().link;
        let invite = invite(&link, &secret, "salt", "challenge", "agent_a");
        invites.redeem(Some(&public), &login(&invite), NOW).unwrap();

        assert!(invites.mint(DEVICE, None, NOW).is_err());
        invites.left(8);
        assert!(invites.mint(DEVICE, None, NOW).is_err());
        invites.left(7);
        assert!(invites.mint(DEVICE, None, NOW).is_ok());
    }

    #[test]
    fn an_invite_made_for_one_agent_refuses_another_and_stays_open_for_it() {
        let (public, secret) = sign::gen_keypair();
        let mut invites = Invites::default();
        let link = invites.mint(DEVICE, Some("agent_a"), NOW).unwrap().link;
        assert_eq!(invites.state(NOW).open.and_then(|open| open.agent), Some("agent_a".to_owned()));

        let other = invite(&link, &secret, "salt", "challenge", "agent_b");
        assert_eq!(invites.redeem(Some(&public), &login(&other), NOW), Err(Refusal::ForOtherAgent));
        assert_eq!(invites.state(NOW).holder, None);

        let chosen = invite(&link, &secret, "salt", "challenge", "agent_a");
        assert_eq!(invites.redeem(Some(&public), &login(&chosen), NOW).unwrap().agent, "agent_a");
    }
}
