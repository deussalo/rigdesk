//! The person at the PC invites a Rig agent from the tray: "Invite a Rig agent" mints a link only Rig can redeem, since
//! claiming it takes the agent API's bearer, the key this PC was paired with. The link works once and lapses unclaimed
//! after `rigdesk_agent::session::INVITE_FOR_MS`. While it is open the tray says until when, and copies or revokes it; once an agent joined by
//! it, the tray names the agent and ends its session, as for a session the operator started. The tray process asks
//! `--server` over IPC once a second, as `notice` does. An invite takes nobody's session and replaces no viewer's link,
//! so the tray offers one only while neither is there; and it obeys the PC's switch for sessions started from Rig.

use std::sync::{mpsc, Mutex};

use hbb_common::log;
use rigdesk_agent::{operator::Switch, Entry};
use serde_json::json;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};

/// IPC config name: read, the invite as the tray shows it (see [`text`]); written, `mint`, `revoke` or `end:<agent id>`.
pub const IPC_INVITE: &str = "agent-invite";

lazy_static::lazy_static! {
    /// The open invite's link, kept only here and in the tray, to be copied again; the agent API keeps its secret hashed.
    static ref LINK: Mutex<Option<String>> = Default::default();
}

/// The name an invite's link gives this PC, which /health reports so Rig can match it.
pub fn link_host() -> String {
    crate::common::hostname().to_lowercase()
}

/// Why the PC's switch for sessions from Rig is closed, if it is: an invite is not made then, and one open is revoked.
fn closed() -> Option<String> {
    let switch = super::operator::PcSwitch { file: super::data_dir().map(|dir| dir.join(super::operator::SWITCH_FILE)) };
    switch.open().err().map(|closed| rigdesk_agent::operator::Refused::Closed(closed).words())
}

/// Why the person cannot invite an agent now, if they cannot.
fn unable() -> Option<String> {
    if let Some(words) = closed() {
        return Some(words);
    }
    let standing = super::shared().standing();
    if let Some(owner) = standing.owner {
        return Some(format!("{} holds this PC's session now", owner.title));
    }
    if standing.link_open && super::shared().invite_until().is_none() {
        return Some("a viewer's share link is open".into());
    }
    None
}

/// For the tray: `off` without a pairing with Rig or while an invite cannot be made (see [`unable`]), `held\n<agent id>\n<title>` while an agent holds a session an invite
/// started, `open\n<until ms>\n<link>` while an invite waits, else empty.
pub fn text() -> String {
    if super::bearer().is_none() {
        return "off".into();
    }
    let shared = super::shared();
    if let Some(owner) = shared.invited_owner() {
        return format!("held\n{}\n{}", owner.agent_id, owner.title);
    }
    let mut link = LINK.lock().unwrap();
    match (shared.invite_until(), link.clone()) {
        (Some(until_ms), Some(text)) => match closed() {
            None => format!("open\n{until_ms}\n{text}"),
            Some(reason) => {
                *link = None;
                if shared.revoke_invite() {
                    super::HOST.journal.record(&Entry::new("human", "invite", "revoked", json!({"reason": reason})));
                }
                "off".into()
            }
        },
        _ => {
            *link = None;
            if unable().is_some() {
                return "off".into();
            }
            String::new()
        }
    }
}

/// From the tray: the person at the PC mints an invite, takes it back, or ends the session it started.
pub fn act(verb: &str) {
    if super::bearer().is_none() {
        return;
    }
    let shared = super::shared();
    match verb.split_once(':').unwrap_or((verb, "")) {
        ("mint", _) => {
            let refused = unable();
            let Some((minted, until_ms)) = refused.is_none().then(|| shared.invite()).flatten() else {
                let reason = refused.unwrap_or_else(|| "a viewer's share link is open".into());
                super::HOST.journal.record(&Entry::new("human", "invite", "refused", json!({"reason": reason})));
                return;
            };
            *LINK.lock().unwrap() = Some(format!("rigdesk://{}/{}#{}", link_host(), minted.grant, minted.secret));
            super::HOST.journal.record(&Entry::new("human", "invite", "minted", json!({"until_ms": until_ms})));
        }
        ("revoke", _) => {
            *LINK.lock().unwrap() = None;
            if shared.revoke_invite() {
                super::HOST.journal.record(&Entry::new("human", "invite", "revoked", json!({})));
            }
        }
        ("end", agent_id) => {
            if shared.end_invited(agent_id) {
                super::HOST.journal.record(&Entry::new("human", "invite", "ended", json!({"agent_id": agent_id})));
            }
        }
        _ => log::warn!("agent: unknown invite verb from the tray"),
    }
}

enum Shown {
    Off,
    Idle(MenuItem),
    Open { link: String, said: MenuItem, copy: MenuItem, revoke: MenuItem },
    Held { agent_id: String, said: MenuItem, end: MenuItem },
}

pub struct Invites {
    rx: mpsc::Receiver<String>,
    shown: Shown,
    /// The agent last announced, so a state read again after a failed poll does not notify twice.
    toasted: Option<String>,
}

