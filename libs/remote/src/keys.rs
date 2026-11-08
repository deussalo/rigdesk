//! The agent's pointer and keyboard actions as the `MouseEvent`/`KeyEvent` messages a RustDesk viewer sends, so a stock
//! PC injects them exactly as it would a person's. Mirrors `src/agent/input.rs`, which feeds the same events to
//! RigDesk's own injection path.

use base::message_proto::{ControlKey, KeyEvent, KeyboardMode, Message, MouseEvent};
use hbb_common::protobuf::EnumOrUnknown;

const MOUSE_TYPE_MOVE: i32 = 0;
const MOUSE_TYPE_DOWN: i32 = 1;
const MOUSE_TYPE_UP: i32 = 2;
const MOUSE_TYPE_WHEEL: i32 = 3;
const MOUSE_BUTTON_LEFT: i32 = 0x01;
const MOUSE_BUTTON_RIGHT: i32 = 0x02;

fn mouse(mask: i32, x: i32, y: i32) -> Message {
    let mut evt = MouseEvent::new();
    evt.mask = mask;
    evt.x = x;
    evt.y = y;
    let mut msg = Message::new();
    msg.set_mouse_event(evt);
    msg
}

fn key(evt: KeyEvent) -> Message {
    let mut msg = Message::new();
    msg.set_key_event(evt);
    msg
}

fn button(name: &str) -> i32 {
    match name {
        "right" => MOUSE_BUTTON_RIGHT,
        _ => MOUSE_BUTTON_LEFT,
    }
}

pub fn move_to(x: i32, y: i32) -> Vec<Message> {
    vec![mouse(MOUSE_TYPE_MOVE, x, y)]
}

pub fn click(x: i32, y: i32, btn: &str, double: bool) -> Vec<Message> {
    let b = button(btn);
    let mut out = vec![mouse(MOUSE_TYPE_MOVE, x, y)];
    for _ in 0..if double { 2 } else { 1 } {
        out.push(mouse((b << 3) | MOUSE_TYPE_DOWN, x, y));
        out.push(mouse((b << 3) | MOUSE_TYPE_UP, x, y));
    }
    out
}

pub fn drag(from: (i32, i32), to: (i32, i32), btn: &str) -> Vec<Message> {
    let b = button(btn);
    vec![
        mouse(MOUSE_TYPE_MOVE, from.0, from.1),
        mouse((b << 3) | MOUSE_TYPE_DOWN, from.0, from.1),
        mouse(MOUSE_TYPE_MOVE, to.0, to.1),
        mouse((b << 3) | MOUSE_TYPE_UP, to.0, to.1),
    ]
}

/// Scroll by whole notches; positive `dy` scrolls up, positive `dx` right.
pub fn scroll(dx: i32, dy: i32) -> Vec<Message> {
    vec![mouse(MOUSE_TYPE_WHEEL, -dx, dy)]
}

fn legacy() -> KeyEvent {
    let mut evt = KeyEvent::new();
    evt.mode = EnumOrUnknown::new(KeyboardMode::Legacy);
    evt
}

/// A run of text. Seq enters the whole string at once, so it is unaffected by held modifiers.
pub fn type_text(text: &str) -> Vec<Message> {
    let mut evt = legacy();
    evt.set_seq(text.to_owned());
    vec![key(evt)]
}

/// A named key once (down then up).
pub fn press_key(name: &str) -> Result<Vec<Message>, String> {
    let ck = control_key(name).ok_or_else(|| format!("unknown key: {name}"))?;
    Ok(tap(|evt| evt.set_control_key(ck), &[]))
}

