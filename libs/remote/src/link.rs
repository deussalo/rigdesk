//! The RustDesk session with the stock PC: a direct TCP connection to its direct-IP port, the password login, and the
//! messages the agent's tools need (screenshots, the cursor the PC reports, its clipboard). It connects when a tool
//! first asks and lets go after `IDLE`, so the PC shows a RustDesk session only while the agent is at work.
//!
//! No RustDesk-layer encryption: a direct-IP connection has no rendezvous server to vouch for the PC's key, so stock
//! RustDesk itself sends such sessions in the clear. The tailnet's WireGuard carries them, as it does every RigDesk PC.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::Duration;

use base::message_proto::{
    login_response, message, misc, option_message::BoolOption, supported_decoding::PreferCodec, ClipboardFormat,
    DisplayInfo, Hash, ImageQuality, LoginRequest, Message, OptionMessage, ScreenshotRequest, SupportedDecoding,
};
use hbb_common::protobuf::{Message as _, MessageField};
use hbb_common::sha2::{Digest, Sha256};
use hbb_common::tcp::FramedStream;
use hbb_common::tokio::{self, sync::mpsc, sync::Notify};
use hbb_common::{bail, log, ResultType};

/// RustDesk's direct-IP port.
const DIRECT_PORT: u16 = 21118;
const CONNECT_TIMEOUT_MS: u64 = 10_000;
/// The PC sends test-delay pings every few seconds; nothing at all for this long means the link is dead.
const READ_TIMEOUT_MS: u64 = 30_000;
/// No tool call for this long and the session is closed.
const IDLE: Duration = Duration::from_secs(10 * 60);
/// A refused login waits this long before trying again, well clear of the PC's lockout after repeated failures.
const LOGIN_RETRY: Duration = Duration::from_secs(5 * 60);
/// The PC reports a cursor move this soon after the agent's own input as the tail of it.
const OWN_INPUT_SLACK_MS: u64 = 500;
/// The protocol version this client speaks, the RustDesk release its code is taken from.
const CLIENT_VERSION: &str = "1.5.1";

#[derive(Clone, Default)]
pub struct Peer {
    pub hostname: String,
    pub platform: String,
    pub version: String,
    pub displays: Vec<DisplayInfo>,
    pub current: usize,
}

#[derive(Default)]
struct State {
    out: Option<mpsc::UnboundedSender<Message>>,
    peer: Option<Peer>,
    /// Why the last attempt or session ended, and when (ms).
    failure: Option<(String, u64)>,
    clipboard: Option<String>,
}

pub struct Link {
    addr: String,
    username: String,
    password: String,
    name: String,
    state: Mutex<State>,
    demand: Notify,
    last_use_ms: AtomicU64,
    injected_ms: AtomicU64,
    injected_at: Mutex<(i32, i32)>,
    foreign_move_ms: AtomicU64,
    shots: Mutex<HashMap<String, std_mpsc::Sender<Result<Vec<u8>, String>>>>,
}

fn now_ms() -> u64 {
    rigdesk_agent::now_ms()
}

/// Where to dial and what to give the PC as the login username: a stock PC accepts a direct-IP login whose username is
/// the address it was dialled at.
pub fn target(peer: &str) -> (String, String) {
    let peer = peer.trim();
    if peer.parse::<SocketAddr>().is_ok() {
        return (peer.to_string(), peer.to_string());
    }
    if let Ok(ip) = peer.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>() {
        return (SocketAddr::new(ip, DIRECT_PORT).to_string(), peer.to_string());
    }
    if peer.contains(':') {
        return (peer.to_string(), peer.to_string());
    }
    (format!("{peer}:{DIRECT_PORT}"), format!("{peer}:{DIRECT_PORT}"))
}

/// The login proof: SHA-256 of the salted password hash and the PC's one-time challenge.
pub fn login_proof(password: &str, hash: &Hash) -> Vec<u8> {
    let salted = Sha256::new().chain_update(password).chain_update(&hash.salt).finalize();
    Sha256::new().chain_update(&salted[..]).chain_update(&hash.challenge).finalize().to_vec()
}

