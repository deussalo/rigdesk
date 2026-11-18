//! The person at a stock PC answers an agent's desk/request through RustDesk's own accept prompt: Rig's `POST /offer` on
//! the bridge's agent API logs in with no password under the agent's name, which is the login RustDesk's "accept
//! sessions via click" answers with its prompt: the connection manager's Accept and Dismiss on a desktop, the "Do you
//! accept?" notification and dialog on Android. Their answer is an `offer` event with Rig's ask and nonce. On Allow the
//! connection they accepted carries the session (`Link::adopt`), so no password is used, and none need be kept; when it
//! ends, the agent holding the session is told. `POST /offer/end` takes the prompt away, since the PC drops it with the
//! connection.
//!
//! RigDesk Android shows its Allow only to a knock proved with Rig's pairing key. Its greeting asks for that with a nonce
//! of this connection, and the knock then carries Rig's signature (see `base::rig_knock`). A stock PC asks for nothing
//! and gets nothing.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base::{
    message_proto::{login_response, message, misc, Hash, Message, OptionMessage, RigKnock},
    rig_knock,
};
use hbb_common::{
    log,
    protobuf::{Message as _, MessageField},
    sodiumoxide::crypto::sign,
    tcp::FramedStream,
    tokio::{self, runtime::Handle, task::AbortHandle},
};
use rigdesk_agent::operator::{Closed, Offer, Switch};
use rigdesk_agent::{EventKind, Shared};

use crate::link::Link;

/// What the PC answers a login that comes with no password, while its person is asked.
const NO_PASSWORD_ACCESS: &str = "No Password Access";
/// What the PC says as it closes the connection when its person presses Dismiss.
const CLOSED_BY_PERSON: &str = "Closed manually by the peer";
/// The PC drops an unanswered prompt itself after about 3 minutes; this only bounds a PC that went silent.
const READ_TIMEOUT_MS: u64 = 30_000;
/// An allowed connection no agent has claimed by then is hung up: Rig settled the ask another way, or never claimed it.
const UNCLAIMED: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, PartialEq)]
pub enum Answer {
    Allowed,
    Dismissed,
    Refused(String),
    Lapsed(String),
}

impl Answer {
    /// The `offer` event's answer: `allow`, `deny`, `lapsed` for a prompt that was shown, else why none was.
    fn said(&self, shown: bool) -> String {
        match self {
            Answer::Allowed => "allow".into(),
            Answer::Dismissed => "deny".into(),
            Answer::Lapsed(_) if shown => "lapsed".into(),
            Answer::Lapsed(why) => format!("lapsed: {why}"),
            Answer::Refused(words) => format!("refused: {words}"),
        }
    }
}

/// The connection the person allowed, and the PC's login answer on it.
pub struct Allowed {
    pub stream: FramedStream,
    pub peer_info: Message,
}

/// What a knock proves itself with, for the agent it is made for.
struct Proof {
    key_file: PathBuf,
    agent_id: String,
    agent_title: String,
}

impl Proof {
    fn knock(&self, key: &sign::SecretKey, device: &str, hash: &Hash, dialer: IpAddr) -> RigKnock {
        let login = rig_knock::Login { device, salt: &hash.salt, challenge: &hash.challenge, nonce: &hash.rig_knock_nonce, dialer };
        RigKnock {
            signature: sign::sign_detached(&login.message(&self.agent_id, &self.agent_title), key).to_bytes().to_vec().into(),
            agent_id: self.agent_id.clone(),
            agent_title: self.agent_title.clone(),
            ..Default::default()
        }
    }

    /// The login with the proof in it. Without a usable key it goes unproved, and the device refuses it in its own words.
    async fn added_to(&self, mut login: Message, device: &str, hash: &Hash, dialer: IpAddr) -> Message {
        let file = self.key_file.clone();
        match tokio::task::spawn_blocking(move || crate::invite::read_key(&file)).await {
            Ok(Ok((_, key))) => login.mut_login_request().rig_knock = MessageField::some(self.knock(&key, device, hash, dialer)),
            Ok(Err(e)) => log::warn!("the knock goes without Rig's proof: {e}"),
            Err(e) => log::warn!("the knock goes without Rig's proof: {e}"),
        }
        login
    }
}