/// Starts asking `--server` for the invite's state.
pub fn start() -> Invites {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut last = None;
        loop {
            let now = crate::ipc::get_config(IPC_INVITE).ok().flatten().unwrap_or_else(|| "off".into());
            if last.as_ref() != Some(&now) {
                last = Some(now.clone());
                if tx.send(now).is_err() {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
    Invites { rx, shown: Shown::Off, toasted: None }
}

fn send(verb: String) {
    if let Err(e) = crate::ipc::set_config(IPC_INVITE, verb) {
        log::error!("tray: the invite could not reach the RigDesk service: {e}");
    }
}

/// Local time of day, as the person at the PC reads it.
fn clock(at_ms: u64) -> String {
    let at = std::time::UNIX_EPOCH + std::time::Duration::from_millis(at_ms);
    chrono::DateTime::<chrono::Local>::from(at).format("%H:%M").to_string()
}

impl Invites {
    /// Shows the entries for the invite's state as it changes; called from the tray's event loop.
    pub fn tick(&mut self, menu: &Menu) {
        let Some(text) = self.rx.try_iter().last() else { return };
        if text.is_empty() || text.starts_with("open\n") {
            self.toasted = None;
        }
        let mut lines = text.splitn(3, '\n');
        let shown = match (lines.next().unwrap_or(""), lines.next(), lines.next()) {
            ("off", ..) => Shown::Off,
            ("open", Some(until_ms), Some(link)) => Shown::Open {
                link: link.to_string(),
                said: MenuItem::new(format!("Rig invite open until {}", clock(until_ms.parse().unwrap_or_default())), false, None),
                copy: MenuItem::new("Copy the invite link", true, None),
                revoke: MenuItem::new("Revoke the invite", true, None),
            },
            ("held", Some(agent_id), Some(title)) => {
                if self.toasted.as_deref() != Some(agent_id) {
                    self.toasted = Some(agent_id.to_string());
                    #[cfg(windows)]
                    toast(title);
                    #[cfg(target_os = "linux")]
                    notify(title);
                }
                Shown::Held {
                    agent_id: agent_id.to_string(),
                    said: MenuItem::new(format!("Rig agent {title} joined by your invite"), false, None),
                    end: MenuItem::new("End the invited agent's session", true, None),
                }
            }
            _ => Shown::Idle(MenuItem::new("Invite a Rig agent", true, None)),
        };
        match std::mem::replace(&mut self.shown, Shown::Off) {
            Shown::Off => {}
            Shown::Idle(invite) => {
                menu.remove(&invite).ok();
            }
            Shown::Open { said, copy, revoke, .. } => {
                for item in [&said, &copy, &revoke] {
                    menu.remove(item).ok();
                }
            }
            Shown::Held { said, end, .. } => {
                menu.remove(&said).ok();
                menu.remove(&end).ok();
            }
        }
        match &shown {
            Shown::Off => {}
            Shown::Idle(invite) => {
                menu.append(invite).ok();
            }
            Shown::Open { said, copy, revoke, .. } => {
                menu.append_items(&[said, copy, revoke]).ok();
            }
            Shown::Held { said, end, .. } => {
                menu.append_items(&[said, end]).ok();
            }
        }
        self.shown = shown;
    }

    /// True when the event was one of the invite's entries, which it then acts on.
    pub fn clicked(&self, event: &MenuEvent) -> bool {
        match &self.shown {
            Shown::Idle(invite) if event.id == *invite.id() => send("mint".into()),
            Shown::Open { revoke, .. } if event.id == *revoke.id() => send("revoke".into()),
            Shown::Open { link, copy, .. } if event.id == *copy.id() => {
                if let Err(e) = super::shell::clipboard_set(link) {
                    log::warn!("tray: the invite link was not copied: {e}");
                }
            }
            Shown::Held { agent_id, end, .. } if event.id == *end.id() => send(format!("end:{agent_id}")),
            _ => return false,
        }
        true
    }
}

fn words(title: &str) -> (String, String) {
    (
        format!("Rig agent {title} is using this computer"),
        "It joined by your invite, which is now used up. Your own mouse and keyboard still go first, and every action is logged. End it from the RigDesk tray icon.".to_string(),
    )
}

#[cfg(target_os = "linux")]
fn notify(title: &str) {
    use dbus::arg::PropMap;
    let (summary, body) = words(title);
    let sent = dbus::blocking::Connection::new_session().and_then(|conn| {
        let proxy = conn.with_proxy("org.freedesktop.Notifications", "/org/freedesktop/Notifications", std::time::Duration::from_secs(3));
        let notified: Result<(u32,), dbus::Error> =
            proxy.method_call("org.freedesktop.Notifications", "Notify", (crate::get_app_name(), 0u32, "", summary, body, Vec::<String>::new(), PropMap::new(), 0i32));
        notified
    });
    if let Err(e) = sent {
        log::warn!("tray: the notification of an invited agent failed: {e}");
    }
}

#[cfg(windows)]
fn toast(title: &str) {
    use tauri_winrt_notification::{Duration, Sound, Toast};
    let (summary, body) = words(title);
    if let Err(e) = Toast::new(Toast::POWERSHELL_APP_ID).title(&summary).text1(&body).sound(Some(Sound::Default)).duration(Duration::Long).show() {
        log::warn!("tray: the notification of an invited agent failed: {e}");
    }
}
