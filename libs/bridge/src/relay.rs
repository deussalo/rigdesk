//! The bridge's viewer port: a RigDesk GUI (or any RustDesk client) dials the bridge by direct IP, as it would a PC,
//! logs in with the bridge's own viewer password, and is handed the bridge's one session to the PC. It sees what the
//! PC sends; its pointer and keys go to the PC through the bridge, which so knows every human act: a click, wheel or
//! key while the agent holds input takes it back before it reaches the PC, and the pointer merely crossing the picture
//! is kept from the PC until then, so watching never disturbs the agent.
//!
//! Like the PC's own direct-IP port, the protocol here runs in the clear; listen on loopback and publish it on the
//! tailnet (`tailscale serve --tcp`), whose WireGuard carries it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base::message_proto::{login_response, message, misc, Hash, LoginResponse, Message, TestDelay};
use hbb_common::protobuf::Message as _;
use hbb_common::tcp::{new_listener, FramedStream};
use hbb_common::tokio::{self, sync::broadcast::error::RecvError};
use hbb_common::{bail, log, ResultType};
use rigdesk_agent::{Holder, Shared};

use crate::link::{login_proof, Link, ToViewer};

const LOGIN_WAIT_MS: u64 = 30_000;
const READ_TIMEOUT_MS: u64 = 60_000;
const SESSION_WAIT: Duration = Duration::from_secs(20);
const PING_EVERY: Duration = Duration::from_secs(3);
/// A wrong password holds the connection this long before it closes, to slow guessing.
const REFUSAL_PAUSE: Duration = Duration::from_secs(3);
/// The words a RustDesk client takes as "ask for the password again".
const WRONG_PASSWORD: &str = "Wrong Password";

pub struct Relay {
    link: Arc<Link>,
    shared: Arc<Shared>,
    password: String,
}

/// What one input from a viewer does: go to the PC, take input back from the agent first, or stay off the PC.
#[derive(Debug, PartialEq)]
enum Input {
    Forward,
    TakeBackThenForward,
    Hold,
}

/// A pointer move while the agent holds input is held back; any other pointer act or key takes input back first.
fn judge(msg: &Message, agent_holds: bool) -> Input {
    let pointer_move = match &msg.union {
        Some(message::Union::MouseEvent(m)) => m.mask & 0x7 == 0,
        Some(message::Union::KeyEvent(_) | message::Union::PointerDeviceEvent(_)) => false,
        _ => return Input::Forward,
    };
    match (agent_holds, pointer_move) {
        (false, _) => Input::Forward,
        (true, true) => Input::Hold,
        (true, false) => Input::TakeBackThenForward,
    }
}

/// What a viewer may send the PC: its input, clipboard, screenshots and the picture's own controls. Its pings and video
/// acknowledgements are the bridge's to answer, and anything opening other channels has no session here to go to.
fn to_pc(msg: &Message) -> bool {
    match &msg.union {
        Some(message::Union::MouseEvent(_) | message::Union::KeyEvent(_) | message::Union::PointerDeviceEvent(_)) => true,
        Some(message::Union::Clipboard(_) | message::Union::MultiClipboards(_) | message::Union::ScreenshotRequest(_)) => true,
        Some(message::Union::Misc(m)) => !matches!(
            m.union,
            Some(misc::Union::VideoReceived(_) | misc::Union::CloseReason(_) | misc::Union::SwitchSidesRequest(_))
        ),
        _ => false,
    }
}

