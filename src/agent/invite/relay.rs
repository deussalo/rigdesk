//! The invite of a PC paired with Rig by its key alone, with no agent API token, so Rig does not reach it over the
//! tailnet: the link names the PC by RustDesk ID, and Rig redeems it inside RustDesk's login, through the relay, with a
//! proof made with its key (see `base::invite`). The tray shows it as it shows a tailnet invite, in the same words. The
//! session it starts is a RustDesk session, which the connection window names and ends as well.

use std::sync::Mutex;

use base::invite;
use hbb_common::{log, tokio::sync::mpsc::UnboundedSender};
use rigdesk_agent::Entry;
use serde_json::json;

use crate::ipc::Data;

lazy_static::lazy_static! {
    /// The connection of the agent that joined by invite, and the channel that ends it as the connection window does.
    static ref JOINED: Mutex<Option<(i32, UnboundedSender<Data>)>> = Default::default();
}

/// From the connection whose login redeemed an invite: the tray can end it, and the PC journals who joined.
pub fn joined(conn: i32, holder: &invite::Holder, end: UnboundedSender<Data>) {
    *JOINED.lock().unwrap() = Some((conn, end));
    let who = format!("{} ({})", holder.title, holder.agent);
    super::super::HOST.journal.record(&Entry::new(who, "join", "ok", json!({"by": "invite, through the relay"})));
}

/// For the tray, in [`super::text`]'s form.
pub fn text() -> String {
    if !invite::paired() {
        return "off".into();
    }
    let state = invite::state();
    if let Some(holder) = state.holder {
        return format!("held\n{}\n{}", holder.agent, holder.title);
    }
    let mut link = super::LINK.lock().unwrap();
    if let Some(reason) = super::closed() {
        *link = None;
        if invite::revoke() {
            super::super::HOST.journal.record(&Entry::new("human", "invite", "revoked", json!({"reason": reason})));
        }
        return "off".into();
    }
    match (state.open, link.clone()) {
        (Some(open), Some((text, title))) => format!("open\n{}\n{text}\n{title}", open.until_ms),
        _ => {
            *link = None;
            let agents: String = super::pickable().iter().map(|(id, title)| format!("\n{id}\t{title}")).collect();
            format!("idle{agents}")
        }
    }
}

/// From the tray, as [`super::act`].
pub fn act(verb: &str) {
    if !invite::paired() {
        return;
    }
    let journal = &super::super::HOST.journal;
    match verb.split_once(':').unwrap_or((verb, "")) {
        ("mint", agent_id) => {
            let bound = (!agent_id.is_empty()).then_some(agent_id);
            let title = super::pickable().into_iter().find(|(id, _)| id == agent_id).map_or_else(|| agent_id.to_string(), |(_, title)| title);
            let minted = match super::closed() {
                Some(reason) => Err(reason),
                None => invite::mint(bound).map_err(str::to_owned),
            };
            match minted {
                Ok(minted) => {
                    *super::LINK.lock().unwrap() = Some((minted.link, title));
                    journal.record(&Entry::new("human", "invite", "minted", json!({"until_ms": minted.until_ms, "for": bound, "via": "relay"})));
                }
                Err(reason) => journal.record(&Entry::new("human", "invite", "refused", json!({"reason": reason}))),
            }
        }
        ("revoke", _) => {
            *super::LINK.lock().unwrap() = None;
            if invite::revoke() {
                journal.record(&Entry::new("human", "invite", "revoked", json!({})));
            }
        }
        ("end", agent_id) => {
            let holds = invite::state().holder.map_or(false, |holder| holder.agent == agent_id);
            let joined = JOINED.lock().unwrap().clone();
            match joined {
                Some((conn, end)) if holds && invite::holder_conn() == Some(conn) => {
                    if end.send(Data::Close).is_ok() {
                        journal.record(&Entry::new("human", "invite", "ended", json!({"agent_id": agent_id})));
                    }
                }
                _ => log::warn!("agent: the tray ended an invited session that is no longer there"),
            }
        }
        _ => log::warn!("agent: unknown invite verb from the tray"),
    }
}
