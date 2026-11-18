//! The person at a stock PC answers an agent's desk/request through RustDesk's own accept prompt: Rig's `POST /offer` on
//! the bridge's agent API logs in with no password under the agent's name, which is the login RustDesk's "accept
//! sessions via click" answers with its prompt: the connection manager's Accept and Dismiss on a desktop, the "Do you
//! accept?" notification and dialog on Android. Their answer is an `offer` event with Rig's ask and nonce. On Allow the
//! connection they accepted carries the session (`Link::adopt`), so no password is used, and none need be kept; when it
//! ends, the agent holding the session is told. `POST /offer/end` takes the prompt away, since the PC drops it with the
//! connection.

use std::sync::{Arc, Mutex};

use base::message_proto::{login_response, message, misc, Message, OptionMessage};
use hbb_common::protobuf::Message as _;
use hbb_common::tcp::FramedStream;
use hbb_common::tokio::{runtime::Handle, task::AbortHandle};
use rigdesk_agent::operator::{Closed, Offer, Switch};
use rigdesk_agent::{EventKind, Shared};

use crate::link::Link;

/// What the PC answers a login that comes with no password, while its person is asked.
const NO_PASSWORD_ACCESS: &str = "No Password Access";
/// What the PC says as it closes the connection when its person presses Dismiss.
const CLOSED_BY_PERSON: &str = "Closed manually by the peer";
/// The PC drops an unanswered prompt itself after about 3 minutes; this only bounds a PC that went silent.
const READ_TIMEOUT_MS: u64 = 30_000;

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

/// The bridge's side of `/offer`: one prompt at a time, the latest ask's.
pub struct Knocker {
    link: Arc<Link>,
    shared: Arc<Shared>,
    runtime: Handle,
    shown: Mutex<Option<(String, AbortHandle)>>,
}

impl Knocker {
    pub fn new(link: Arc<Link>, shared: Arc<Shared>, runtime: Handle) -> Arc<Self> {
        Arc::new(Knocker { link, shared, runtime, shown: Mutex::new(None) })
    }
}

impl Switch for Knocker {
    fn open(&self) -> Result<(), Closed> {
        Ok(())
    }

    fn offer(&self, offer: Offer) -> Result<(), String> {
        let (link, shared) = (self.link.clone(), self.shared.clone());
        let name = format!("{} (Rig agent)", offer.owner.title);
        let ask = offer.ask.clone();
        let task = self.runtime.spawn(async move {
            let (answer, shown, allowed) = knock(&link, &name).await;
            if let Some(allowed) = allowed {
                link.adopt(allowed);
            }
            shared.events.push(EventKind::Offer { ask: offer.ask, nonce: offer.nonce, answer: answer.said(shown) });
        });
        if let Some((_, earlier)) = self.shown.lock().unwrap().replace((ask, task.abort_handle())) {
            earlier.abort();
        }
        Ok(())
    }

    fn offer_end(&self, ask: &str) -> bool {
        let mut shown = self.shown.lock().unwrap();
        match shown.take() {
            Some((held, task)) if held == ask => {
                let showing = !task.is_finished();
                task.abort();
                showing
            }
            other => {
                *shown = other;
                false
            }
        }
    }
}

/// One knock: the prompt is shown, and its person's answer read; whether the PC showed it; on Allow, the connection.
pub async fn knock(link: &Link, name: &str) -> (Answer, bool, Option<Allowed>) {
    let mut stream = match link.dial().await {
        Ok(stream) => stream,
        Err(e) => return (Answer::Lapsed(format!("cannot reach the PC at {}: {e}", link.label())), false, None),
    };
    let (answer, shown, peer_info) = answered(link, name, &mut stream).await;
    let allowed = peer_info.filter(|_| answer == Answer::Allowed).map(|peer_info| Allowed { stream, peer_info });
    (answer, shown, allowed)
}

async fn answered(link: &Link, name: &str, stream: &mut FramedStream) -> (Answer, bool, Option<Message>) {
    let mut waiting = false;
    loop {
        let bytes = match stream.next_timeout(READ_TIMEOUT_MS).await {
            Some(Ok(bytes)) => bytes,
            Some(Err(e)) => return (Answer::Lapsed(format!("connection error: {e}")), waiting, None),
            None => return (Answer::Lapsed("the PC closed the connection, or its prompt went unanswered".into()), waiting, None),
        };
        let Ok(msg) = Message::parse_from_bytes(&bytes) else { continue };
        let login = matches!(msg.union, Some(message::Union::LoginResponse(_))).then(|| msg.clone());
        let reply = match on_message(link, name, msg) {
            Step::Send(reply) => reply,
            Step::Waiting => {
                waiting = true;
                continue;
            }
            Step::Done(answer) => return (answer, waiting, login),
            Step::Nothing => continue,
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
    use base::message_proto::{Hash, LoginResponse, Misc, PeerInfo};

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
