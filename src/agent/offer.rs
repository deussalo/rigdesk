//! An agent's request to join, shown to the person at the PC in RustDesk's own connection manager while the operator's
//! ask stands in Rig: the agent is added as a client waiting for approval, over the IPC a RustDesk connection uses, so
//! the prompt is the connection manager's own Accept and Dismiss. Their answer goes out as an `offer` event with Rig's
//! ask and nonce, and the entry goes with the IPC stream, as a connection's does when it closes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use hbb_common::{log, tokio, tokio::sync::mpsc};
use rigdesk_agent::{operator::Offer, EventKind};

use crate::ipc::Data;

/// The connection manager's ids for these entries: above any a RustDesk connection is given (they count up from about
/// 1000), so the connection manager can tell Rig's request from a remote login, which never carries such an id.
pub const OFFER_ID_BASE: i32 = 1_000_000_000;
/// An offer Rig never ends is taken away after this, unanswered, as RustDesk's own accept prompt lapses; Rig shows it again.
const OFFER_FOR: Duration = Duration::from_secs(3 * 60);

static NEXT: AtomicI32 = AtomicI32::new(0);

lazy_static::lazy_static! {
    /// The prompt showing for each ask, by its id; dropping its sender ends the IPC stream, which takes the entry away.
    static ref SHOWN: Mutex<HashMap<String, (i32, mpsc::UnboundedSender<Data>)>> = Default::default();
}

/// Adds the agent to the connection manager as waiting for approval.
pub fn show(offer: Offer) -> Result<(), String> {
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| "the agent API runs on no runtime".to_string())?;
    let id = OFFER_ID_BASE + NEXT.fetch_add(1, Ordering::SeqCst) % 1_000_000;
    let (to_cm, rx_to_cm) = mpsc::unbounded_channel::<Data>();
    let (tx_from_cm, from_cm) = mpsc::unbounded_channel::<Data>();
    to_cm
        .send(Data::Login {
            id,
            is_file_transfer: false,
            is_view_camera: false,
            is_terminal: false,
            peer_id: "Rig".into(),
            name: format!("{} (Rig agent)", offer.owner.title),
            avatar: String::new(),
            authorized: false,
            port_forward: String::new(),
            keyboard: true,
            clipboard: false,
            audio: false,
            file: false,
            file_transfer_enabled: false,
            restart: false,
            recording: false,
            block_input: false,
            privacy_mode: false,
            from_switch: false,
        })
        .map_err(|e| e.to_string())?;
    SHOWN.lock().unwrap().insert(offer.ask.clone(), (id, to_cm));
    runtime.spawn(async move {
        if let Err(e) = crate::server::start_cm_ipc(rx_to_cm, tx_from_cm).await {
            log::info!("agent: the request shown in the connection manager ended: {e}");
        }
    });
    runtime.spawn(answered(offer, id, from_cm));
    Ok(())
}

/// Takes the prompt for this ask away.
pub fn end(ask: &str) -> bool {
    SHOWN.lock().unwrap().remove(ask).is_some()
}

/// This prompt's entry, unless Rig ended it or showed another for the same ask since.
fn ended(ask: &str, id: i32) -> bool {
    let mut shown = SHOWN.lock().unwrap();
    if shown.get(ask).map(|(shown, _)| *shown) != Some(id) {
        return false;
    }
    shown.remove(ask);
    true
}

async fn answered(offer: Offer, id: i32, mut from_cm: mpsc::UnboundedReceiver<Data>) {
    let answer = tokio::time::timeout(OFFER_FOR, async {
        while let Some(data) = from_cm.recv().await {
            match data {
                Data::Authorize => return Some("allow"),
                Data::Close => return Some("deny"),
                _ => {}
            }
        }
        None
    })
    .await;
    let ended = ended(&offer.ask, id);
    let answer = match answer {
        _ if !ended => return,
        Ok(Some(answer)) => answer,
        _ => "lapsed",
    };
    log::info!("agent: the person at the PC answered {}'s request ({}): {answer}", offer.owner.who(), offer.ask);
    super::shared().events.push(EventKind::Offer { ask: offer.ask, nonce: offer.nonce, answer: answer.into() });
}