/// A chord: modifiers held while the last key is tapped, e.g. ctrl+c, ctrl+shift+n, alt+F4.
pub fn shortcut(spec: &str) -> Result<Vec<Message>, String> {
    let parts: Vec<&str> = spec.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
    let Some((last, mods)) = parts.split_last() else {
        return Err("empty shortcut".into());
    };
    let modifiers: Vec<ControlKey> =
        mods.iter().map(|m| modifier(m).ok_or_else(|| format!("unknown modifier: {m}"))).collect::<Result<_, _>>()?;
    if let Some(ck) = control_key(last) {
        return Ok(tap(|evt| evt.set_control_key(ck), &modifiers));
    }
    let mut chars = last.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return Err(format!("unknown key in shortcut: {last}"));
    };
    Ok(tap(|evt| evt.set_chr(c as u32), &modifiers))
}

/// Each key event names the modifiers that must be down with it: the PC releases any modifier an event does not list
/// before pressing its key.
fn with_modifiers(modifiers: &[ControlKey]) -> KeyEvent {
    let mut evt = legacy();
    evt.modifiers = modifiers.iter().map(|m| EnumOrUnknown::new(*m)).collect();
    evt
}

fn tap(set: impl Fn(&mut KeyEvent), modifiers: &[ControlKey]) -> Vec<Message> {
    let mut out = hold(modifiers, true);
    let mut evt = with_modifiers(modifiers);
    set(&mut evt);
    evt.down = true;
    out.push(key(evt.clone()));
    evt.down = false;
    out.push(key(evt));
    out.extend(hold(modifiers, false));
    out
}

fn hold(modifiers: &[ControlKey], down: bool) -> Vec<Message> {
    modifiers
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mut evt = with_modifiers(&modifiers[..i]);
            evt.set_control_key(*m);
            evt.down = down;
            key(evt)
        })
        .collect()
}

fn modifier(name: &str) -> Option<ControlKey> {
    Some(match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => ControlKey::Control,
        "alt" | "option" => ControlKey::Alt,
        "shift" => ControlKey::Shift,
        "win" | "meta" | "cmd" | "super" => ControlKey::Meta,
        _ => return None,
    })
}

fn control_key(name: &str) -> Option<ControlKey> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => ControlKey::Return,
        "tab" => ControlKey::Tab,
        "esc" | "escape" => ControlKey::Escape,
        "backspace" => ControlKey::Backspace,
        "delete" | "del" => ControlKey::Delete,
        "insert" | "ins" => ControlKey::Insert,
        "space" => ControlKey::Space,
        "home" => ControlKey::Home,
        "end" => ControlKey::End,
        "pageup" | "pgup" => ControlKey::PageUp,
        "pagedown" | "pgdn" => ControlKey::PageDown,
        "up" => ControlKey::UpArrow,
        "down" => ControlKey::DownArrow,
        "left" => ControlKey::LeftArrow,
        "right" => ControlKey::RightArrow,
        "capslock" => ControlKey::CapsLock,
        "f1" => ControlKey::F1,
        "f2" => ControlKey::F2,
        "f3" => ControlKey::F3,
        "f4" => ControlKey::F4,
        "f5" => ControlKey::F5,
        "f6" => ControlKey::F6,
        "f7" => ControlKey::F7,
        "f8" => ControlKey::F8,
        "f9" => ControlKey::F9,
        "f10" => ControlKey::F10,
        "f11" => ControlKey::F11,
        "f12" => ControlKey::F12,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_holds_ctrl_around_the_letter_and_names_it_on_each_event() {
        let msgs = shortcut("ctrl+c").unwrap();
        let events: Vec<&KeyEvent> = msgs.iter().map(|m| m.key_event()).collect();
        assert_eq!(events.len(), 4);
        assert!(events[0].down && events[0].control_key() == ControlKey::Control);
        assert!(events[1].down && events[1].chr() == 'c' as u32);
        assert_eq!(events[1].modifiers, vec![EnumOrUnknown::new(ControlKey::Control)]);
        assert!(!events[2].down && events[2].chr() == 'c' as u32);
        assert!(!events[3].down && events[3].control_key() == ControlKey::Control);
    }
}
