//! The viewer's agent controls: requests to the PC's share, and the PC's `AgentState` turned into the JSON the UI
//! reads. The share itself lives on the PC (`src/agent`); this side only asks and shows.

use base::message_proto::{agent_request, AgentNote, AgentRequest, AgentState, Message};
use hbb_common::{base64::Engine, log};
use serde_json::json;

use crate::{
    client::{Data, Interface},
    ui_session_interface::{InvokeUiSession, Session},
};

/// Build the request for `verb`: share, hand, take_back, block, unblock, revoke, note (with text and an optional
/// PNG) or frame (a still of display `display`).
pub fn request(verb: &str, text: String, png: Vec<u8>, display: i32) -> Option<Message> {
    use agent_request::Union;
    let union = match verb {
        "share" => Union::Share(true),
        "hand" => Union::Hand(true),
        "take_back" => Union::TakeBack(true),
        "block" => Union::Block(true),
        "unblock" => Union::Unblock(true),
        "revoke" => Union::Revoke(true),
        "note" => Union::Note(AgentNote { text, png: png.into(), ..Default::default() }),
        "frame" => Union::Frame(display),
        _ => return None,
    };
    let mut req = AgentRequest::new();
    req.union = Some(union);
    let mut msg = Message::new();
    msg.set_agent_request(req);
    Some(msg)
}

pub fn send<T: InvokeUiSession>(session: &Session<T>, verb: &str, text: String, png: Vec<u8>, display: i32) {
    match request(verb, text, png, display) {
        Some(msg) => session.send(Data::Message(msg)),
        None => log::warn!("unknown agent request {verb}"),
    }
}

pub fn state_json(state: &AgentState) -> String {
    let action = state.action.as_ref().map(|a| {
        json!({
            "agent_title": a.agent_title,
            "tool": a.tool,
            "summary": a.summary,
            "ok": a.ok,
            "x": if a.has_point { json!(a.x) } else { json!(null) },
            "y": if a.has_point { json!(a.y) } else { json!(null) },
        })
    });
    let frame = if state.frame.is_empty() {
        String::new()
    } else {
        hbb_common::base64::engine::general_purpose::STANDARD.encode(&state.frame)
    };
    json!({
        "holder": state.holder,
        "owner_title": state.owner_title,
        "owner_graph": state.owner_graph,
        "link_open": state.link_open,
        "host": state.host,
        "grant": state.grant,
        "secret": state.secret,
        "action": action,
        "note": state.note,
        "frame": frame,
    })
    .to_string()
}
