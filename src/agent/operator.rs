//! Sessions the operator starts from Rig, with no share link: the PC's switch for them, and what the person at the PC
//! is told. The switch is `agent-join` beside api.token, written from the enrollment (`-AgentJoin on|off`), and the
//! pause in the PIN-locked security settings. The tray asks this process over IPC who holds such a session, and ends it.

use std::path::PathBuf;

use base::config::keys;
use hbb_common::{config::Config, log};
use rigdesk_agent::operator::{session_line, Closed, Offer, Switch};
pub use rigdesk_agent::operator::{disconnect, parse_session, words, JoinedBy};

pub const SWITCH_FILE: &str = "agent-join";
/// IPC config names: read, the agent holding an operator-started session as `session_line` writes it (empty when none);
/// written with that agent id, its session ends.
pub const IPC_SESSION: &str = "agent-operator-session";
pub const IPC_END: &str = "agent-operator-end";

pub struct PcSwitch {
    pub file: Option<PathBuf>,
}

impl Switch for PcSwitch {
    fn open(&self) -> Result<(), Closed> {
        match self.file.as_ref().map(std::fs::read_to_string) {
            Some(Ok(on)) if on.trim() == "on" => {}
            Some(Ok(_)) => return Err(Closed::Off("its enrollment turned them off (-AgentJoin off); an administrator turns them on by enrolling it again with -AgentJoin on".into())),
            _ => return Err(Closed::Off("it was enrolled before they existed; an administrator turns them on by enrolling it again with -AgentJoin on".into())),
        }
        if Config::get_option(keys::OPTION_AGENT_JOIN_PAUSED) == "Y" {
            return Err(Closed::Paused);
        }
        Ok(())
    }

    fn offer(&self, offer: Offer) -> Result<(), String> {
        super::offer::show(offer)
    }

    fn offer_end(&self, ask: &str) -> bool {
        super::offer::end(ask)
    }

    fn offers(&self) -> bool {
        true
    }
}

/// For the tray: who holds a session the operator started, as `session_line` writes it; empty when nobody does.
pub fn session_text() -> String {
    super::shared().operator_session().map_or(String::new(), |(owner, how)| session_line(&owner.agent_id, &owner.title, how))
}

/// The person at the PC ended that agent's operator-started session from the tray.
pub fn end(agent_id: &str) {
    if super::shared().end_operator(agent_id) {
        log::info!("agent: the person at the PC ended {agent_id}'s session started from Rig");
    }
}

/// On Linux this process runs as the signed-in user, on their session bus, so it says itself when such a session starts;
/// on Windows it is SYSTEM, and the tray says it.
#[cfg(target_os = "linux")]
pub fn notify_starts() {
    use hbb_common::tokio;
    use rigdesk_agent::{EventKind, Holder};
    let mut rx = super::shared().events.subscribe();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let EventKind::Holder { holder: Holder::Agent, by } = &event.kind {
                        if by == "operator" {
                            if let Some((owner, how)) = super::shared().operator_session() {
                                tokio::task::spawn_blocking(move || notify(&owner.title, how));
                            }
                        }
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(target_os = "linux")]
fn notify(title: &str, how: JoinedBy) {
    use dbus::arg::PropMap;
    let (summary, body) = words(title, how);
    let sent = dbus::blocking::Connection::new_session().and_then(|conn| {
        let proxy = conn.with_proxy("org.freedesktop.Notifications", "/org/freedesktop/Notifications", std::time::Duration::from_secs(3));
        let notified: Result<(u32,), dbus::Error> =
            proxy.method_call("org.freedesktop.Notifications", "Notify", (crate::get_app_name(), 0u32, "", summary, body, Vec::<String>::new(), PropMap::new(), 0i32));
        notified
    });
    if let Err(e) = sent {
        log::warn!("agent: the desktop notification of a session started from Rig failed: {e}");
    }
}