/// The bridge's side of `/offer`: one prompt at a time, the latest ask's.
pub struct Knocker {
    link: Arc<Link>,
    shared: Arc<Shared>,
    runtime: Handle,
    pair_key: Option<PathBuf>,
    shown: Mutex<Option<(String, AbortHandle)>>,
    /// The ask whose allowed connection the link took last.
    allowed: Arc<Mutex<Option<String>>>,
}

impl Knocker {
    pub fn new(link: Arc<Link>, shared: Arc<Shared>, runtime: Handle, pair_key: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Knocker { link, shared, runtime, pair_key, shown: Mutex::new(None), allowed: Arc::default() })
    }
}

impl Switch for Knocker {
    /// Always open: this also makes the bridge's `/join` reachable, behind Rig's bearer, though the plugin never uses it for a stock PC.
    fn open(&self) -> Result<(), Closed> {
        Ok(())
    }

    fn offer(&self, offer: Offer) -> Result<(), String> {
        let (link, shared, taken) = (self.link.clone(), self.shared.clone(), self.allowed.clone());
        let name = format!("{} (Rig agent)", offer.owner.title);
        let ask = offer.ask.clone();
        let proof = self.pair_key.clone().map(|key_file| Proof { key_file, agent_id: offer.owner.agent_id.clone(), agent_title: offer.owner.title.clone() });
        let task = self.runtime.spawn(async move {
            let (answer, shown, allowed) = knock(&link, &name, proof.as_ref()).await;
            let adopted = allowed.is_some();
            if let Some(allowed) = allowed {
                *taken.lock().unwrap() = Some(offer.ask.clone());
                link.adopt(allowed);
            }
            shared.events.push(EventKind::Offer { ask: offer.ask.clone(), nonce: offer.nonce, answer: answer.said(shown) });
            if adopted {
                tokio::spawn(async move {
                    tokio::time::sleep(UNCLAIMED).await;
                    if !hang_up_unclaimed(&link, &shared, &taken, &offer.ask) {
                        let mut taken = taken.lock().unwrap();
                        if taken.as_deref() == Some(offer.ask.as_str()) {
                            *taken = None;
                        }
                    }
                });
            }
        });
        if let Some((_, earlier)) = self.shown.lock().unwrap().replace((ask, task.abort_handle())) {
            earlier.abort();
        }
        Ok(())
    }

    fn offer_end(&self, ask: &str) -> bool {
        let mut shown = self.shown.lock().unwrap();
        let ended = match shown.take() {
            Some((held, task)) if held == ask => {
                let showing = !task.is_finished();
                task.abort();
                showing
            }
            other => {
                *shown = other;
                false
            }
        };
        hang_up_unclaimed(&self.link, &self.shared, &self.allowed, ask) || ended
    }
}

/// Hangs up the connection allowed for `ask` while no agent holds the session: Rig settled that ask another way, or never claimed it.
fn hang_up_unclaimed(link: &Link, shared: &Shared, taken: &Mutex<Option<String>>, ask: &str) -> bool {
    let mut taken = taken.lock().unwrap();
    if taken.as_deref() != Some(ask) || shared.standing().owner.is_some() {
        return false;
    }
    *taken = None;
    link.hang_up();
    true
}

/// One knock: the prompt is shown, and its person's answer read; whether the PC showed it; on Allow, the connection.
async fn knock(link: &Link, name: &str, proof: Option<&Proof>) -> (Answer, bool, Option<Allowed>) {
    let mut stream = match link.dial().await {
        Ok(stream) => stream,
        Err(e) => return (Answer::Lapsed(format!("cannot reach the PC at {}: {e}", link.label())), false, None),
    };
    let (answer, shown, peer_info) = answered(link, name, proof, &mut stream).await;
    let allowed = peer_info.filter(|_| answer == Answer::Allowed).map(|peer_info| Allowed { stream, peer_info });
    (answer, shown, allowed)
}

