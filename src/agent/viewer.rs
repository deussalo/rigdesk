//! The viewer's side of the share, over the peer connection: each authorized remote session hears the share's state
//! (holder, owner, the agent's last action) as `AgentState`, and drives it with `AgentRequest` -- mint a link, hand
//! input to the agent or take it back, block, end the share, send the agent a message with an image, or fetch a still
//! of a display for the message box.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Once},
};

use base::message_proto::{agent_request, AgentAction, AgentRequest, AgentState, Message};
use hbb_common::{log, tokio};
use rigdesk_agent::{EventKind, Holder};

pub type Push = Box<dyn FnMut(Arc<Message>) + Send>;

lazy_static::lazy_static! {
    static ref VIEWERS: Mutex<HashMap<i32, Push>> = Default::default();
}
static FORWARD: Once = Once::new();

/// An authorized remote session opened: it hears the share's state from now on, starting with the current one.
pub fn join(conn_id: i32, mut push: Push) {
    FORWARD.call_once(forward);
    push(wrap(state()));
    VIEWERS.lock().unwrap().insert(conn_id, push);
}

pub fn leave(conn_id: i32) {
    VIEWERS.lock().unwrap().remove(&conn_id);
}

pub fn request(conn_id: i32, req: AgentRequest) {
    use agent_request::Union;
    match req.union {
        Some(Union::Share(_)) => {
            let minted = super::mint_link();
            broadcast(state());
            let mut reply = state();
            reply.grant = minted.grant;
            reply.secret = minted.secret;
            send_to(conn_id, reply);
        }
        Some(Union::Hand(_)) => {
            if !super::hand_to_agent() {
                say(conn_id, "No agent holds this desktop's share yet, or input is blocked");
            }
        }
        Some(Union::TakeBack(_)) => {
            super::human_took_over("take back");
        }
        Some(Union::Block(_)) => {
            super::block_agent();
        }
        Some(Union::Unblock(_)) => {
            super::unblock_agent();
        }
        Some(Union::Revoke(_)) => {
            if !super::revoke("the human ended the share") {
                broadcast(state());
            }
        }
        Some(Union::Note(note)) => {
            let Some(owner) = super::standing().owner else {
                say(conn_id, "No agent holds this desktop's share; share it with an agent first");
                return;
            };
            let pngs = if note.png.is_empty() { Vec::new() } else { vec![note.png.to_vec()] };
            super::human_message(note.text, pngs);
            say(conn_id, &format!("Sent to {}", owner.title));
        }
        Some(Union::Frame(index)) => {
            tokio::task::spawn_blocking(move || {
                let mut reply = state();
                match super::capture::display_png(index.max(0) as usize) {
                    Ok((png, _, _)) => reply.frame = png.into(),
                    Err(e) => reply.note = format!("Could not capture the screen: {e}"),
                }
                send_to(conn_id, reply);
            });
        }
        None => {}
    }
}

fn wrap(state: AgentState) -> Arc<Message> {
    let mut msg = Message::new();
    msg.set_agent_state(state);
    Arc::new(msg)
}

fn state() -> AgentState {
    let standing = super::standing();
    let mut state = AgentState::new();
    state.holder = match standing.holder {
        Some(Holder::Human) => "human",
        Some(Holder::Agent) => "agent",
        Some(Holder::Blocked) => "blocked",
        None => "",
    }
    .to_owned();
    if let Some(owner) = standing.owner {
        state.owner_title = owner.title;
        state.owner_graph = owner.graph;
    }
    state.link_open = standing.link_open;
    state.host = crate::common::whoami_hostname();
    state
}

fn say(conn_id: i32, words: &str) {
    let mut reply = state();
    reply.note = words.to_owned();
    send_to(conn_id, reply);
}

fn send_to(conn_id: i32, state: AgentState) {
    if let Some(push) = VIEWERS.lock().unwrap().get_mut(&conn_id) {
        push(wrap(state));
    }
}

fn broadcast(state: AgentState) {
    let msg = wrap(state);
    for push in VIEWERS.lock().unwrap().values_mut() {
        push(msg.clone());
    }
}

/// Tell every viewer of each share change and agent action as it happens.
fn forward() {
    let mut rx = super::shared().events.subscribe();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match rx.recv().await {
                Ok(event) => match event.kind {
                    EventKind::Claimed { .. } | EventKind::Holder { .. } | EventKind::Revoked { .. } => broadcast(state()),
                    EventKind::Action { agent_title, tool, summary, ok, x, y } => {
                        let mut action = AgentAction::new();
                        action.agent_title = agent_title;
                        action.tool = tool;
                        action.summary = summary;
                        action.ok = ok;
                        if let (Some(x), Some(y)) = (x, y) {
                            action.has_point = true;
                            action.x = x;
                            action.y = y;
                        }
                        let mut state = state();
                        state.action = Some(action).into();
                        broadcast(state);
                    }
                    EventKind::Message { .. } | EventKind::Dropped { .. } => {}
                },
                Err(RecvError::Lagged(_)) => broadcast(state()),
                Err(RecvError::Closed) => break,
            }
        }
        log::info!("agent: viewer forwarding ended");
    });
}
