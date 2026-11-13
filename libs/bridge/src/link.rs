//! The RustDesk session with the stock PC: a direct TCP connection to its direct-IP port, the password login, and the
//! messages the agent's tools need (screenshots, the cursor the PC reports, its clipboard). It connects when a tool
//! first asks or a viewer attaches, and lets go after `IDLE` with no viewer, so the PC shows a RustDesk session only
//! while it is in use. What the PC sends is also relayed to the attached viewers (see `relay`).
//!
//! The PC is reached one of two ways (`Route`). By direct IP there is no RustDesk-layer encryption: no rendezvous
//! server vouches for the PC's key, so stock RustDesk itself sends such sessions in the clear, and the tailnet's
//! WireGuard carries them, as it does every RigDesk PC. By ID, through our own hbbs and hbbr, the session is
//! encrypted end to end (see `rendezvous`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::Duration;

use base::message_proto::{
    login_request, login_response, message, misc, option_message::BoolOption, supported_decoding::PreferCodec, ClipboardFormat,
    DisplayInfo, Hash, ImageQuality, LoginRequest, Message, Misc, OptionMessage, PeerInfo, ScreenshotRequest,
    SupportedDecoding,
};
use hbb_common::protobuf::{Message as _, MessageField};
use hbb_common::sha2::{Digest, Sha256};
use hbb_common::tcp::FramedStream;
use hbb_common::tokio::{self, sync::broadcast, sync::mpsc, sync::Notify};
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
/// Messages from the PC a slow viewer may fall behind by before it is told to catch up from a fresh key frame.
const RELAY_BACKLOG: usize = 256;

/// What the session sends its attached viewers.
#[derive(Clone)]
pub enum ToViewer {
    Message(Arc<Message>),
    /// The session to the PC ended; a viewer reconnects to get the next one.
    Ended,
}

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
    /// The PC's login answer as it sent it, handed to each viewer as its own.
    info: Option<PeerInfo>,
}

/// How the bridge reaches the PC.
#[derive(Clone, Debug)]
pub enum Route {
    /// Its direct-IP port, over the tailnet.
    Direct { addr: String },
    /// Its RustDesk ID, registered with our ID server, through our relay.
    Id { id: String, server: crate::rendezvous::Server },
}

impl Route {
    /// The route for a `--peer`, and the login username the PC expects on it: a stock PC accepts a direct-IP login
    /// whose username is the address it was dialled at, and an ID login under its ID.
    pub fn direct(peer: &str) -> (Route, String) {
        let (addr, username) = target(peer);
        (Route::Direct { addr }, username)
    }

    pub fn by_id(id: String, server: crate::rendezvous::Server) -> (Route, String) {
        (Route::Id { id: id.clone(), server }, id)
    }

    pub fn label(&self) -> String {
        match self {
            Route::Direct { addr } => addr.clone(),
            Route::Id { id, server } => format!("ID {id} via {}", server.host),
        }
    }
}

pub struct Link {
    route: Route,
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
    viewers: AtomicUsize,
    to_viewers: broadcast::Sender<ToViewer>,
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
    pub fn new((route, username): (Route, String), password: String, name: String) -> Arc<Self> {
        Arc::new(Link {
            addr: route.label(),
            route,
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
            viewers: AtomicUsize::new(0),
            to_viewers: broadcast::channel(RELAY_BACKLOG).0,
        })
    }

    fn wanted(&self) -> bool {
        self.viewers.load(Ordering::SeqCst) > 0 || now_ms().saturating_sub(self.last_use_ms.load(Ordering::SeqCst)) < IDLE.as_millis() as u64
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
                state.info = None;
                if let Err(e) = &ended {
                    state.failure = Some((e.to_string(), now_ms()));
                }
            }
            let _ = self.to_viewers.send(ToViewer::Ended);
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

    /// A new connection to the PC, ready for its login challenge. The screen session holds one; each shell command
    /// opens its own beside it.
    pub async fn dial(&self) -> ResultType<FramedStream> {
        Ok(match &self.route {
            Route::Direct { addr } => {
                let local: SocketAddr = (if addr.starts_with('[') { "[::]:0" } else { "0.0.0.0:0" }).parse()?;
                let mut stream = FramedStream::new(addr.as_str(), Some(local), CONNECT_TIMEOUT_MS).await?;
                // Declines the key exchange; see the module note.
                stream.send(&Message::new()).await?;
                stream
            }
            Route::Id { id, server } => crate::rendezvous::dial(server, id).await?,
        })
    }

