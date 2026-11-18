//! The person at the PC invites a Rig agent from the tray: "Invite a Rig agent" mints a link only Rig can redeem, since
//! claiming it takes the agent API's bearer, the key this PC was paired with. The link works once and lapses unclaimed
//! after `rigdesk_agent::session::INVITE_FOR_MS`. While it is open the tray says until when, and copies or revokes it; once an agent joined by
//! it, the tray names the agent and ends its session, as for a session the operator started. The tray process asks
//! `--server` over IPC once a second, as `notice` does. An invite takes nobody's session and replaces no viewer's link,
//! so the tray offers one only while neither is there; and it obeys the PC's switch for sessions started from Rig. Next
//! to the plain invite, which Rig binds to whichever agent claims it first, the tray offers one for each agent that last
//! used this PC, which only that agent can claim.

use std::sync::{mpsc, Mutex};

use hbb_common::log;
use rigdesk_agent::{journal::recent_agents, operator::Switch, Entry, Owner};
use serde_json::json;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};

/// IPC config name: read, the invite as the tray shows it (see [`text`]); written, `mint`, `mint:<agent id>`, `revoke` or
/// `end:<agent id>`.
pub const IPC_INVITE: &str = "agent-invite";

const PICKABLE_AGENTS: usize = 3;

lazy_static::lazy_static! {
    /// The open invite's link, kept only here and in the tray, to be copied again, with the title of the agent it is for,
    /// if the person chose one; the agent API keeps its secret hashed.
    static ref LINK: Mutex<Option<(String, String)>> = Default::default();
    /// The agents the tray offers, newest first: read from the journal once, then kept as agents take the session.
    static ref PICKABLE: Mutex<Option<Vec<(String, String)>>> = Default::default();
}

/// The agents the tray offers, as (id, title). The journal is read once, on its own thread, as this answers the tray's IPC.
fn pickable() -> Vec<(String, String)> {
    let mut kept = PICKABLE.lock().unwrap();
    if kept.is_none() {
        *kept = Some(Vec::new());
        std::thread::spawn(|| {
            let read = recent_agents(&super::HOST.journal.tail(usize::MAX), PICKABLE_AGENTS);
            let mut kept = PICKABLE.lock().unwrap();
            let mut agents = kept.take().unwrap_or_default();
            agents.extend(read.into_iter().filter(|(id, _)| !agents.iter().any(|(met, _)| met == id)).collect::<Vec<_>>());
            agents.truncate(PICKABLE_AGENTS);
            *kept = Some(agents);
        });
    }
    kept.clone().unwrap_or_default()
}

/// An agent holding the session goes first in the list.
fn met(owner: &Owner) {
    let mut kept = PICKABLE.lock().unwrap();
    let agents = kept.get_or_insert_with(Vec::new);
    if agents.first().map_or(false, |(id, _)| *id == owner.agent_id) {
        return;
    }
    agents.retain(|(id, _)| *id != owner.agent_id);
    agents.insert(0, (owner.agent_id.clone(), owner.title.split_whitespace().collect::<Vec<_>>().join(" ")));
    agents.truncate(PICKABLE_AGENTS);
}

/// The name an invite's link gives this PC, which /health reports so Rig can match it.
pub fn link_host() -> String {
    crate::common::hostname().to_lowercase()
}