fn fresh() -> String {
    format!("{:016x}{:016x}", hbb_common::rand::random::<u64>(), hbb_common::rand::random::<u64>())
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Relay {
    pub fn new(link: Arc<Link>, shared: Arc<Shared>, password: String) -> Arc<Self> {
        Arc::new(Relay { link, shared, password })
    }

    pub async fn listen(self: Arc<Self>, port: u16) -> ResultType<u16> {
        let listener = new_listener(SocketAddr::from(([127, 0, 0, 1], port)), true).await?;
        let port = listener.local_addr()?.port();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, addr)) => {
                        let relay = self.clone();
                        tokio::spawn(async move {
                            if let Err(e) = relay.viewer(FramedStream::from(stream, addr)).await {
                                log::info!("viewer left: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        log::warn!("viewer port: {e}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });
        Ok(port)
    }

    async fn viewer(&self, mut stream: FramedStream) -> ResultType<()> {
        let hash = Hash { salt: fresh(), challenge: fresh(), ..Default::default() };
        let mut msg = Message::new();
        msg.set_hash(hash.clone());
        stream.send(&msg).await?;
        let login = loop {
            let Some(Ok(bytes)) = stream.next_timeout(LOGIN_WAIT_MS).await else { bail!("no login") };
            if let Ok(Message { union: Some(message::Union::LoginRequest(login)), .. }) = Message::parse_from_bytes(&bytes) {
                break login;
            }
        };
        if !same(&login.password, &login_proof(&self.password, &hash)) {
            tokio::time::sleep(REFUSAL_PAUSE).await;
            stream.send(&login_error(WRONG_PASSWORD)).await?;
            bail!("{} ({}) gave a wrong password", login.my_name, login.my_platform);
        }
        if login.union.is_some() {
            stream.send(&login_error("The bridge relays the remote desktop only")).await?;
            bail!("{} asked for a session other than the desktop", login.my_name);
        }
        let mut from_pc = self.link.attach();
        let served = self.serve(&mut stream, &mut from_pc, &login.my_name).await;
        self.link.detach();
        served
    }

    async fn serve(&self, stream: &mut FramedStream, from_pc: &mut tokio::sync::broadcast::Receiver<ToViewer>, who: &str) -> ResultType<()> {
        let info = match self.link.peer_info(SESSION_WAIT).await {
            Ok(info) => info,
            Err(e) => {
                stream.send(&login_error(&e)).await?;
                bail!("{who} found no session to the PC: {e}");
            }
        };
        let mut msg = Message::new();
        msg.set_login_response(LoginResponse { union: Some(login_response::Union::PeerInfo(info)), ..Default::default() });
        stream.send(&msg).await?;
        log::info!("viewer {who} attached");
        self.link.refresh_video();
        let mut ping = tokio::time::interval(PING_EVERY);
        loop {
            tokio::select! {
                got = from_pc.recv() => match got {
                    Ok(ToViewer::Message(msg)) => stream.send(msg.as_ref()).await?,
                    Ok(ToViewer::Ended) => bail!("the session to the PC ended"),
                    Err(RecvError::Lagged(_)) => self.link.refresh_video(),
                    Err(RecvError::Closed) => bail!("the bridge is closing"),
                },
                incoming = stream.next_timeout(READ_TIMEOUT_MS) => {
                    let bytes = match incoming {
                        Some(Ok(bytes)) => bytes,
                        Some(Err(e)) => bail!("{who}: {e}"),
                        None => bail!("{who} went silent or closed the connection"),
                    };
                    let Ok(msg) = Message::parse_from_bytes(&bytes) else { continue };
                    if !to_pc(&msg) {
                        continue;
                    }
                    let agent_holds = self.shared.standing().holder == Some(Holder::Agent);
                    match judge(&msg, agent_holds) {
                        Input::Hold => continue,
                        Input::TakeBackThenForward => {
                            self.shared.human_input("viewer input");
                        }
                        Input::Forward => {}
                    }
                    self.link.forward(msg);
                }
                _ = ping.tick() => {
                    let mut msg = Message::new();
                    msg.set_test_delay(TestDelay { time: rigdesk_agent::now_ms() as i64, ..Default::default() });
                    stream.send(&msg).await?;
                }
            }
        }
    }
}

fn login_error(words: &str) -> Message {
    let mut msg = Message::new();
    msg.set_login_response(LoginResponse { union: Some(login_response::Union::Error(words.into())), ..Default::default() });
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::message_proto::{KeyEvent, MouseEvent};

    fn mouse(mask: i32) -> Message {
        let mut msg = Message::new();
        msg.set_mouse_event(MouseEvent { mask, x: 10, y: 10, ..Default::default() });
        msg
    }

    async fn next(stream: &mut FramedStream) -> Message {
        let bytes = stream.next_timeout(10_000).await.expect("a message").expect("readable");
        Message::parse_from_bytes(&bytes).expect("a message")
    }

    /// A stand-in stock PC: takes the bridge's login, answers a key-frame request with a frame, and reports each pointer
    /// event it receives.
    async fn fake_pc(listener: tokio::net::TcpListener, password: &str, pointer: tokio::sync::mpsc::UnboundedSender<i32>) {
        let (stream, addr) = listener.accept().await.expect("the bridge dials the PC");
        let mut pc = FramedStream::from(stream, addr);
        let hash = Hash { salt: "s".into(), challenge: "c".into(), ..Default::default() };
        let mut msg = Message::new();
        msg.set_hash(hash.clone());
        pc.send(&msg).await.expect("send");
        loop {
            match next(&mut pc).await.union {
                Some(message::Union::LoginRequest(login)) => {
                    assert_eq!(login.password.to_vec(), login_proof(password, &hash));
                    let info = base::message_proto::PeerInfo { hostname: "pc".into(), platform: "Windows".into(), ..Default::default() };
                    let mut msg = Message::new();
                    msg.set_login_response(LoginResponse { union: Some(login_response::Union::PeerInfo(info)), ..Default::default() });
                    pc.send(&msg).await.expect("send");
                }
                Some(message::Union::Misc(m)) if m.refresh_video() => {
                    let mut msg = Message::new();
                    msg.set_video_frame(base::message_proto::VideoFrame::new());
                    pc.send(&msg).await.expect("send");
                }
                Some(message::Union::MouseEvent(m)) => pointer.send(m.mask).expect("reported"),
                _ => {}
            }
        }
    }

    async fn log_in(port: u16, password: &str) -> (FramedStream, Message) {
        let mut viewer = FramedStream::new(format!("127.0.0.1:{port}"), None, 5_000).await.expect("the viewer port answers");
        viewer.send(&Message::new()).await.expect("send");
        let Some(message::Union::Hash(hash)) = next(&mut viewer).await.union else { panic!("no challenge") };
        let mut msg = Message::new();
        msg.set_login_request(base::message_proto::LoginRequest {
            password: login_proof(password, &hash).into(),
            my_name: "phone".into(),
            ..Default::default()
        });
        viewer.send(&msg).await.expect("send");
        let answer = next(&mut viewer).await;
        (viewer, answer)
    }

    #[tokio::test]
    async fn a_viewer_logs_in_to_the_bridge_sees_the_pc_and_its_click_takes_input_back_from_the_agent() {
        let pc = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let pc_port = pc.local_addr().expect("addr").port();
        let (pointer_tx, mut pointer) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(fake_pc(pc, "pc-pw", pointer_tx));
        let link = Link::new(format!("127.0.0.1:{pc_port}"), "pc-pw".into(), "Rig agent".into());
        tokio::spawn(link.clone().run());
        let shared = Arc::new(Shared::new(rigdesk_agent::Events::new(64)));
        let minted = shared.mint();
        let owner = rigdesk_agent::Owner { agent_id: "a".into(), title: "front desk".into(), graph: "g".into() };
        shared.claim(&minted.grant, &minted.secret, owner).expect("claimed");
        let port = Relay::new(link, shared.clone(), "bridge-pw".into()).listen(0).await.expect("listening");

        let (_, refused) = log_in(port, "guess").await;
        assert!(matches!(refused.union, Some(message::Union::LoginResponse(LoginResponse { union: Some(login_response::Union::Error(ref e)), .. })) if e == WRONG_PASSWORD));

        let (mut viewer, welcome) = log_in(port, "bridge-pw").await;
        let Some(message::Union::LoginResponse(LoginResponse { union: Some(login_response::Union::PeerInfo(info)), .. })) = welcome.union else {
            panic!("no PeerInfo: {welcome:?}")
        };
        assert_eq!(info.hostname, "pc");
        while !matches!(next(&mut viewer).await.union, Some(message::Union::VideoFrame(_))) {}

        viewer.send(&mouse(0)).await.expect("send");
        assert_eq!(shared.standing().holder, Some(Holder::Agent));
        viewer.send(&mouse(1 | (1 << 3))).await.expect("send");
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), pointer.recv()).await.expect("the click reaches the PC"), Some(1 | (1 << 3)));
        assert_eq!(shared.standing().holder, Some(Holder::Human));
    }

    #[test]
    fn while_the_agent_holds_input_a_click_or_key_takes_it_back_and_a_passing_pointer_is_held() {
        let mut key = Message::new();
        key.set_key_event(KeyEvent::new());
        assert_eq!(judge(&mouse(0), true), Input::Hold);
        assert_eq!(judge(&mouse(1 | (1 << 3)), true), Input::TakeBackThenForward);
        assert_eq!(judge(&mouse(3), true), Input::TakeBackThenForward);
        assert_eq!(judge(&key, true), Input::TakeBackThenForward);
        assert_eq!(judge(&mouse(0), false), Input::Forward);
        assert_eq!(judge(&key, false), Input::Forward);
    }
}
