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
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use base::message_proto::{
    login_request, login_response, message, misc, option_message::BoolOption, supported_decoding::PreferCodec, ClipboardFormat,
    DisplayInfo, Hash, ImageQuality, LoginRequest, Message, Misc, OptionMessage, PeerInfo, RigKnock, ScreenshotRequest,
    SupportedDecoding,
};
use hbb_common::protobuf::{Message as _, MessageField};
use hbb_common::sha2::{Digest, Sha256};
use hbb_common::tcp::FramedStream;
use hbb_common::tokio::{self, sync::broadcast, sync::mpsc, sync::Notify};
use hbb_common::{bail, log, ResultType};
use rigdesk_agent::operator::NoLogin;
use rigdesk_agent::Owner;

/// RustDesk's direct-IP port.
const DIRECT_PORT: u16 = 21118;
const CONNECT_TIMEOUT_MS: u64 = 10_000;
/// The PC sends test-delay pings every few seconds; nothing at all for this long means the link is dead.
const READ_TIMEOUT_MS: u64 = 30_000;
/// No tool call for this long and the session is closed.
const IDLE: Duration = Duration::from_secs(10 * 60);
/// With no session open, whether the PC can be reached is checked this often, without opening one.
const PROBE_EVERY: Duration = Duration::from_secs(30);
const PROBE_WAIT: Duration = Duration::from_secs(5);
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
    failure: Option<(Failure, u64)>,
    /// The platform the PC said at its last login, kept across sessions: Rig's Accept names a phone or a PC by it.
    platform: Option<String>,
    clipboard: Option<String>,
    /// The PC's login answer as it sent it, handed to each viewer as its own.
    info: Option<PeerInfo>,
}

/// Why the agent's claim ends when the person at the PC closes the session; the plugin names the PC in it.
pub const PERSON_ENDED: &str = "the person at the PC ended it";

/// Why the last attempt or session to the PC ended, by what the operator's Accept is told.
#[derive(Clone, Debug)]
enum Failure {
    /// Nothing took the connection, or it closed before the PC asked for a login: no login was tried.
    Unreached(String),
    /// The PC answered the login with a refusal, in its words.
    Refused(String),
    Ended(String),
}

impl Failure {
    fn of(e: &hbb_common::anyhow::Error) -> Failure {
        if let Some(Unreached(why)) = e.downcast_ref::<Unreached>() {
            Failure::Unreached(why.clone())
        } else if let Some(LoginRefused(words)) = e.downcast_ref::<LoginRefused>() {
            Failure::Refused(words.clone())
        } else {
            Failure::Ended(e.to_string())
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Unreached(why) | Failure::Ended(why) => f.write_str(why),
            Failure::Refused(words) => write!(f, "{}", LoginRefused(words.clone())),
        }
    }
}

/// The connection to the PC failed before it asked for a login.
#[derive(Debug)]
struct Unreached(String);

impl std::fmt::Display for Unreached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreached {}

/// The PC answered the login with a refusal, in its words.
#[derive(Debug)]
struct LoginRefused(String);

impl std::fmt::Display for LoginRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "login refused by the PC: {}", self.0)
    }
}

impl std::error::Error for LoginRefused {}

/// The PC closed the session because the person at it closed it there.
#[derive(Debug)]
struct PersonClosed;

impl std::fmt::Display for PersonClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the person at the PC closed the session")
    }
}

impl std::error::Error for PersonClosed {}

/// What a RigDesk PC answers a login under an ID not its own; the plugin takes it to mean "try the next PC".
const NOT_THE_INVITED_DEVICE: &str = "Offline";

/// One connection's redemption of an invite.
#[derive(Default)]
struct InviteLogin {
    /// Our address on this connection, which is the dialer the device sees.
    dialer: Option<std::net::IpAddr>,
    /// The greeting the proof answered and the proof's signature.
    sent: Option<(Hash, Vec<u8>)>,
    accepted: bool,
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
    /// Where whether the PC can be reached is published, for /health and the plugin.
    shared: Arc<rigdesk_agent::Shared>,
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
    /// One session, for an ad-hoc join: opened at once, held while the bridge lives, and the bridge exits when it ends.
    once: AtomicBool,
    logged_in: AtomicBool,
    /// A connection the person at the PC allowed (`knock`), to carry the next session instead of a password login.
    adopted: Mutex<Option<crate::knock::Allowed>>,
    /// Ends the session running now, so the adopted connection takes its place.
    swap: Notify,
    /// The session running now is on an adopted connection.
    carried: AtomicBool,
    /// Hangs up the adopted session running now (`hang_up`).
    hang: AtomicBool,
    /// The person at the PC closed the last session: nothing dials it, for a tool, a viewer or the saved password, until Rig is let in
    /// again, by a login on a new join (`log_in`), a connection the person allows (`adopt`) or a new claim.
    closed_by_person: AtomicBool,
    /// An invite the device made: the session login presents it in place of the password.
    invite: OnceLock<crate::invite::Invite>,
    invite_login: Mutex<InviteLogin>,
    /// Rig's pairing key: a password login to a device that asks for Rig's proof is signed with it, so the person there is
    /// told which agent joined.
    pair_key: OnceLock<PathBuf>,
    /// The agent the login Rig's Accept asks for is made for, with the ask the operator accepted, while that login runs.
    signs_for: Mutex<Option<(Owner, String)>>,
    /// The address the session running now dials from, as the device sees it.
    dialer: Mutex<Option<IpAddr>>,
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
    pub fn new((route, username): (Route, String), password: String, name: String, shared: Arc<rigdesk_agent::Shared>) -> Arc<Self> {
        // Until the first probe answers, the PC is not known to be reachable; /health says so rather than nothing.
        shared.set_down(Some(format!("{} not checked yet", route.label())));
        Arc::new(Link {
            shared,
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
            once: AtomicBool::new(false),
            logged_in: AtomicBool::new(false),
            adopted: Mutex::new(None),
            swap: Notify::new(),
            carried: AtomicBool::new(false),
            hang: AtomicBool::new(false),
            closed_by_person: AtomicBool::new(false),
            invite: OnceLock::new(),
            invite_login: Mutex::new(InviteLogin::default()),
            pair_key: OnceLock::new(),
            signs_for: Mutex::new(None),
            dialer: Mutex::new(None),
        })
    }