async fn answered(link: &Link, name: &str, proof: Option<&Proof>, stream: &mut FramedStream) -> (Answer, bool, Option<Message>) {
    let mut waiting = false;
    loop {
        let bytes = match stream.next_timeout(READ_TIMEOUT_MS).await {
            Some(Ok(bytes)) => bytes,
            Some(Err(e)) => return (Answer::Lapsed(format!("connection error: {e}")), waiting, None),
            None => return (Answer::Lapsed("the PC closed the connection, or its prompt went unanswered".into()), waiting, None),
        };
        let Ok(msg) = Message::parse_from_bytes(&bytes) else { continue };
        let login = matches!(msg.union, Some(message::Union::LoginResponse(_))).then(|| msg.clone());
        let asks_proof = match &msg.union {
            Some(message::Union::Hash(hash)) if !hash.rig_knock_nonce.is_empty() => Some(hash.clone()),
            _ => None,
        };
        let reply = match on_message(link, name, msg) {
            Step::Send(reply) => reply,
            Step::Waiting => {
                waiting = true;
                continue;
            }
            Step::Done(answer) => return (answer, waiting, login),
            Step::Nothing => continue,
        };
        let reply = match (asks_proof, proof) {
            (Some(hash), Some(proof)) => proof.added_to(reply, link.username(), &hash, stream.local_addr().ip()).await,
            _ => reply,
        };
        if let Err(e) = stream.send(&reply).await {
            return (Answer::Lapsed(format!("connection error: {e}")), waiting, None);
        }
        waiting |= matches!(reply.union, Some(message::Union::LoginRequest(_)));
    }
}

enum Step {
    Send(Message),
    Waiting,
    Done(Answer),
    Nothing,
}