impl Link {
    pub fn new(peer: String, password: String, name: String) -> Arc<Self> {
        let (addr, username) = target(&peer);
        Arc::new(Link {
            addr,
            username,
            password,
            name,
            state: Mutex::new(State::default()),
            demand: Notify::new(),
            last_use_ms: AtomicU64::new(0),
            injected_ms: AtomicU64::new(0),
            injected_at: Mutex::new((i32::MIN, i32::MIN)),
            foreign_move_ms: AtomicU64::new(0),
            shots: Mutex::new(HashMap::new()),
        })
    }

    fn wanted(&self) -> bool {
        now_ms().saturating_sub(self.last_use_ms.load(Ordering::SeqCst)) < IDLE.as_millis() as u64
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            if !self.wanted() {
                self.demand.notified().await;
                continue;
            }
            let started = now_ms();
            let ended = self.session().await;
            {
                let mut state = self.state.lock().unwrap();
                state.out = None;
                state.peer = None;
                if let Err(e) = &ended {
                    state.failure = Some((e.to_string(), now_ms()));
                }
            }
            for (_, waiter) in self.shots.lock().unwrap().drain() {
                let _ = waiter.send(Err("the RustDesk session to the PC closed".into()));
            }
            match ended {
                Ok(()) => log::info!("closed the session to {} after it went idle", self.addr),
                Err(e) => {
                    log::warn!("session to {} ended: {e}", self.addr);
                    let pause = if e.to_string().starts_with("login refused") {
                        LOGIN_RETRY
                    } else if now_ms() - started > 60_000 {
                        Duration::from_secs(1)
                    } else {
                        Duration::from_secs(5)
                    };
                    tokio::time::sleep(pause).await;
                }
            }
        }
    }

    async fn session(&self) -> ResultType<()> {
        let local: SocketAddr = (if self.addr.starts_with('[') { "[::]:0" } else { "0.0.0.0:0" }).parse()?;
        let mut stream = FramedStream::new(self.addr.as_str(), Some(local), CONNECT_TIMEOUT_MS).await?;
        // Declines the key exchange; see the module note.
        stream.send(&Message::new()).await?;
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let mut idle_check = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                incoming = stream.next_timeout(READ_TIMEOUT_MS) => {
                    let bytes = match incoming {
                        Some(Ok(bytes)) => bytes,
                        Some(Err(e)) => bail!("connection error: {e}"),
                        None => bail!("the PC went silent or closed the connection"),
                    };
                    let Ok(msg) = Message::parse_from_bytes(&bytes) else { continue };
                    if let Some(reply) = self.on_message(msg, &tx)? {
                        stream.send(&reply).await?;
                    }
                }
                Some(out) = rx.recv() => stream.send(&out).await?,
                _ = idle_check.tick() => {
                    if !self.wanted() {
                        return Ok(());
                    }
                }
            }
        }
    }

    fn on_message(&self, msg: Message, tx: &mpsc::UnboundedSender<Message>) -> ResultType<Option<Message>> {
        match msg.union {
            Some(message::Union::Hash(hash)) => return Ok(Some(self.login(&hash))),
            Some(message::Union::LoginResponse(res)) => match res.union {
                Some(login_response::Union::Error(e)) => bail!("login refused by the PC: {e}"),
                Some(login_response::Union::PeerInfo(pi)) => {
                    log::info!("logged in to {} ({} {}, RustDesk {})", self.addr, pi.hostname, pi.platform, pi.version);
                    let mut state = self.state.lock().unwrap();
                    state.peer = Some(Peer {
                        hostname: pi.hostname.clone(),
                        platform: pi.platform.clone(),
                        version: pi.version.clone(),
                        displays: pi.displays.clone(),
                        current: pi.current_display.max(0) as usize,
                    });
                    state.out = Some(tx.clone());
                    state.failure = None;
                }
                None => {}
            },
            Some(message::Union::PeerInfo(pi)) => {
                if let Some(peer) = self.state.lock().unwrap().peer.as_mut() {
                    peer.displays = pi.displays.clone();
                    peer.current = pi.current_display.max(0) as usize;
                }
            }
            Some(message::Union::TestDelay(t)) if !t.from_client => {
                let mut reply = Message::new();
                reply.set_test_delay(t);
                return Ok(Some(reply));
            }
            Some(message::Union::CursorPosition(pos)) => self.on_cursor(pos.x, pos.y),
            Some(message::Union::ScreenshotResponse(res)) => {
                if let Some(waiter) = self.shots.lock().unwrap().remove(&res.sid) {
                    let _ = waiter.send(if res.msg.is_empty() { Ok(res.data.to_vec()) } else { Err(res.msg) });
                }
            }
            Some(message::Union::Clipboard(cb)) => self.on_clipboard(cb),
            Some(message::Union::MultiClipboards(mcb)) => {
                if let Some(cb) = mcb.clipboards.into_iter().find(|c| c.format.enum_value() == Ok(ClipboardFormat::Text)) {
                    self.on_clipboard(cb);
                }
            }
            Some(message::Union::Misc(m)) => match m.union {
                Some(misc::Union::CloseReason(reason)) => bail!("the PC closed the session: {reason}"),
                Some(misc::Union::SwitchDisplay(sd)) => {
                    if let Some(peer) = self.state.lock().unwrap().peer.as_mut() {
                        peer.current = sd.display.max(0) as usize;
                    }
                }
                _ => {}
            },
            Some(message::Union::MessageBox(mb)) => {
                log::info!("the PC says ({}): {} {}", mb.msgtype, mb.title, mb.text);
                if mb.msgtype.contains("error") {
                    bail!("the PC refused: {} {}", mb.title, mb.text);
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn login(&self, hash: &Hash) -> Message {
        let option = OptionMessage {
            image_quality: ImageQuality::Low.into(),
            custom_fps: 5,
            show_remote_cursor: BoolOption::Yes.into(),
            disable_audio: BoolOption::Yes.into(),
            supported_decoding: MessageField::some(SupportedDecoding {
                ability_vp9: 1,
                ability_vp8: 1,
                ability_av1: 1,
                prefer: PreferCodec::VP9.into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut msg = Message::new();
        msg.set_login_request(LoginRequest {
            username: self.username.clone(),
            password: login_proof(&self.password, hash).into(),
            my_id: "rig".into(),
            my_name: self.name.clone(),
            my_platform: std::env::consts::OS.into(),
            option: MessageField::some(option),
            session_id: rand_id(),
            version: CLIENT_VERSION.into(),
            ..Default::default()
        });
        msg
    }

    fn on_cursor(&self, x: i32, y: i32) {
        let ours = *self.injected_at.lock().unwrap() == (x, y);
        let after_own = now_ms().saturating_sub(self.injected_ms.load(Ordering::SeqCst)) > OWN_INPUT_SLACK_MS;
        if !ours && after_own {
            self.foreign_move_ms.store(now_ms(), Ordering::SeqCst);
        }
    }

    fn on_clipboard(&self, cb: base::message_proto::Clipboard) {
        let bytes = if cb.compress { hbb_common::compress::decompress(&cb.content) } else { cb.content.to_vec() };
        if let Ok(text) = String::from_utf8(bytes) {
            self.state.lock().unwrap().clipboard = Some(text);
        }
    }

    /// Open the session if it is not, and wait for the login, up to `wait`.
    pub fn ensure(&self, wait: Duration) -> Result<(mpsc::UnboundedSender<Message>, Peer), String> {
        let asked = now_ms();
        self.last_use_ms.store(asked, Ordering::SeqCst);
        self.demand.notify_one();
        let deadline = std::time::Instant::now() + wait;
        loop {
            {
                let state = self.state.lock().unwrap();
                if let (Some(out), Some(peer)) = (&state.out, &state.peer) {
                    return Ok((out.clone(), peer.clone()));
                }
                if let Some((why, at)) = &state.failure {
                    if *at >= asked {
                        return Err(format!("cannot reach the PC at {}: {why}", self.addr));
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                let last = self.state.lock().unwrap().failure.clone().map(|(why, _)| format!(" (last error: {why})")).unwrap_or_default();
                return Err(format!("no RustDesk session to the PC at {} yet{last}", self.addr));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Send the agent's input, remembering it so the PC's report of the same cursor move is not taken for a person's.
    pub fn inject(&self, out: &mpsc::UnboundedSender<Message>, msgs: Vec<Message>) -> Result<(), String> {
        for msg in msgs {
            if let Some(message::Union::MouseEvent(m)) = &msg.union {
                if m.mask & 0x7 != 3 {
                    *self.injected_at.lock().unwrap() = (m.x, m.y);
                }
            }
            out.send(msg).map_err(|_| "the RustDesk session to the PC closed".to_string())?;
        }
        self.injected_ms.store(now_ms(), Ordering::SeqCst);
        Ok(())
    }

    pub fn foreign_move_after(&self, since_ms: u64) -> bool {
        let quiet_from = since_ms.max(self.injected_ms.load(Ordering::SeqCst)) + OWN_INPUT_SLACK_MS;
        self.foreign_move_ms.load(Ordering::SeqCst) > quiet_from
    }

    pub fn screenshot(&self, out: &mpsc::UnboundedSender<Message>, display: usize, wait: Duration) -> Result<Vec<u8>, String> {
        let sid = format!("{:016x}", rand_id());
        let (tx, rx) = std_mpsc::channel();
        self.shots.lock().unwrap().insert(sid.clone(), tx);
        let mut msg = Message::new();
        msg.set_screenshot_request(ScreenshotRequest { display: display as i32, sid: sid.clone(), ..Default::default() });
        out.send(msg).map_err(|_| "the RustDesk session to the PC closed".to_string())?;
        let got = rx.recv_timeout(wait);
        self.shots.lock().unwrap().remove(&sid);
        match got {
            Ok(r) => r,
            Err(_) => Err("the PC did not answer the screenshot request; RustDesk 1.4 or newer is needed on the PC".into()),
        }
    }

    pub fn clipboard(&self) -> Option<String> {
        self.state.lock().unwrap().clipboard.clone()
    }

    pub fn set_clipboard(&self, out: &mpsc::UnboundedSender<Message>, text: &str) -> Result<(), String> {
        let mut msg = Message::new();
        msg.set_clipboard(base::message_proto::Clipboard {
            content: text.as_bytes().to_vec().into(),
            format: ClipboardFormat::Text.into(),
            ..Default::default()
        });
        out.send(msg).map_err(|_| "the RustDesk session to the PC closed".to_string())?;
        self.state.lock().unwrap().clipboard = Some(text.to_string());
        Ok(())
    }
}

fn rand_id() -> u64 {
    hbb_common::rand::random::<u64>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_address_dials_the_direct_port_and_logs_in_under_it() {
        assert_eq!(target("100.64.1.2"), ("100.64.1.2:21118".into(), "100.64.1.2".into()));
        assert_eq!(target("deus.tail1234.ts.net"), ("deus.tail1234.ts.net:21118".into(), "deus.tail1234.ts.net:21118".into()));
        assert_eq!(target("100.64.1.2:4000"), ("100.64.1.2:4000".into(), "100.64.1.2:4000".into()));
    }

    #[test]
    fn the_login_proof_hashes_password_salt_then_challenge() {
        let hash = Hash { salt: "salt".into(), challenge: "challenge".into(), ..Default::default() };
        let salted = Sha256::digest(b"pwsalt");
        let mut both = salted.to_vec();
        both.extend_from_slice(b"challenge");
        assert_eq!(login_proof("pw", &hash), Sha256::digest(&both).to_vec());
    }
}
