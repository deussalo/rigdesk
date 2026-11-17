//! The management lane on Linux (see `rigdesk_agent::manage` and `rigdesk_agent::systemd`), served by the root
//! `--service`. The agent API every other route lives on runs in the desktop user's `--server`, whose files that user's
//! processes can read and write; the lane is root's, so it keeps everything apart from it:
//!
//!   /var/lib/rigdesk/enroll.json       the installer's `lane_token` and `agent_shell`, applied at start and deleted
//!   /var/lib/rigdesk/agent/            root's alone (0700): `lane.token`, the switch `agent-shell`, the journal
//!                                      `actions.jsonl` and the jobs under `shell/`
//!   /run/rigdesk/lane.sock             the lane, connectable by root alone; `tailscale serve --set-path=/lane` puts it on
//!                                      the tailnet, since tailscaled is root
//!
//! The person's pause is the `agent-shell-paused` option, which the PIN-locked settings set and the desktop user's
//! process syncs to root's config.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hbb_common::{config::Config, log, tokio};
use rigdesk_agent::{manage::Lane, systemd::SystemdRunner, Api, Events, Journal, Shared};
use serde_derive::Deserialize;

pub const ROOT_DIR: &str = "/var/lib/rigdesk";
const SOCKET_DIR: &str = "/run/rigdesk";
const JOURNAL_BYTES: u64 = 8 << 20;

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
            user: Box::new(rigdesk_agent::systemd::signed_in),
        });
        let shared = Arc::new(Shared::new(Events::new(512)));
        let journal = Arc::new(Journal::open(agent_dir().join("actions.jsonl"), JOURNAL_BYTES));
        let lane = Lane::new(runner, agent_dir().join("shell"), shared.clone(), journal.clone());
        let on = lane.switch().get("open").and_then(|v| v.as_bool()).unwrap_or(false);
        if let Err(why) = private_dir(Path::new(SOCKET_DIR)) {
            log::error!("agent shell: {why}; no management lane");
            return;
        }
        let socket = Path::new(SOCKET_DIR).join("lane.sock");
        match Api::lane_only(lane, shared, journal, &token).serve_unix(&socket).await {
            Ok(()) => log::info!("agent shell: management lane on {}; the agent shell is {}", socket.display(), if on { "open" } else { "closed" }),
            Err(e) => {
                log::error!("agent shell: cannot serve {}: {e}", socket.display());
                return;
            }
        }
        std::future::pending::<()>().await;
    });
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
