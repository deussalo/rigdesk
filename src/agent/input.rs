//! The agent's pointer and keyboard actions, built as the same `MouseEvent`/`KeyEvent` the viewer sends and pushed
//! through `input_service`'s handlers. Because it is the one injection path, the human watching the session sees the
//! agent move, click and type live, and the cursor is tagged with the agent so the viewer can label it.

use base::message_proto::{ControlKey, KeyEvent, KeyboardMode, MouseEvent};
use hbb_common::protobuf::EnumOrUnknown;

use crate::input::{
    MOUSE_BUTTON_LEFT, MOUSE_BUTTON_RIGHT, MOUSE_TYPE_DOWN, MOUSE_TYPE_MOVE, MOUSE_TYPE_UP, MOUSE_TYPE_WHEEL,
};
use crate::server::input_service::{handle_key, handle_mouse};

use super::AGENT_CONN_ID;

/// A colour for the agent's cursor overlay, distinct from any peer's.
const AGENT_ARGB: u32 = 0xff22c55e;

fn send_mouse(mask: i32, x: i32, y: i32) {
    let mut evt = MouseEvent::new();
    evt.mask = mask;
    evt.x = x;
    evt.y = y;
    handle_mouse(&evt, AGENT_CONN_ID, "agent".to_owned(), AGENT_ARGB, true, true);
}

fn button(name: &str) -> i32 {
    match name {
        "right" => MOUSE_BUTTON_RIGHT,
        _ => MOUSE_BUTTON_LEFT,
    }
}

pub fn move_to(x: i32, y: i32) {
    send_mouse(MOUSE_TYPE_MOVE, x, y);
}

pub fn click(x: i32, y: i32, btn: &str, double: bool) {
    let b = button(btn);
    send_mouse(MOUSE_TYPE_MOVE, x, y);
    let presses = if double { 2 } else { 1 };
    for _ in 0..presses {
        send_mouse((b << 3) | MOUSE_TYPE_DOWN, x, y);
        send_mouse((b << 3) | MOUSE_TYPE_UP, x, y);
    }
}

pub fn drag(from: (i32, i32), to: (i32, i32), btn: &str) {
    let b = button(btn);
    send_mouse(MOUSE_TYPE_MOVE, from.0, from.1);
    send_mouse((b << 3) | MOUSE_TYPE_DOWN, from.0, from.1);
    send_mouse(MOUSE_TYPE_MOVE, to.0, to.1);
    send_mouse((b << 3) | MOUSE_TYPE_UP, to.0, to.1);
}

/// Scroll by whole notches; positive `dy` scrolls up, positive `dx` right.
pub fn scroll(dx: i32, dy: i32) {
    let mut evt = MouseEvent::new();
    evt.mask = MOUSE_TYPE_WHEEL;
    evt.x = -dx;
    evt.y = dy;
    handle_mouse(&evt, AGENT_CONN_ID, "agent".to_owned(), AGENT_ARGB, true, true);
}

fn legacy() -> KeyEvent {
    let mut evt = KeyEvent::new();
    evt.mode = EnumOrUnknown::new(KeyboardMode::Legacy);
    evt
}

/// Type a run of text. Seq enters the whole string at once, so it is unaffected by held modifiers.
pub fn type_text(text: &str) {
    let mut evt = legacy();
    evt.set_seq(text.to_owned());
    handle_key(&evt);
}

/// A named key once (down then up): Enter, Tab, Escape, Backspace, Delete, arrows, F1-F12, Home, End, and so on.
pub fn press_key(name: &str) -> Result<(), String> {
    let ck = control_key(name).ok_or_else(|| format!("unknown key: {name}"))?;
    tap(ck, &[]);
    Ok(())
}

/// A chord: modifiers held while the last key is tapped, e.g. ctrl+c, ctrl+shift+n, alt+F4.
pub fn shortcut(spec: &str) -> Result<(), String> {
    let parts: Vec<&str> = spec.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
    let Some((last, mods)) = parts.split_last() else {
        return Err("empty shortcut".into());
    };
    let modifiers: Vec<ControlKey> = mods.iter().map(|m| modifier(m).ok_or_else(|| format!("unknown modifier: {m}"))).collect::<Result<_, _>>()?;
    if let Some(ck) = control_key(last) {
        tap(ck, &modifiers);
        return Ok(());
    }
    let mut chars = last.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return Err(format!("unknown key in shortcut: {last}"));
    };
    tap_unicode(c, &modifiers);
    Ok(())
}

/// Each key event names the modifiers that must be down with it: the injection path releases any modifier the event
/// does not list before pressing its key, as it does for the viewer's own events.
fn with_modifiers(modifiers: &[ControlKey]) -> KeyEvent {
    let mut evt = legacy();
    evt.modifiers = modifiers.iter().map(|m| EnumOrUnknown::new(*m)).collect();
    evt
}

fn tap(ck: ControlKey, modifiers: &[ControlKey]) {
    hold(modifiers, true);
    let mut evt = with_modifiers(modifiers);
    evt.set_control_key(ck);
    evt.down = true;
    handle_key(&evt);
    evt.down = false;
    handle_key(&evt);
    hold(modifiers, false);
}

fn tap_unicode(c: char, modifiers: &[ControlKey]) {
    hold(modifiers, true);
    let mut evt = with_modifiers(modifiers);
    evt.set_chr(c as u32);
    evt.down = true;
    handle_key(&evt);
    evt.down = false;
    handle_key(&evt);
    hold(modifiers, false);
}

fn hold(modifiers: &[ControlKey], down: bool) {
    for (i, m) in modifiers.iter().enumerate() {
        let mut evt = with_modifiers(&modifiers[..i]);
        evt.set_control_key(*m);
        evt.down = down;
        handle_key(&evt);
    }
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