/// Why the person cannot invite an agent now, if they cannot.
fn unable() -> Option<String> {
    let switch = super::operator::PcSwitch { file: super::data_dir().map(|dir| dir.join(super::operator::SWITCH_FILE)) };
    if let Err(closed) = switch.open() {
        return Some(rigdesk_agent::operator::Refused::Closed(closed).words());
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

/// For the tray: `off` without a pairing with Rig or while an invite cannot be made (see [`unable`]),
/// `held\n<agent id>\n<title>` while an agent holds a session an invite started,
/// `open\n<until ms>\n<link>\n<title of the agent it is for, or nothing>` while an invite waits, else `idle` and a
/// `\n<agent id>\t<title>` for each agent the person can pick.
pub fn text() -> String {
    if super::bearer().is_none() {
        return "off".into();
    }
    let shared = super::shared();
    if let Some(owner) = shared.standing().owner {
        met(&owner);
    }
    if let Some(owner) = shared.invited_owner() {
        return format!("held\n{}\n{}", owner.agent_id, owner.title);
    }
    let mut link = LINK.lock().unwrap();
    match (shared.invite_until(), link.clone()) {
        (Some(until_ms), Some((text, title))) => format!("open\n{until_ms}\n{text}\n{title}"),
        _ => {
            *link = None;
            if unable().is_some() {
                return "off".into();
            }
            let agents: String = pickable().iter().map(|(id, title)| format!("\n{id}\t{title}")).collect();
            format!("idle{agents}")
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
        ("mint", agent_id) => {
            let bound = (!agent_id.is_empty()).then(|| agent_id.to_string());
            let title = pickable().into_iter().find(|(id, _)| id == agent_id).map_or_else(|| agent_id.to_string(), |(_, title)| title);
            let refused = unable();
            let Some((minted, until_ms)) = refused.is_none().then(|| shared.invite(bound.clone())).flatten() else {
                let reason = refused.unwrap_or_else(|| "a viewer's share link is open".into());
                super::HOST.journal.record(&Entry::new("human", "invite", "refused", json!({"reason": reason})));
                return;
            };
            *LINK.lock().unwrap() = Some((format!("rigdesk://{}/{}#{}", link_host(), minted.grant, minted.secret), title));
            super::HOST.journal.record(&Entry::new("human", "invite", "minted", json!({"until_ms": until_ms, "for": bound})));
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
    Idle { invite: MenuItem, only: Vec<(String, MenuItem)> },
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
        if text.starts_with("idle") || text.starts_with("open\n") {
            self.toasted = None;
        }
        let mut lines = text.splitn(4, '\n');
        let shown = match (lines.next().unwrap_or(""), lines.next(), lines.next(), lines.next()) {
            ("off", ..) => Shown::Off,
            ("open", Some(until_ms), Some(link), title) => Shown::Open {
                link: link.to_string(),
                said: MenuItem::new(
                    format!(
                        "Rig invite{} open until {}",
                        title.filter(|title| !title.is_empty()).map_or_else(String::new, |title| format!(" for {title}")),
                        clock(until_ms.parse().unwrap_or_default())
                    ),
                    false,
                    None,
                ),
                copy: MenuItem::new("Copy the invite link", true, None),
                revoke: MenuItem::new("Revoke the invite", true, None),
            },
            ("held", Some(agent_id), Some(title), _) => {
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
            _ => Shown::Idle {
                invite: MenuItem::new("Invite a Rig agent", true, None),
                only: text
                    .strip_prefix("idle\n")
                    .into_iter()
                    .flat_map(str::lines)
                    .filter_map(|line| line.split_once('\t'))
                    .map(|(id, title)| (id.to_string(), MenuItem::new(format!("Invite only {title} ({id})"), true, None)))
                    .collect(),
            },
        };
        match std::mem::replace(&mut self.shown, Shown::Off) {
            Shown::Off => {}
            Shown::Idle { invite, only } => {
                menu.remove(&invite).ok();
                for (_, item) in &only {
                    menu.remove(item).ok();
                }
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
            Shown::Idle { invite, only } => {
                menu.append(invite).ok();
                for (_, item) in only {
                    menu.append(item).ok();
                }
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
        if let Shown::Idle { only, .. } = &self.shown {
            if let Some((agent_id, _)) = only.iter().find(|(_, item)| event.id == *item.id()) {
                send(format!("mint:{agent_id}"));
                return true;
            }
        }
        match &self.shown {
            Shown::Idle { invite, .. } if event.id == *invite.id() => send("mint".into()),
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
