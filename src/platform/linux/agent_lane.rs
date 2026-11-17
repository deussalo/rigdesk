//! The management lane on Linux (see `rigdesk_agent::manage` and `rigdesk_agent::systemd`), served by the root
//! `--service`. The agent API every other route lives on runs in the desktop user's `--server`, whose files that user's
//! processes can read and write; the lane is root's, so it keeps everything apart from it:
//!
//!   /var/lib/rigdesk/enroll.json       the installer's `lane_token` and `agent_shell`, applied at start and deleted
//!   /var/lib/rigdesk/agent/            root's alone (0700): `lane.token`, the switch `agent-shell`, the journal
//!                                      `actions.jsonl`, the jobs under `shell/`, and the health samples
//!                                      `health.jsonl`, taken every 10 minutes in `health/` (see `rigdesk_agent::health`)
//!   /run/rigdesk/lane.sock             the lane, connectable by root alone; `tailscale serve --set-path=/lane` puts it on
//!                                      the tailnet, since tailscaled is root
//!   /var/log/rigdesk/managed-by-rig.txt  what ran, which every user may read and the tray opens
//!
//! The person's pause is the `agent-shell-paused` option, which the PIN-locked settings set and the desktop user's
//! process syncs to root's config. The human's blocks live in the desktop user's server, which the lane asks.

use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hbb_common::{config::Config, log, tokio};
use rigdesk_agent::{
    digest,
    health::{Collector, Samples, INTERVAL},
    manage::Lane,
    systemd::{self, SystemdRunner},
    Api, Events, Journal, Shared,
};
use serde_derive::Deserialize;

pub const ROOT_DIR: &str = "/var/lib/rigdesk";
const SOCKET_DIR: &str = "/run/rigdesk";
const DIGEST_DIR: &str = "/var/log/rigdesk";
const JOURNAL_BYTES: u64 = 8 << 20;
const HEALTH_SCRIPT: &str = include_str!("agent_health.sh");

pub fn agent_dir() -> PathBuf {
    Path::new(ROOT_DIR).join("agent")
}

/// The switch file: `on` or `off`, as the installer left it.
pub fn switch_file() -> PathBuf {
    agent_dir().join("agent-shell")
}