    pub fn set_invite(&self, invite: crate::invite::Invite) {
        self.invite.get_or_init(|| invite);
    }

    /// An ad-hoc join's one session: a one-time password stops working when its connection closes, so the connection is
    /// never let go while held, a refused login is never retried, and the session's end is the bridge's. What happens is
    /// told on stdout, one line each, for the plugin that started it: `logged in …`, then `ended: …`; or `refused: …`
    /// (the PC's own words) or `failed: …` before any login.
    pub fn set_pair_key(&self, key_file: PathBuf) {
        self.pair_key.get_or_init(|| key_file);
    }

    pub fn set_once(&self) {
        self.once.store(true, Ordering::SeqCst);
    }

    fn ended_once(&self, ended: &ResultType<()>) -> ! {
        let said = match ended {
            Ok(()) => "the session closed".to_string(),
            Err(e) => e.to_string(),
        };
        let line = if self.logged_in.load(Ordering::SeqCst) {
            format!("ended: {said}")
        } else if let Some(words) = said.strip_prefix("login refused by the PC: ") {
            format!("refused: {words}")
        } else {
            format!("failed: {said}")
        };
        println!("{}", line.replace('\n', " "));
        std::process::exit(if line.starts_with("ended") { 0 } else { 3 });
    }

    fn wanted(&self) -> bool {
        if !self.may_dial() {
            return false;
        }
        self.once.load(Ordering::SeqCst) || self.viewers.load(Ordering::SeqCst) > 0 || now_ms().saturating_sub(self.last_use_ms.load(Ordering::SeqCst)) < IDLE.as_millis() as u64
    }