    async fn session(&self) -> ResultType<()> {
        let mut stream = self.dial().await?;
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let mut idle_check = tokio::time::interval(Duration::from_secs(5));
        // next_timeout restarts with every select! round, so the idle tick alone would keep a dead link looking alive.
        let mut heard = std::time::Instant::now();
        loop {
            tokio::select! {
                incoming = stream.next_timeout(READ_TIMEOUT_MS) => {
                    let bytes = match incoming {
                        Some(Ok(bytes)) => {
                            heard = std::time::Instant::now();
                            bytes
                        }
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
                    if heard.elapsed() > Duration::from_millis(READ_TIMEOUT_MS) {
                        bail!("the PC sent nothing for {} s; reconnecting", READ_TIMEOUT_MS / 1000);
                    }
                }
            }
        }
    }

    fn on_message(&self, msg: Message, tx: &mpsc::UnboundedSender<Message>) -> ResultType<Option<Message>> {
        if self.to_viewers.receiver_count() > 0 && self.relayed(&msg) {
            let _ = self.to_viewers.send(ToViewer::Message(Arc::new(msg.clone())));
        }
        match msg.union {
            Some(message::Union::Hash(hash)) => {
                let (image_quality, custom_fps) = self.quality();
                let option = OptionMessage {
                    image_quality: image_quality.into(),
                    custom_fps,
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
                return Ok(Some(self.login(&hash, option, None)));
            }
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
                    state.info = Some(pi);
                    state.out = Some(tx.clone());
                    state.failure = None;
                }
                _ => {}
            },
            Some(message::Union::PeerInfo(pi)) => {
                let mut state = self.state.lock().unwrap();
                if let Some(peer) = state.peer.as_mut() {
                    peer.displays = pi.displays.clone();
                    peer.current = pi.current_display.max(0) as usize;
                }
                if let Some(info) = state.info.as_mut() {
                    info.displays = pi.displays;
                    info.current_display = pi.current_display;
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
                    let mut state = self.state.lock().unwrap();
                    if let Some(peer) = state.peer.as_mut() {
                        peer.current = sd.display.max(0) as usize;
                    }
                    if let Some(info) = state.info.as_mut() {
                        info.current_display = sd.display;
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

    /// What viewers see of the PC: all but the login, the PC's pings and the answers to the agent's own screenshots.
    fn relayed(&self, msg: &Message) -> bool {
        match &msg.union {
            Some(message::Union::Hash(_) | message::Union::LoginResponse(_) | message::Union::TestDelay(_)) => false,
            Some(message::Union::SignedId(_) | message::Union::PublicKey(_)) => false,
            Some(message::Union::ScreenshotResponse(res)) => !self.shots.lock().unwrap().contains_key(&res.sid),
            Some(message::Union::Misc(m)) => !matches!(m.union, Some(misc::Union::CloseReason(_))),
            _ => true,
        }
    }

    /// The agent reads still screenshots, so the stream is kept lean until a person watches.
    fn quality(&self) -> (ImageQuality, i32) {
        if self.viewers.load(Ordering::SeqCst) > 0 {
            (ImageQuality::Balanced, 30)
        } else {
            (ImageQuality::Low, 5)
        }
    }

    /// The answer to the PC's login challenge: a screen session, or with `kind`, another kind such as a terminal.
    pub fn login(&self, hash: &Hash, option: OptionMessage, kind: Option<login_request::Union>) -> Message {
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
            union: kind,
            ..Default::default()
        });
        msg
    }

    pub fn label(&self) -> &str {
        &self.addr
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

    /// A viewer joins: the session opens if it is not, keeps open while any viewer stays, and streams for watching.
    pub fn attach(&self) -> broadcast::Receiver<ToViewer> {
        let rx = self.to_viewers.subscribe();
        if self.viewers.fetch_add(1, Ordering::SeqCst) == 0 {
            self.send_quality();
        }
        self.demand.notify_one();
        rx
    }

    pub fn detach(&self) {
        if self.viewers.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.send_quality();
            self.last_use_ms.store(now_ms(), Ordering::SeqCst);
        }
    }

    fn send_quality(&self) {
        let (image_quality, custom_fps) = self.quality();
        let mut misc = Misc::new();
        misc.set_option(OptionMessage { image_quality: image_quality.into(), custom_fps, ..Default::default() });
        let mut msg = Message::new();
        msg.set_misc(misc);
        self.forward(msg);
    }

    /// Asks the PC for a fresh key frame, which a viewer that joined or fell behind decodes from.
    pub fn refresh_video(&self) {
        let mut misc = Misc::new();
        misc.set_refresh_video(true);
        let mut msg = Message::new();
        msg.set_misc(misc);
        self.forward(msg);
    }

    /// A viewer's message to the PC, sent as is; dropped when no session is open.
    pub fn forward(&self, msg: Message) {
        if let Some(out) = &self.state.lock().unwrap().out {
            let _ = out.send(msg);
        }
    }

    /// The PC's login answer once the session is up, waiting up to `wait` for it.
    pub async fn peer_info(&self, wait: Duration) -> Result<PeerInfo, String> {
        let asked = now_ms();
        self.demand.notify_one();
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            {
                let state = self.state.lock().unwrap();
                if let (Some(_), Some(info)) = (&state.out, &state.info) {
                    return Ok(info.clone());
                }
                if let Some((why, at)) = &state.failure {
                    if *at >= asked {
                        return Err(format!("cannot reach the PC: {why}"));
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("no session to the PC yet".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
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