fn on_message(link: &Link, name: &str, msg: Message) -> Step {
    match msg.union {
        Some(message::Union::Hash(hash)) => {
            let mut login = link.login(&hash, OptionMessage::default(), None);
            login.mut_login_request().password = Default::default();
            login.mut_login_request().my_name = name.to_string();
            Step::Send(login)
        }
        Some(message::Union::LoginResponse(res)) => match res.union {
            Some(login_response::Union::Error(e)) if e == NO_PASSWORD_ACCESS => Step::Waiting,
            Some(login_response::Union::Error(e)) => Step::Done(Answer::Refused(e)),
            Some(login_response::Union::PeerInfo(_)) => Step::Done(Answer::Allowed),
            _ => Step::Nothing,
        },
        Some(message::Union::Misc(m)) => match m.union {
            Some(misc::Union::CloseReason(reason)) if reason == CLOSED_BY_PERSON => Step::Done(Answer::Dismissed),
            Some(misc::Union::CloseReason(reason)) => Step::Done(Answer::Lapsed(format!("the PC closed it: {reason}"))),
            _ => Step::Nothing,
        },
        Some(message::Union::TestDelay(t)) if !t.from_client => {
            let mut reply = Message::new();
            reply.set_test_delay(t);
            Step::Send(reply)
        }
        _ => Step::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::message_proto::{LoginResponse, Misc, PeerInfo};
    use hbb_common::base64::{engine::general_purpose::STANDARD, Engine};

    fn link() -> Arc<Link> {
        let shared = Arc::new(rigdesk_agent::Shared::new(rigdesk_agent::Events::new(8)));
        Link::new(crate::link::Route::direct("100.64.0.9"), "saved".into(), "Rig agent".into(), shared)
    }

    fn login_response(union: login_response::Union) -> Message {
        let mut msg = Message::new();
        msg.set_login_response(LoginResponse { union: Some(union), ..Default::default() });
        msg
    }

    #[test]
    fn the_login_carries_the_agents_name_and_no_password_even_with_one_saved() {
        let link = link();
        let mut hash = Message::new();
        hash.set_hash(Hash { salt: "s".into(), challenge: "c".into(), ..Default::default() });
        let Step::Send(sent) = on_message(&link, "a1 (Rig agent)", hash) else { panic!("a login goes back") };
        let login = sent.login_request();
        assert!(login.password.is_empty());
        assert_eq!(login.my_name, "a1 (Rig agent)");
        assert_eq!(login.my_id, "rig");
    }

    #[test]
    fn an_allowed_connection_is_hung_up_for_its_own_ask_only_while_no_agent_holds_the_session() {
        let link = link();
        let shared = rigdesk_agent::Shared::new(rigdesk_agent::Events::new(8));
        let taken = Mutex::new(Some("desk_1".to_string()));
        assert!(!hang_up_unclaimed(&link, &shared, &taken, "desk_2"), "another ask's end");
        let minted = shared.mint();
        let owner = rigdesk_agent::Owner { agent_id: "a1".into(), title: "Builder".into(), graph: "g".into() };
        assert!(shared.claim(&minted.grant, &minted.secret, owner).is_ok());
        assert!(!hang_up_unclaimed(&link, &shared, &taken, "desk_1"), "a claimed session stays");
        shared.revoke("left");
        assert!(hang_up_unclaimed(&link, &shared, &taken, "desk_1"));
        assert!(taken.lock().unwrap().is_none());
    }

    /// What a device on loopback gets from a knock, after greeting it with `hash`, and the address the knock came from.
    async fn login_after_greeting(hash: Hash, proof: Option<&Proof>) -> (base::message_proto::LoginRequest, IpAddr) {
        let listener = hbb_common::tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(rigdesk_agent::Shared::new(rigdesk_agent::Events::new(8)));
        let link = Link::new(crate::link::Route::direct(&addr.to_string()), String::new(), "Rig agent".into(), shared);
        let device = async {
            let (socket, dialer) = listener.accept().await.unwrap();
            let mut device = FramedStream::from(socket, dialer);
            let mut greeting = Message::new();
            greeting.set_hash(hash);
            device.send(&greeting).await.unwrap();
            loop {
                let bytes = device.next_timeout(5_000).await.expect("the bridge hung up").unwrap();
                if let Some(login) = Message::parse_from_bytes(&bytes).ok().filter(Message::has_login_request) {
                    return (login.login_request().clone(), dialer.ip());
                }
            }
        };
        tokio::select! {
            login = device => login,
            _ = knock(&link, "a1 (Rig agent)", proof) => panic!("the knock ended before the device had its login"),
        }
    }

    #[tokio::test]
    async fn a_knock_proves_itself_only_to_a_device_that_asks_and_only_for_that_connection() {
        let dir = std::env::temp_dir().join(format!("rigdesk-knock-proof-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let key_file = dir.join("pair.key");
        let public = STANDARD.decode(crate::invite::pair_key(&key_file).unwrap()).unwrap();
        let public = sign::PublicKey::from_slice(&public).unwrap();
        let proof = Proof { key_file: key_file.clone(), agent_id: "agent_a".into(), agent_title: "Builder".into() };
        let greeting = Hash { salt: "s".into(), challenge: "c".into(), rig_knock_nonce: b"nonce".to_vec().into(), ..Default::default() };

        let (login, dialer) = login_after_greeting(greeting.clone(), Some(&proof)).await;
        assert!(login.password.is_empty());
        let knocked = login.rig_knock.clone().into_option().expect("the device asked for the proof");
        assert_eq!((knocked.agent_id.as_str(), knocked.agent_title.as_str()), ("agent_a", "Builder"));
        let signature = sign::Signature::from_bytes(&knocked.signature).unwrap();
        let mut signed = rig_knock::Login { device: &login.username, salt: "s", challenge: "c", nonce: b"nonce", dialer };
        assert!(sign::verify_detached(&signature, &signed.message("agent_a", "Builder"), &public));
        signed.nonce = b"another connection";
        assert!(!sign::verify_detached(&signature, &signed.message("agent_a", "Builder"), &public));

        let stock = Hash { rig_knock_nonce: Default::default(), ..greeting.clone() };
        assert!(login_after_greeting(stock, Some(&proof)).await.0.rig_knock.is_none(), "a stock PC asks for no proof");
        assert!(login_after_greeting(greeting.clone(), None).await.0.rig_knock.is_none(), "no key configured");
        let keyless = Proof { key_file: dir.join("none"), agent_id: "agent_a".into(), agent_title: "Builder".into() };
        assert!(login_after_greeting(greeting, Some(&keyless)).await.0.rig_knock.is_none(), "the key file is not there yet");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_persons_answer_is_read() {
        let link = link();
        let name = "a1 (Rig agent)";
        assert!(matches!(on_message(&link, name, login_response(login_response::Union::Error(NO_PASSWORD_ACCESS.into()))), Step::Waiting));
        assert!(matches!(on_message(&link, name, login_response(login_response::Union::PeerInfo(PeerInfo::new()))), Step::Done(Answer::Allowed)));
        let mut dismissed = Message::new();
        dismissed.set_misc(Misc { union: Some(misc::Union::CloseReason(CLOSED_BY_PERSON.into())), ..Default::default() });
        assert!(matches!(on_message(&link, name, dismissed), Step::Done(Answer::Dismissed)));
        let Step::Done(refused) = on_message(&link, name, login_response(login_response::Union::Error("Connection not allowed".into()))) else { panic!() };
        assert_eq!(refused.said(true), "refused: Connection not allowed");
        assert_eq!(Answer::Lapsed("gone".into()).said(true), "lapsed");
        assert_eq!(Answer::Lapsed("gone".into()).said(false), "lapsed: gone");
    }
}