/// Serves the lane for as long as the service runs; returns at once where it cannot. Only in the root `--service`.
pub fn serve() {
    if !super::is_root() {
        log::info!("agent shell: this process is not root, so it offers no management lane");
        return;
    }
    if let Err(why) = private_dir(Path::new(ROOT_DIR)).and_then(|_| private_dir(&agent_dir())) {
        log::error!("agent shell: {why}; no management lane");
        return;
    }
    apply_enrollment();
    let token = match std::fs::read_to_string(agent_dir().join("lane.token")).map(|t| t.trim().to_string()) {
        Ok(token) if !token.is_empty() => token,
        _ => {
            log::info!("agent shell: no lane.token; the management lane is off on this PC");
            return;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().thread_name("agent-lane").build() {
        Ok(runtime) => runtime,
        Err(e) => {
            log::error!("agent shell: no runtime for the lane: {e}");
            return;
        }
    };
    runtime.block_on(async move {
        let runner = Arc::new(SystemdRunner {
            switch: switch_file(),
            paused: Box::new(|| Config::get_option(base::config::keys::OPTION_AGENT_SHELL_PAUSED) == "Y"),
            user: Box::new(systemd::signed_in),
        });
        let shared = Arc::new(Shared::new(Events::new(512)));
        let journal = Arc::new(Journal::open(agent_dir().join("actions.jsonl"), JOURNAL_BYTES));
        let lane = Lane::new(runner, agent_dir().join("shell"), shared.clone(), journal.clone());
        lane.blocks_from(blocked_in_session);
        let collector = Arc::new(Collector {
            lane: lane.clone(),
            shared: shared.clone(),
            samples: Arc::new(Samples::open(Some(agent_dir().join("health.jsonl")))),
            script: HEALTH_SCRIPT,
            dir: agent_dir().join("health"),
            rigdesk: crate::VERSION.to_string(),
            interval: INTERVAL,
        });
        let on = lane.switch().get("open").and_then(|v| v.as_bool()).unwrap_or(false);
        keep_digest(on, &shared, &journal);
        if let Err(why) = private_dir(Path::new(SOCKET_DIR)) {
            log::error!("agent shell: {why}; no management lane");
            return;
        }
        let socket = Path::new(SOCKET_DIR).join("lane.sock");
        match Api::lane_only(lane, Some(collector), shared, journal, &token).serve_unix(&socket).await {
            Ok(()) => log::info!("agent shell: management lane on {}; the agent shell is {}", socket.display(), if on { "open" } else { "closed" }),
            Err(e) => {
                log::error!("agent shell: cannot serve {}: {e}", socket.display());
                return;
            }
        }
        std::future::pending::<()>().await;
    });
}

/// The tray's list of what ran.
pub fn digest_file() -> PathBuf {
    Path::new(DIGEST_DIR).join("managed-by-rig.txt")
}

/// Opens the tray's list for the person at the PC; false when the agent shell is off here.
pub fn open_digest() -> bool {
    let path = digest_file();
    path.exists() && std::process::Command::new("xdg-open").arg(path).spawn().is_ok()
}

/// While the agent shell is on, keeps the tray's list; while it is off, there is none, and the tray offers none.
fn keep_digest(on: bool, shared: &Arc<Shared>, journal: &Arc<Journal>) {
    if !on {
        let _ = std::fs::remove_file(digest_file());
        return;
    }
    write_digest(journal);
    let journal = journal.clone();
    tokio::spawn(digest::keep(shared.clone(), move || write_digest(&journal)));
}

fn write_digest(journal: &Journal) {
    let written = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(DIGEST_DIR)
        .and_then(|_| write_private(&digest_file(), &digest::text(journal, "root", "\n"), 0o644));
    if let Err(e) = written {
        log::warn!("agent shell: the tray's list of commands is not written: {e}");
    }
}

/// When the desktop user's server says the human blocked `agent`: it keeps the blocks, as the PC's share state. None
/// when nobody is signed in or their server does not answer, as after its restart, when it has no blocks either. Root
/// reads the user's own token for that server, and only a file the user owns and no link to one, so a link planted
/// there cannot make root read and send another file.
fn blocked_in_session(agent: &str) -> Option<u64> {
    if agent.is_empty() || !agent.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return None;
    }
    let user = systemd::signed_in().ok()?;
    let path = Path::new(&user.home).join(".config").join(crate::get_app_name().to_lowercase()).join("agent").join("api.token");
    let mut file = std::fs::OpenOptions::new().read(true).custom_flags(hbb_common::libc::O_NOFOLLOW | hbb_common::libc::O_NONBLOCK).open(&path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.uid() != user.uid {
        hbb_common::throttled_log!(std::time::Duration::from_secs(60), warn, "agent shell: {} is not {}'s own file; blocks are not read", path.display(), user.name);
        return None;
    }
    let mut token = String::new();
    (&mut file).take(256).read_to_string(&mut token).ok()?;
    let token = token.trim();
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return None;
    }
    let timeout = std::time::Duration::from_secs(2);
    let mut stream = std::net::TcpStream::connect_timeout(&(std::net::Ipv4Addr::LOCALHOST, crate::agent::API_PORT).into(), timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    write!(stream, "GET /blocked?agent={agent} HTTP/1.0\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n\r\n").ok()?;
    let mut said = Vec::new();
    stream.take(64 << 10).read_to_end(&mut said).ok()?;
    let said = String::from_utf8_lossy(&said);
    let (head, body) = said.split_once("\r\n\r\n")?;
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(body).ok()?.get("since_ms")?.as_u64()
}

/// Made if missing, then root's alone.
fn private_dir(dir: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("cannot make {} root's alone: {e}", dir.display()))
}

#[derive(Deserialize)]
struct RootEnrollment {
    #[serde(default)]
    lane_token: String,
    /// `on` or `off`: whether agents may run commands on this PC without a session.
    #[serde(default)]
    agent_shell: String,
}

/// Applies the installer's root enrollment once, and deletes it. Nothing here logs a secret.
fn apply_enrollment() {
    let path = Path::new(ROOT_DIR).join("enroll.json");
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    if let Err(e) = std::fs::remove_file(&path) {
        log::error!("enroll: could not delete {}: {e}", path.display());
    }
    let enrollment = match serde_json::from_str::<RootEnrollment>(&text) {
        Ok(e) => e,
        Err(e) => {
            log::error!("enroll: {} is not valid enrollment JSON ({e}); nothing applied", path.display());
            return;
        }
    };
    let token = enrollment.lane_token.trim();
    if !token.is_empty() {
        let valid = (32..=128).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !valid {
            log::error!("enroll: lane_token is 32-128 of A-Za-z0-9_-; the lane token is left as it was");
        } else {
            match write_private(&agent_dir().join("lane.token"), token, 0o600) {
                Ok(()) => log::info!("enroll: lane token set"),
                Err(e) => log::error!("enroll: could not write the lane token: {e}"),
            }
        }
    }
    match enrollment.agent_shell.as_str() {
        "" => {}
        on @ ("on" | "off") => match write_private(&switch_file(), on, 0o644) {
            Ok(()) => log::info!("enroll: agent shell {on}"),
            Err(e) => log::error!("enroll: could not keep the agent shell switch: {e}"),
        },
        other => log::error!("enroll: agent_shell is on or off, not {other}; the switch is left as it was"),
    }
}

fn write_private(path: &Path, text: &str, mode: u32) -> std::io::Result<()> {
    let tmp = path.with_extension("new");
    std::fs::write(&tmp, text)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)
}
