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
use hbb_common::{bail, log, sha2::{Digest, Sha256}, ResultType};
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
/// The words a RustDesk client takes as "ask for the password".
const EMPTY_PASSWORD: &str = "Empty Password";
const WRONG_PASSWORD_LIMIT: u32 = 5;

pub struct Relay {
    link: Arc<Link>,
    shared: Arc<Shared>,
    password: String,
}


/// The human's input goes first: it reaches the PC, and the agent's actions wait until it pauses.
fn is_input(msg: &Message) -> bool {
    matches!(&msg.union, Some(message::Union::MouseEvent(_) | message::Union::KeyEvent(_) | message::Union::PointerDeviceEvent(_)))
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

/// A RustDesk client remembers sha256(password + salt), so the salt stays the same across connections and restarts, as a stock PC's does.
fn steady_salt(password: &str) -> String {
    let digest = Sha256::new().chain_update("rigdesk viewer salt\0").chain_update(password).finalize();
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
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
        let hash = Hash { salt: steady_salt(&self.password), challenge: fresh(), ..Default::default() };
        let mut msg = Message::new();
        msg.set_hash(hash.clone());
        stream.send(&msg).await?;
        let mut wrong = 0;
        let login = loop {
            let Some(Ok(bytes)) = stream.next_timeout(LOGIN_WAIT_MS).await else { bail!("no login") };
            let Ok(Message { union: Some(message::Union::LoginRequest(login)), .. }) = Message::parse_from_bytes(&bytes) else { continue };
            // A client with no saved password logs in empty first and asks for one only on this answer, on the same connection.
            if login.password.is_empty() {
                stream.send(&login_error(EMPTY_PASSWORD)).await?;
                continue;
            }
            if same(&login.password, &login_proof(&self.password, &hash)) {
                break login;
            }
            wrong += 1;
            log::info!("{} ({}) gave a wrong password", login.my_name, login.my_platform);
            tokio::time::sleep(REFUSAL_PAUSE).await;
            stream.send(&login_error(WRONG_PASSWORD)).await?;
            if wrong >= WRONG_PASSWORD_LIMIT {
                bail!("{} ({}) gave a wrong password {wrong} times", login.my_name, login.my_platform);
            }
        };
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
                    if is_input(&msg) {
                        self.shared.human_input("viewer input");
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
    /// event it receives. Like RustDesk, it takes any number of connections, such as the bridge's reach probes.
    async fn fake_pc(listener: tokio::net::TcpListener, password: &'static str, pointer: tokio::sync::mpsc::UnboundedSender<i32>) {
        loop {
            let (stream, addr) = listener.accept().await.expect("the bridge dials the PC");
            tokio::spawn(fake_connection(FramedStream::from(stream, addr), password, pointer.clone()));
        }
    }

    async fn fake_connection(mut pc: FramedStream, password: &'static str, pointer: tokio::sync::mpsc::UnboundedSender<i32>) {
        let hash = Hash { salt: "s".into(), challenge: "c".into(), ..Default::default() };
        let mut msg = Message::new();
        msg.set_hash(hash.clone());
        pc.send(&msg).await.expect("send");
        loop {
            let Some(Ok(bytes)) = pc.next_timeout(10_000).await else { return };
            let Ok(msg) = Message::parse_from_bytes(&bytes) else { return };
            match msg.union {
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

    #[tokio::test]
    async fn a_viewer_logs_in_to_the_bridge_sees_the_pc_and_its_click_takes_input_back_from_the_agent() {
        let pc = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let pc_port = pc.local_addr().expect("addr").port();
        let (pointer_tx, mut pointer) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(fake_pc(pc, "pc-pw", pointer_tx));
        let shared = Arc::new(Shared::new(rigdesk_agent::Events::new(64)));
        let link = Link::new(crate::link::Route::direct(&format!("127.0.0.1:{pc_port}")), "pc-pw".into(), "Rig agent".into(), shared.clone());
        tokio::spawn(link.clone().run());
        let minted = shared.mint();
        let owner = rigdesk_agent::Owner { agent_id: "a".into(), title: "front desk".into(), graph: "g".into() };
        shared.claim(&minted.grant, &minted.secret, owner).expect("claimed");
        let port = Relay::new(link, shared.clone(), "bridge-pw".into()).listen(0).await.expect("listening");

        let mut viewer = FramedStream::new(format!("127.0.0.1:{port}"), None, 5_000).await.expect("the viewer port answers");
        viewer.send(&Message::new()).await.expect("send");
        let Some(message::Union::Hash(hash)) = next(&mut viewer).await.union else { panic!("no challenge") };
        let mut welcome = Message::new();
        for (password, refusal) in [("", Some(EMPTY_PASSWORD)), ("guess", Some(WRONG_PASSWORD)), ("bridge-pw", None)] {
            let mut msg = Message::new();
            let proof = if password.is_empty() { Vec::new() } else { login_proof(password, &hash) };
            msg.set_login_request(base::message_proto::LoginRequest { password: proof.into(), my_name: "phone".into(), ..Default::default() });
            viewer.send(&msg).await.expect("send");
            welcome = next(&mut viewer).await;
            if let Some(words) = refusal {
                assert!(matches!(welcome.union, Some(message::Union::LoginResponse(LoginResponse { union: Some(login_response::Union::Error(ref e)), .. })) if e == words));
            }
        }
        let Some(message::Union::LoginResponse(LoginResponse { union: Some(login_response::Union::PeerInfo(info)), .. })) = welcome.union else {
            panic!("no PeerInfo: {welcome:?}")
        };
        assert_eq!(info.hostname, "pc");
        while !matches!(next(&mut viewer).await.union, Some(message::Union::VideoFrame(_))) {}

        viewer.send(&mouse(1 | (1 << 3))).await.expect("send");
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), pointer.recv()).await.expect("the click reaches the PC"), Some(1 | (1 << 3)));
        assert_eq!(shared.standing().holder, Some(Holder::Human));
    }

    #[tokio::test]
    async fn a_remembered_password_works_on_the_next_connection() {
        let shared = Arc::new(Shared::new(rigdesk_agent::Events::new(64)));
        let link = Link::new(crate::link::Route::direct("127.0.0.1:1"), "pc-pw".into(), "Rig agent".into(), shared.clone());
        let port = Relay::new(link, shared, "bridge-pw".into()).listen(0).await.expect("listening");
        let mut salts = Vec::new();
        for _ in 0..2 {
            let mut viewer = FramedStream::new(format!("127.0.0.1:{port}"), None, 5_000).await.expect("the viewer port answers");
            viewer.send(&Message::new()).await.expect("send");
            let Some(message::Union::Hash(hash)) = next(&mut viewer).await.union else { panic!("no challenge") };
            salts.push(hash.salt);
        }
        assert_eq!(salts[0], salts[1]);
    }

    #[tokio::test]
    async fn with_no_session_open_the_bridge_says_whether_the_pc_answers() {
        let shared = Arc::new(Shared::new(rigdesk_agent::Events::new(64)));
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = closed.local_addr().expect("addr").port();
        drop(closed);
        let link = Link::new(crate::link::Route::direct(&format!("127.0.0.1:{port}")), "pc-pw".into(), "Rig agent".into(), shared.clone());
        assert_eq!(shared.down(), Some(format!("127.0.0.1:{port} not checked yet")));
        tokio::spawn(link.run());
        let mut waited = 0;
        while shared.down().is_some_and(|why| why.ends_with("not checked yet")) && waited < 50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            waited += 1;
        }
        assert!(shared.down().is_some_and(|why| why.starts_with(&format!("nothing answers at 127.0.0.1:{port}"))), "{:?}", shared.down());
        assert!(shared.events.since(0).iter().any(|e| matches!(&e.kind, rigdesk_agent::EventKind::Reach { down: Some(_) })));
    }
}