    /// With no password saved, only a connection the person at the PC allowed carries a session; once the person closed one, nothing
    /// does until Rig is let in again.
    fn may_dial(&self) -> bool {
        if self.closed_by_person.load(Ordering::SeqCst) {
            if !self.shared.claimed() {
                return false;
            }
            self.closed_by_person.store(false, Ordering::SeqCst);
        }
        !self.password.is_empty() || self.once.load(Ordering::SeqCst) || self.carried.load(Ordering::SeqCst) || self.adopted.lock().unwrap().is_some()
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            if !self.wanted() {
                self.probe().await;
                let _ = tokio::time::timeout(PROBE_EVERY, self.demand.notified()).await;
                continue;
            }
            let started = now_ms();
            let ended = self.session().await;
            let carried = self.carried.swap(false, Ordering::SeqCst);
            let by_person = matches!(&ended, Err(e) if e.downcast_ref::<PersonClosed>().is_some());
            if by_person {
                self.last_use_ms.store(0, Ordering::SeqCst);
                self.shared.revoke(PERSON_ENDED);
                self.closed_by_person.store(true, Ordering::SeqCst);
            } else if carried && !self.may_dial() {
                self.shared.revoke("the RustDesk session the person at the PC allowed ended; desk/request asks them again");
            }
            {
                let mut state = self.state.lock().unwrap();
                state.out = None;
                state.peer = None;
                state.info = None;
                if let (Err(e), false) = (&ended, by_person) {
                    state.failure = Some((Failure::of(e), now_ms()));
                    self.shared.set_down(Some(e.to_string()));
                }
            }
            let _ = self.to_viewers.send(ToViewer::Ended);
            for (_, waiter) in self.shots.lock().unwrap().drain() {
                let _ = waiter.send(Err("the RustDesk session to the PC closed".into()));
            }
            if self.once.load(Ordering::SeqCst) {
                self.ended_once(&ended);
            }
            match ended {
                Ok(()) => log::info!("closed the session to {} after it went idle", self.addr),
                Err(_) if by_person => log::info!("the person at {} closed the session; nothing dials it until Rig is let in again", self.addr),
                Err(e) => {
                    log::warn!("session to {} ended: {e}", self.addr);
                    let pause = if e.downcast_ref::<LoginRefused>().is_some() {
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

    /// What made a dial fail, in the OS's words where it has them.
    async fn dial_cause(&self, e: hbb_common::anyhow::Error) -> String {
        let Route::Direct { addr } = &self.route else { return e.to_string() };
        // hbb_common's FramedStream::new drops the OS error and says only "Failed to connect to <addr>": a second connect learns it.
        match tokio::time::timeout(PROBE_WAIT, tokio::net::TcpStream::connect(addr.as_str())).await {
            Ok(Err(refused)) => refused.to_string(),
            Err(_) => format!("no answer within {} s", PROBE_WAIT.as_secs()),
            Ok(Ok(_)) => e.to_string(),
        }
    }

    /// Whether the PC answers, found without logging in: its direct-IP port takes a connection, or the ID server has it.
    async fn probe(&self) {
        let down = match &self.route {
            Route::Direct { addr } => match tokio::time::timeout(PROBE_WAIT, tokio::net::TcpStream::connect(addr.as_str())).await {
                Ok(Ok(_)) => None,
                Ok(Err(e)) => Some(format!("nothing answers at {addr}: {e}")),
                Err(_) => Some(format!("nothing answers at {addr} within {} s", PROBE_WAIT.as_secs())),
            },
            Route::Id { id, server } => match crate::rendezvous::online(server, id).await {
                Ok(true) => None,
                Ok(false) => Some(format!("the PC {id} is offline (not registered with the ID server now)")),
                Err(e) => Some(format!("cannot ask the ID server {} about {id}: {e}", server.host)),
            },
        };
        self.shared.set_down(down);
    }

    async fn session(&self) -> ResultType<()> {
        // A hang-up that arrived as the last session ended on its own is not for this one.
        self.hang.store(false, Ordering::SeqCst);
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        // Under the lock hang_up takes, so a hang-up never finds the adopted connection gone and `carried` still the last session's.
        let adopted = {
            let mut slot = self.adopted.lock().unwrap();
            let adopted = slot.take();
            self.carried.store(adopted.is_some(), Ordering::SeqCst);
            adopted
        };
        let mut stream = match adopted {
            Some(allowed) => {
                self.on_message(allowed.peer_info, &tx)?;
                allowed.stream
            }
            None => match self.dial().await {
                Ok(stream) => stream,
                Err(e) => return Err(Unreached(self.dial_cause(e).await).into()),
            },
        };
        *self.dialer.lock().unwrap() = Some(stream.local_addr().ip());
        if self.invite.get().is_some() {
            *self.invite_login.lock().unwrap() = InviteLogin { dialer: Some(stream.local_addr().ip()), ..Default::default() };
        }
        let mut idle_check = tokio::time::interval(Duration::from_secs(5));
        // next_timeout restarts with every select! round, so the idle tick alone would keep a dead link looking alive.
        let mut heard = std::time::Instant::now();
        // Until the PC sends anything, a dropped connection is one that never reached it: no login was asked for.
        let mut answered = false;
        loop {
            tokio::select! {
                incoming = stream.next_timeout(READ_TIMEOUT_MS) => {
                    let bytes = match incoming {
                        Some(Ok(bytes)) => {
                            heard = std::time::Instant::now();
                            answered = true;
                            bytes
                        }
                        Some(Err(e)) if !answered => return Err(Unreached(format!("it closed the connection before asking for a login: {e}")).into()),
                        None if !answered => return Err(Unreached("it closed the connection before asking for a login".into()).into()),
                        Some(Err(e)) => bail!("connection error: {e}"),
                        None => bail!("the PC went silent or closed the connection"),
                    };
                    let Ok(msg) = Message::parse_from_bytes(&bytes) else { continue };
                    if let Some(reply) = self.on_message(msg, &tx)? {
                        stream.send(&reply).await?;
                    }
                }
                Some(out) = rx.recv() => stream.send(&out).await?,
                _ = self.swap.notified() => {
                    if self.adopted.lock().unwrap().is_some() || self.hang.swap(false, Ordering::SeqCst) {
                        return Ok(());
                    }
                }
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

    /// Nothing of an invite goes to a PC that has not shown it holds it: any PC can answer a dial under any ID, and could
    /// otherwise take the proof to the real device.
    fn check_invite_holder(&self, hash: &Hash) -> ResultType<()> {
        let Some(invite) = self.invite.get() else { return Ok(()) };
        let dialer = self.invite_login.lock().unwrap().dialer;
        if !dialer.is_some_and(|dialer| invite.held_by(&self.username, hash, dialer)) {
            return Err(LoginRefused(NOT_THE_INVITED_DEVICE.into()).into());
        }
        Ok(())
    }

    fn on_invite_accepted(&self, tag: &[u8]) {
        let Some(invite) = self.invite.get() else { return };
        let mut login = self.invite_login.lock().unwrap();
        if login.sent.as_ref().is_some_and(|(hash, signature)| invite.accepted(&self.username, hash, signature, tag)) {
            login.accepted = true;
        }
    }

    fn on_message(&self, msg: Message, tx: &mpsc::UnboundedSender<Message>) -> ResultType<Option<Message>> {
        if self.to_viewers.receiver_count() > 0 && self.relayed(&msg) {
            let _ = self.to_viewers.send(ToViewer::Message(Arc::new(msg.clone())));
        }
        match msg.union {
            Some(message::Union::Hash(hash)) => {
                self.check_invite_holder(&hash)?;
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
                let mut login = self.login(&hash, option, None);
                if let Some(invite) = self.invite.get() {
                    let request = login.mut_login_request();
                    request.password = Default::default();
                    let proof = invite.proof(&self.username, &hash);
                    self.invite_login.lock().unwrap().sent = Some((hash, proof.signature.to_vec()));
                    request.rig_invite = MessageField::some(proof);
                } else if let Some(knock) = self.password_signed(&hash) {
                    let request = login.mut_login_request();
                    request.my_name = format!("{} (Rig agent)", knock.agent_title);
                    request.rig_knock = MessageField::some(knock);
                }
                return Ok(Some(login));
            }
            Some(message::Union::RigInviteAccepted(tag)) => self.on_invite_accepted(&tag),
            Some(message::Union::LoginResponse(res)) => match res.union {
                Some(login_response::Union::Error(e)) => return Err(LoginRefused(e).into()),
                Some(login_response::Union::PeerInfo(pi)) => {
                    if self.invite.get().is_some() && !self.invite_login.lock().unwrap().accepted {
                        return Err(LoginRefused("it did not confirm the invite".into()).into());
                    }
                    log::info!("logged in to {} ({} {}, RustDesk {})", self.addr, pi.hostname, pi.platform, pi.version);
                    let mut state = self.state.lock().unwrap();
                    state.platform = Some(pi.platform.clone());
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
                    self.shared.set_down(None);
                    if let Some(peer) = state.peer.as_ref().filter(|_| self.once.load(Ordering::SeqCst) && !self.logged_in.swap(true, Ordering::SeqCst)) {
                        println!("logged in {} ({}, RustDesk {})", peer.hostname.replace('\n', " "), peer.platform.replace('\n', " "), peer.version.replace('\n', " "));
                    }
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
                Some(misc::Union::CloseReason(reason)) if reason == crate::knock::CLOSED_BY_PERSON => return Err(PersonClosed.into()),
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
    /// Rig's signature for a password login to a device that asks for it: for the agent Rig's Accept logs in for, else the
    /// agent holding the session, which a redial carries on with nobody asked. None with no pairing key or no such agent,
    /// and the login goes unnamed.
    fn password_signed(&self, hash: &Hash) -> Option<RigKnock> {
        let key_file = self.pair_key.get().filter(|_| !hash.rig_knock_nonce.is_empty())?;
        let dialer = (*self.dialer.lock().unwrap())?;
        let accepted = self.signs_for.lock().unwrap().clone();
        let (owner, ask) = accepted.or_else(|| self.shared.standing().owner.map(|owner| (owner, String::new())))?;
        match crate::invite::read_key(key_file) {
            Ok((_, key)) => Some(crate::knock::signed(&key, &owner.agent_id, &owner.title, &ask, &self.username, hash, dialer)),
            Err(e) => {
                log::warn!("the password login goes without Rig's signature: {e}");
                None
            }
        }
    }

    fn relayed(&self, msg: &Message) -> bool {
        match &msg.union {
            Some(message::Union::Hash(_) | message::Union::LoginResponse(_) | message::Union::TestDelay(_)) => false,
            Some(message::Union::SignedId(_) | message::Union::PublicKey(_)) => false,
            Some(message::Union::RigInviteAccepted(_)) => false,
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

    pub fn username(&self) -> &str {
        &self.username
    }

    /// The connection the person at the PC allowed carries the session from now: any session running is closed for it.
    pub fn adopt(&self, allowed: crate::knock::Allowed) {
        self.closed_by_person.store(false, Ordering::SeqCst);
        *self.adopted.lock().unwrap() = Some(allowed);
        self.last_use_ms.store(now_ms(), Ordering::SeqCst);
        self.swap.notify_one();
        self.demand.notify_one();
    }

    /// Hangs up the connection adopted last, whether it waits or carries the session now; nothing else is touched.
    pub fn hang_up(&self) {
        self.last_use_ms.store(0, Ordering::SeqCst);
        let mut adopted = self.adopted.lock().unwrap();
        if adopted.take().is_none() && self.carried.load(Ordering::SeqCst) {
            self.hang.store(true, Ordering::SeqCst);
            self.swap.notify_one();
        }
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

    /// A login that has actually succeeded, on the connection the person allowed or with the saved password, made now
    /// unless a session is up: Rig asks for it on the operator's Accept. A password the PC refuses is tried this once:
    /// the link does not dial it again until something uses the PC.
    pub fn log_in(&self, wait: Duration, signs_for: Option<(Owner, String)>) -> Result<(), NoLogin> {
        self.closed_by_person.store(false, Ordering::SeqCst);
        if !self.may_dial() {
            return Err(NoLogin::NoPassword);
        }
        let asked = now_ms();
        *self.signs_for.lock().unwrap() = signs_for;
        let made = self.ensure(wait);
        *self.signs_for.lock().unwrap() = None;
        made.map(|_| ()).map_err(|why| {
            self.last_use_ms.store(0, Ordering::SeqCst);
            let state = self.state.lock().unwrap();
            match state.failure.as_ref().filter(|(_, at)| *at >= asked).map(|(failure, _)| failure) {
                Some(Failure::Unreached(cause)) => self.unreached(cause, state.platform.as_deref()),
                Some(refused @ Failure::Refused(_)) => NoLogin::Refused(refused.to_string()),
                _ => NoLogin::Failed(why),
            }
        })
    }

    /// Rig's Accept found nothing at the PC to log in to: what failed where, and what to start, for the device the PC last said it is.
    fn unreached(&self, cause: &str, platform: Option<&str>) -> NoLogin {
        const UNTRIED: &str = "No login was tried, and no password was sent.";
        let (at, why) = match (&self.route, platform) {
            (Route::Direct { addr }, Some("Android")) => {
                (addr.clone(), format!("RigDesk on the phone isn't accepting connections ({cause}). {UNTRIED} Start the service in RigDesk on the phone."))
            }
            (Route::Direct { addr }, Some(_)) => (addr.clone(), format!("RustDesk on the PC isn't accepting connections ({cause}). {UNTRIED} \
                                                                          Start the RustDesk service on the PC, with direct IP access on.")),
            (Route::Direct { addr }, None) => (addr.clone(), format!("Nothing there is accepting connections ({cause}). {UNTRIED} \
                                                                       Start the service in RigDesk on a phone, or the RustDesk service on a PC, with direct IP access on.")),
            (Route::Id { id, .. }, _) => (format!("ID {id}"), format!("The ID server didn't put Rig through ({cause}). {UNTRIED} \
                                                                        Start the RustDesk service on the PC, and check that it's online.")),
        };
        NoLogin::Unreached { at, why }
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
            if self.closed_by_person.load(Ordering::SeqCst) && !self.may_dial() {
                return Err(format!("no RustDesk session to the PC at {}: the person there closed the last one, so nothing dials it until an agent \
                                    joins again", self.addr));
            }
            if !self.may_dial() {
                return Err(format!("no RustDesk session to the PC at {}: Rig keeps no password for it, so only the person at the PC lets an agent in, \
                                    by Allow on its prompt for desk/request", self.addr));
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
    use base::invite;
    use rigdesk_agent::EventKind;

    #[test]
    fn with_no_password_kept_an_agent_is_told_only_the_persons_allow_lets_it_in() {
        let shared = Arc::new(rigdesk_agent::Shared::new(rigdesk_agent::Events::new(8)));
        let link = Link::new(Route::direct("100.64.0.9"), String::new(), "Rig agent".into(), shared);
        assert!(!link.wanted(), "nothing dials a PC with no password");
        let Err(refused) = link.ensure(Duration::from_secs(5)) else { panic!("a session with no password and no Allow") };
        assert!(refused.contains("only the person at the PC lets an agent in"), "{refused}");
    }

    /// A stock PC on loopback that greets each dial and answers its login with `answer`, counting the logins it gets.
    async fn answering_pc(answer: login_response::Union) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let logins = Arc::new(AtomicUsize::new(0));
        let counted = logins.clone();
        tokio::spawn(async move {
            while let Ok((socket, dialer)) = listener.accept().await {
                let mut pc = FramedStream::from(socket, dialer);
                pc.send(&message_of(Hash { salt: "s".into(), challenge: "c".into(), ..Default::default() })).await.unwrap();
                while let Some(Ok(bytes)) = pc.next_timeout(5_000).await {
                    if Message::parse_from_bytes(&bytes).ok().filter(Message::has_login_request).is_some() {
                        counted.fetch_add(1, Ordering::SeqCst);
                        let mut msg = Message::new();
                        msg.set_login_response(base::message_proto::LoginResponse { union: Some(answer.clone()), ..Default::default() });
                        pc.send(&msg).await.unwrap();
                    }
                }
            }
        });
        (addr, logins)
    }

    async fn logged_in_with(addr: &str, password: &str) -> (Arc<Link>, Result<(), NoLogin>) {
        logged_in_as(addr, password, None).await
    }

    /// A login from a bridge that saw the PC say `platform` at an earlier login.
    async fn logged_in_as(addr: &str, password: &str, platform: Option<&str>) -> (Arc<Link>, Result<(), NoLogin>) {
        let shared = Arc::new(rigdesk_agent::Shared::new(rigdesk_agent::Events::new(8)));
        let link = Link::new(Route::direct(addr), password.into(), "Rig agent".into(), shared);
        link.state.lock().unwrap().platform = platform.map(str::to_string);
        tokio::spawn(link.clone().run());
        let trying = link.clone();
        let said = tokio::task::spawn_blocking(move || trying.log_in(Duration::from_secs(10), None)).await.unwrap();
        (link, said)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rigs_accept_logs_in_once_with_the_saved_password_and_says_what_the_pc_answered() {
        let (_, none) = logged_in_with("127.0.0.1:9", "").await;
        assert_eq!(none, Err(NoLogin::NoPassword), "no password and no Allow: nothing is dialed");

        let (addr, logins) = answering_pc(login_response::Union::Error("Wrong Password".into())).await;
        let (link, refused) = logged_in_with(&addr, "stale").await;
        let Err(NoLogin::Refused(why)) = refused else { panic!("a refused password is not told as refused: {refused:?}") };
        assert_eq!(why, "login refused by the PC: Wrong Password");
        assert!(!link.wanted(), "a refused password is not dialed again on its own");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(logins.load(Ordering::SeqCst), 1, "the saved password was tried more than once");

        let mut pi = PeerInfo::new();
        pi.hostname = "front".into();
        let (addr, _) = answering_pc(login_response::Union::PeerInfo(pi)).await;
        let (link, ok) = logged_in_with(&addr, "good").await;
        assert_eq!(ok, Ok(()));
        assert_eq!(link.state.lock().unwrap().peer.as_ref().map(|peer| peer.hostname.as_str()), Some("front"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rigs_accept_at_a_pc_where_nothing_takes_the_connection_says_no_password_was_tried() {
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = closed.local_addr().unwrap().to_string();
        drop(closed);
        let untried = "No login was tried, and no password was sent.";
        let told = |said: Result<(), NoLogin>| match said {
            Err(NoLogin::Unreached { at, why }) => (at, why),
            other => panic!("a closed port is not told as unreached: {other:?}"),
        };
        let (at, why) = told(logged_in_as(&addr, "saved", Some("Android")).await.1);
        assert_eq!(at, addr);
        let cause = why.strip_prefix("RigDesk on the phone isn't accepting connections (").and_then(|rest| rest.split_once("). ")).map(|(cause, _)| cause);
        let cause = cause.unwrap_or_else(|| panic!("{why}"));
        assert!(!cause.contains(&addr) && !cause.is_empty(), "the cause repeats the address or is empty: {why}");
        assert_eq!(why, format!("RigDesk on the phone isn't accepting connections ({cause}). {untried} Start the service in RigDesk on the phone."));
        let (_, why) = told(logged_in_as(&addr, "saved", Some("Windows")).await.1);
        assert_eq!(why, format!("RustDesk on the PC isn't accepting connections ({cause}). {untried} Start the RustDesk service on the PC, with direct IP access on."));
        let (_, why) = told(logged_in_with(&addr, "saved").await.1);
        assert!(why.starts_with("Nothing there is accepting connections (") && why.contains(untried), "{why}");

        let hanging_up = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = hanging_up.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((socket, _)) = hanging_up.accept().await {
                drop(socket);
            }
        });
        let (_, why) = told(logged_in_as(&addr, "saved", Some("Windows")).await.1);
        assert!(why.starts_with("RustDesk on the PC isn't accepting connections (it closed the connection before asking for a login") && why.contains(untried), "{why}");
    }

    /// A stock PC on loopback that logs in every dial and, on the first session only, then closes it as its person does.
    async fn pc_whose_person_closes_the_first_session() -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let logins = Arc::new(AtomicUsize::new(0));
        let counted = logins.clone();
        tokio::spawn(async move {
            while let Ok((socket, dialer)) = listener.accept().await {
                let counted = counted.clone();
                tokio::spawn(async move {
                    let mut pc = FramedStream::from(socket, dialer);
                    if pc.send(&message_of(Hash { salt: "s".into(), challenge: "c".into(), ..Default::default() })).await.is_err() {
                        return;
                    }
                    while let Some(Ok(bytes)) = pc.next_timeout(5_000).await {
                        if Message::parse_from_bytes(&bytes).ok().filter(Message::has_login_request).is_none() {
                            continue;
                        }
                        let first = counted.fetch_add(1, Ordering::SeqCst) == 0;
                        pc.send(&logged_in()).await.unwrap();
                        if first {
                            let mut close = Message::new();
                            close.set_misc(Misc { union: Some(misc::Union::CloseReason(crate::knock::CLOSED_BY_PERSON.into())), ..Default::default() });
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            pc.send(&close).await.unwrap();
                            return;
                        }
                    }
                });
            }
        });
        (addr, logins)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_persons_close_ends_the_agents_claim_and_nothing_dials_the_pc_again_until_a_new_join() {
        let (addr, logins) = pc_whose_person_closes_the_first_session().await;
        let shared = Arc::new(rigdesk_agent::Shared::new(rigdesk_agent::Events::new(64)));
        let link = Link::new(Route::direct(&addr), "saved".into(), "Rig agent".into(), shared.clone());
        tokio::spawn(link.clone().run());
        let grant = shared.mint();
        let owner = rigdesk_agent::session::Owner { agent_id: "a1".into(), title: "Builder".into(), graph: "g".into() };
        shared.claim(&grant.grant, &grant.secret, owner).unwrap();
        let trying = link.clone();
        assert_eq!(tokio::task::spawn_blocking(move || trying.log_in(Duration::from_secs(10), None)).await.unwrap(), Ok(()));

        let revoked = || {
            shared.events.since(0).into_iter().filter_map(|e| match e.kind {
                EventKind::Revoked { agent_id, reason } => Some((agent_id, reason)),
                _ => None,
            }).collect::<Vec<_>>()
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while revoked().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(revoked(), vec![("a1".to_string(), PERSON_ENDED.to_string())], "the agent's claim ends, saying the person ended it");
        assert!(!shared.claimed());
        assert!(link.shared.down().is_none(), "a person's close is not the PC being down");

        let _viewer = link.attach();
        let using = link.clone();
        let Err(refused) = tokio::task::spawn_blocking(move || using.ensure(Duration::from_secs(2))).await.unwrap() else { panic!("a tool reached the PC after the person closed it") };
        assert!(refused.contains("the person there closed the last one"), "{refused}");
        tokio::time::sleep(Duration::from_secs(7)).await;
        assert_eq!(logins.load(Ordering::SeqCst), 1, "the saved password was dialed again after the person closed the session");
        assert!(!link.wanted(), "a viewer or a tool call does not reopen it");

        let joining = link.clone();
        assert_eq!(tokio::task::spawn_blocking(move || joining.log_in(Duration::from_secs(10), None)).await.unwrap(), Ok(()), "a new join logs in again");
        assert_eq!(logins.load(Ordering::SeqCst), 2);
    }

    /// The logins a phone on loopback gets with the saved password, greeting each with a nonce that asks for Rig's proof.
    async fn signing_phone() -> (String, Arc<Mutex<Vec<(LoginRequest, IpAddr)>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let logins = Arc::new(Mutex::new(Vec::new()));
        let kept = logins.clone();
        tokio::spawn(async move {
            while let Ok((socket, dialer)) = listener.accept().await {
                let mut phone = FramedStream::from(socket, dialer);
                phone.send(&message_of(Hash { salt: "s".into(), challenge: "c".into(), rig_knock_nonce: b"nonce".to_vec().into(), ..Default::default() })).await.unwrap();
                while let Some(Ok(bytes)) = phone.next_timeout(5_000).await {
                    if let Some(login) = Message::parse_from_bytes(&bytes).ok().filter(Message::has_login_request) {
                        kept.lock().unwrap().push((login.login_request().clone(), dialer.ip()));
                        let mut msg = Message::new();
                        msg.set_login_response(base::message_proto::LoginResponse { union: Some(login_response::Union::PeerInfo(PeerInfo::new())), ..Default::default() });
                        phone.send(&msg).await.unwrap();
                    }
                }
            }
        });
        (addr, logins)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_password_login_names_its_agent_and_the_operators_ask_to_a_phone_that_asks_and_only_with_rigs_key() {
        use hbb_common::base64::{engine::general_purpose::STANDARD, Engine};
        use hbb_common::sodiumoxide::crypto::sign;
        let dir = std::env::temp_dir().join(format!("rigdesk-password-signed-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let key_file = dir.join("pair.key");
        let public = sign::PublicKey::from_slice(&STANDARD.decode(crate::invite::pair_key(&key_file).unwrap()).unwrap()).unwrap();
        let owner = Owner { agent_id: "agent_a".into(), title: "Builder".into(), graph: "g".into() };

        let (addr, logins) = signing_phone().await;
        let unkeyed = Link::new(Route::direct(&addr), "good".into(), "Rig agent".into(), Arc::new(rigdesk_agent::Shared::new(rigdesk_agent::Events::new(8))));
        tokio::spawn(unkeyed.clone().run());
        let trying = unkeyed.clone();
        let accepted = Some((owner.clone(), "desk_1".to_string()));
        assert_eq!(tokio::task::spawn_blocking(move || trying.log_in(Duration::from_secs(10), accepted)).await.unwrap(), Ok(()));
        assert!(logins.lock().unwrap()[0].0.rig_knock.is_none(), "no pairing key: the login goes unnamed");

        let (addr, logins) = signing_phone().await;
        let shared = Arc::new(rigdesk_agent::Shared::new(rigdesk_agent::Events::new(8)));
        let link = Link::new(Route::direct(&addr), "good".into(), "Rig agent".into(), shared.clone());
        link.set_pair_key(key_file.clone());
        tokio::spawn(link.clone().run());
        let trying = link.clone();
        let accepted = Some((owner.clone(), "desk_1".to_string()));
        assert_eq!(tokio::task::spawn_blocking(move || trying.log_in(Duration::from_secs(10), accepted)).await.unwrap(), Ok(()));
        let (login, dialer) = logins.lock().unwrap()[0].clone();
        assert!(!login.password.is_empty(), "the password still logs in");
        assert_eq!(login.my_name, "Builder (Rig agent)");
        let knock = login.rig_knock.clone().into_option().expect("Rig's Accept names its agent");
        assert_eq!((knock.agent_id.as_str(), knock.ask.as_str()), ("agent_a", "desk_1"));
        let signature = sign::Signature::from_bytes(&knock.signature).unwrap();
        let signed = base::rig_knock::Login { device: &login.username, salt: "s", challenge: "c", nonce: b"nonce", dialer };
        assert!(sign::verify_detached(&signature, &signed.message("agent_a", "Builder", "desk_1"), &public));
        assert!(link.signs_for.lock().unwrap().is_none(), "the Accept's agent is not carried into a later login");

        let (addr, logins) = signing_phone().await;
        let link = Link::new(Route::direct(&addr), "good".into(), "Rig agent".into(), shared.clone());
        link.set_pair_key(key_file.clone());
        tokio::spawn(link.clone().run());
        let trying = link.clone();
        assert_eq!(tokio::task::spawn_blocking(move || trying.log_in(Duration::from_secs(10), None)).await.unwrap(), Ok(()));
        assert!(logins.lock().unwrap()[0].0.rig_knock.is_none(), "no agent to name: the login goes unnamed");

        let minted = shared.mint();
        assert!(shared.claim(&minted.grant, &minted.secret, owner).is_ok());
        let (addr, logins) = signing_phone().await;
        let redial = Link::new(Route::direct(&addr), "good".into(), "Rig agent".into(), shared);
        redial.set_pair_key(key_file);
        tokio::spawn(redial.clone().run());
        let trying = redial.clone();
        assert!(tokio::task::spawn_blocking(move || trying.ensure(Duration::from_secs(10))).await.unwrap().is_ok());
        let knock = logins.lock().unwrap()[0].0.rig_knock.clone().into_option().expect("a redial names the agent holding the session");
        assert_eq!((knock.agent_id.as_str(), knock.ask.as_str()), ("agent_a", ""), "with nobody asked");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_hang_up_left_by_a_session_that_ended_alone_does_not_end_the_next_one() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(tokio::net::TcpStream::connect(addr), listener.accept());
        let _peer = accepted.unwrap();
        let shared = Arc::new(rigdesk_agent::Shared::new(rigdesk_agent::Events::new(8)));
        let link = Link::new(Route::direct("100.64.0.9"), String::new(), "Rig agent".into(), shared);
        link.carried.store(true, Ordering::SeqCst);
        link.hang_up();
        link.carried.store(false, Ordering::SeqCst);

        link.adopt(crate::knock::Allowed { stream: FramedStream::from(client.unwrap(), addr), peer_info: Message::new() });
        assert!(tokio::time::timeout(Duration::from_millis(300), link.session()).await.is_err(), "the new session ended at once");
    }

    const DEVICE: &str = "105517719";
    const SECRET: &str = "the invite's secret";

    fn invited_link(peer: &str, name: &str) -> Arc<Link> {
        let dir = std::env::temp_dir().join(format!("rigdesk-link-invite-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let (key, file) = (dir.join("pair.key"), dir.join("invite.json"));
        crate::invite::pair_key(&key).unwrap();
        let claim = hbb_common::serde_json::json!({"grant": "g1", "secret": SECRET, "agent_id": "agent_a", "agent_title": "Builder"});
        std::fs::write(&file, claim.to_string()).unwrap();
        let invite = crate::invite::Invite::load(&file, &key).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        let shared = Arc::new(rigdesk_agent::Shared::new(rigdesk_agent::Events::new(8)));
        let link = Link::new((Route::direct(peer).0, DEVICE.to_string()), String::new(), "Builder (Rig agent)".into(), shared);
        link.set_once();
        link.set_invite(invite);
        link
    }

    fn device_greeting(secret: &str, dialer: std::net::IpAddr) -> Hash {
        let (salt, challenge) = ("salt", "challenge");
        let said = invite::Greeting { device_id: DEVICE, salt, challenge };
        let rig_invite_device = invite::device_tag(&invite::secret_hash(secret), &said, dialer).into();
        Hash { salt: salt.into(), challenge: challenge.into(), rig_invite_device, ..Default::default() }
    }

    fn message_of(hash: Hash) -> Message {
        let mut msg = Message::new();
        msg.set_hash(hash);
        msg
    }

    fn logged_in() -> Message {
        let mut msg = Message::new();
        msg.mut_login_response().set_peer_info(PeerInfo { hostname: "phone".into(), ..Default::default() });
        msg
    }

    #[test]
    fn a_pc_that_cannot_show_it_holds_the_invite_gets_nothing_of_it() {
        let rig: std::net::IpAddr = "100.64.0.1".parse().unwrap();
        let other_pc: std::net::IpAddr = "100.64.0.2".parse().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let link = invited_link("100.64.0.9", "refused");
        link.invite_login.lock().unwrap().dialer = Some(rig);

        let bare = Hash { salt: "salt".into(), challenge: "challenge".into(), ..Default::default() };
        let greetings = [bare, device_greeting("another invite", rig), device_greeting(SECRET, other_pc)];
        for greeting in greetings {
            let refused = link.on_message(message_of(greeting), &tx).err().expect("the proof went to a PC that showed nothing");
            assert_eq!(refused.to_string(), "login refused by the PC: Offline");
        }
        assert!(link.invite_login.lock().unwrap().sent.is_none());
    }

    #[test]
    fn a_login_counts_only_once_the_device_confirms_the_invite() {
        let rig: std::net::IpAddr = "100.64.0.1".parse().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let link = invited_link("100.64.0.9", "confirm");
        link.invite_login.lock().unwrap().dialer = Some(rig);

        let reply = link.on_message(message_of(device_greeting(SECRET, rig)), &tx).unwrap().expect("the device gets the login");
        let proof = reply.login_request().rig_invite.clone().into_option().expect("the login presents the invite");
        assert_eq!(proof.agent_id, "agent_a");

        let refused = link.on_message(logged_in(), &tx).err().expect("a login no device confirmed counted");
        assert_eq!(refused.to_string(), "login refused by the PC: it did not confirm the invite");
        assert!(link.state.lock().unwrap().peer.is_none());

        let said = invite::Greeting { device_id: DEVICE, salt: "salt", challenge: "challenge" };
        let mut forged = Message::new();
        forged.set_rig_invite_accepted(invite::accepted_tag(&invite::secret_hash("another invite"), &said, &proof.signature).into());
        link.on_message(forged, &tx).unwrap();
        assert!(link.on_message(logged_in(), &tx).is_err(), "a confirmation from another invite counted");

        let mut accepted = Message::new();
        accepted.set_rig_invite_accepted(invite::accepted_tag(&invite::secret_hash(SECRET), &said, &proof.signature).into());
        link.on_message(accepted, &tx).unwrap();
        link.on_message(logged_in(), &tx).unwrap();
        assert_eq!(link.state.lock().unwrap().peer.as_ref().map(|peer| peer.hostname.as_str()), Some("phone"));
    }

    #[tokio::test]
    async fn the_address_rig_dials_from_is_the_one_the_device_sees() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let link = invited_link(&addr.to_string(), "dial");
        let session = tokio::spawn({
            let link = link.clone();
            async move { link.session().await }
        });
        let (socket, dialer) = listener.accept().await.unwrap();
        let mut device = FramedStream::from(socket, dialer);
        device.send(&message_of(device_greeting(SECRET, dialer.ip()))).await.unwrap();
        let mut login = None;
        while login.is_none() {
            let bytes = device.next_timeout(5_000).await.expect("the bridge hung up").unwrap();
            login = Message::parse_from_bytes(&bytes).ok().filter(Message::has_login_request);
        }
        assert!(login.unwrap().login_request().rig_invite.is_some());
        session.abort();
    }

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
