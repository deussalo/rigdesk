//! `--knock`: asks the person at the PC to let a Rig agent in, through RustDesk's own accept prompt. It logs in with no
//! password under the agent's name, which is the login RustDesk's "accept sessions via click" answers with its prompt:
//! the connection manager's Accept and Dismiss on a desktop, the "Do you accept?" notification and dialog on Android.
//! Nothing of the PC is used: once the PC answers, the connection closes. Ending this process takes the prompt away,
//! since the PC drops it with the connection. What happened is told on stdout, one line each: `waiting` once the PC
//! holds the login for its person, then `allowed`, `dismissed`, `refused: <the PC's words>` or `lapsed: <why>`.

use base::message_proto::{login_response, message, misc, Message, OptionMessage};
use hbb_common::tcp::FramedStream;
use hbb_common::protobuf::Message as _;

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
    pub fn line(&self) -> String {
        match self {
            Answer::Allowed => "allowed".into(),
            Answer::Dismissed => "dismissed".into(),
            Answer::Refused(words) => format!("refused: {words}"),
            Answer::Lapsed(why) => format!("lapsed: {why}"),
        }
        .replace('\n', " ")
    }
}

/// One knock: the prompt is shown, and its person's answer read.
pub async fn knock(link: &Link) -> Answer {
    let mut stream = match link.dial().await {
        Ok(stream) => stream,
        Err(e) => return Answer::Lapsed(format!("cannot reach the PC at {}: {e}", link.label())),
    };
    let answer = answered(link, &mut stream).await;
    drop(stream);
    answer
}

async fn answered(link: &Link, stream: &mut FramedStream) -> Answer {
    let mut waiting = false;
    loop {
        let bytes = match stream.next_timeout(READ_TIMEOUT_MS).await {
            Some(Ok(bytes)) => bytes,
            Some(Err(e)) => return Answer::Lapsed(format!("connection error: {e}")),
            None => return Answer::Lapsed("the PC closed the connection, or its prompt went unanswered".into()),
        };
        let Ok(msg) = Message::parse_from_bytes(&bytes) else { continue };
        let reply = match on_message(link, msg) {
            Step::Send(reply) => reply,
            Step::Waiting => {
                if !waiting {
                    waiting = true;
                    println!("waiting");
                }
                continue;
            }
            Step::Done(answer) => return answer,
            Step::Nothing => continue,
        };
        if let Err(e) = stream.send(&reply).await {
            return Answer::Lapsed(format!("connection error: {e}"));
        }
        if !waiting && matches!(reply.union, Some(message::Union::LoginRequest(_))) {
            waiting = true;
            println!("waiting");
        }
    }
}

enum Step {
    Send(Message),
    Waiting,
    Done(Answer),
    Nothing,
}

fn on_message(link: &Link, msg: Message) -> Step {
    match msg.union {
        Some(message::Union::Hash(hash)) => {
            let mut login = link.login(&hash, OptionMessage::default(), None);
            login.mut_login_request().password = Default::default();
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
    use std::sync::Arc;

    fn link() -> Arc<Link> {
        let shared = Arc::new(rigdesk_agent::Shared::new(rigdesk_agent::Events::new(8)));
        Link::new(crate::link::Route::direct("100.64.0.9"), String::new(), "a1 (Rig agent)".into(), shared)
    }

    fn login_response(union: login_response::Union) -> Message {
        let mut msg = Message::new();
        msg.set_login_response(LoginResponse { union: Some(union), ..Default::default() });
        msg
    }

    #[test]
    fn the_login_carries_the_agents_name_and_no_password() {
        let link = link();
        let mut hash = Message::new();
        hash.set_hash(Hash { salt: "s".into(), challenge: "c".into(), ..Default::default() });
        let Step::Send(sent) = on_message(&link, hash) else { panic!("a login goes back") };
        let login = sent.login_request();
        assert!(login.password.is_empty());
        assert_eq!(login.my_name, "a1 (Rig agent)");
        assert_eq!(login.my_id, "rig");
    }

    #[test]
    fn the_persons_answer_is_read() {
        let link = link();
        assert!(matches!(on_message(&link, login_response(login_response::Union::Error(NO_PASSWORD_ACCESS.into()))), Step::Waiting));
        assert!(matches!(on_message(&link, login_response(login_response::Union::PeerInfo(PeerInfo::new()))), Step::Done(Answer::Allowed)));
        let mut dismissed = Message::new();
        dismissed.set_misc(Misc { union: Some(misc::Union::CloseReason(CLOSED_BY_PERSON.into())), ..Default::default() });
        assert!(matches!(on_message(&link, dismissed), Step::Done(Answer::Dismissed)));
        let Step::Done(refused) = on_message(&link, login_response(login_response::Union::Error("Connection not allowed".into()))) else { panic!() };
        assert_eq!(refused.line(), "refused: Connection not allowed");
    }
}
