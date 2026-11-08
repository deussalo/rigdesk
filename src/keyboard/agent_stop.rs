//! Ctrl+Alt+Esc in a remote tab is the agent panel's Stop. It is caught on the session's key
//! path, which both the rdev grab and Flutter's keys take: while the grab holds the keyboard,
//! Flutter never sees the key.

use rdev::{Event, EventType, Key};
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "flutter")]
static ESC_TAKEN: AtomicBool = AtomicBool::new(false);

/// True when the event is the Stop chord's Esc, which then never reaches the peer; its first
/// press tells the panel.
#[cfg(feature = "flutter")]
pub fn try_stop(session_id: Option<&hbb_common::SessionID>, event: &Event) -> bool {
    let (alt, ctrl, _, _) = super::client::get_modifiers_state(false, false, false, false);
    let (taken, fire) = stop_chord(event, ctrl, alt, &ESC_TAKEN);
    if fire {
        let session_id = session_id.copied().unwrap_or_else(crate::flutter::get_cur_session_id);
        crate::flutter::push_session_event(&session_id, "agent_stop", vec![]);
    }
    taken
}

/// Whether the event is taken from the peer, and whether it fires Stop now. `down` keeps a taken
/// Esc's repeats and release off the peer as well, so it never sees a key-up without its key-down.
#[cfg(any(feature = "flutter", test))]
fn stop_chord(event: &Event, ctrl: bool, alt: bool, down: &AtomicBool) -> (bool, bool) {
    match event.event_type {
        EventType::KeyPress(Key::Escape) if (ctrl && alt) || down.load(Ordering::SeqCst) => {
            (true, !down.swap(true, Ordering::SeqCst))
        }
        EventType::KeyRelease(Key::Escape) => (down.swap(false, Ordering::SeqCst), false),
        _ => (false, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn key(event_type: EventType) -> Event {
        Event {
            time: SystemTime::now(),
            unicode: None,
            platform_code: 0,
            position_code: 0,
            event_type,
            usb_hid: 0,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            extra_data: 0,
        }
    }

    #[test]
    fn ctrl_alt_esc_stops_once_and_never_reaches_the_peer_while_esc_alone_does() {
        let down = AtomicBool::new(false);
        let press = key(EventType::KeyPress(Key::Escape));
        let release = key(EventType::KeyRelease(Key::Escape));
        assert_eq!(stop_chord(&press, false, false, &down), (false, false));
        assert_eq!(stop_chord(&release, false, false, &down), (false, false));
        assert_eq!(stop_chord(&press, true, true, &down), (true, true));
        assert_eq!(stop_chord(&press, true, true, &down), (true, false));
        assert_eq!(stop_chord(&release, false, false, &down), (true, false));
        assert_eq!(stop_chord(&press, true, false, &down), (false, false));
    }
}
