//! The stock PC behind the agent API. Its tools are what the RustDesk protocol itself carries: screenshots (the PC
//! renders the PNG), pointer and keyboard input, and the clipboard. A RigDesk PC adds the UI tree, app launching and
//! PowerShell; a stock PC has no channel for them.

use std::sync::Arc;
use std::time::Duration;

use hbb_common::serde_json::{json, Value};
use rigdesk_agent::{Called, Content, Desk, ToolSpec};

use crate::keys;
use crate::link::{Link, Peer};

const CONNECT_WAIT: Duration = Duration::from_secs(20);
const SCREENSHOT_WAIT: Duration = Duration::from_secs(15);

pub struct RemoteDesk {
    link: Arc<Link>,
}

impl RemoteDesk {
    pub fn new(link: Arc<Link>) -> Self {
        RemoteDesk { link }
    }
}

fn int(args: &Value, key: &str) -> Result<i32, String> {
    args.get(key).and_then(Value::as_i64).map(|v| v as i32).ok_or_else(|| format!("missing integer {key}"))
}

fn text(args: &Value, key: &str) -> Result<String, String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| format!("missing string {key}"))
}

fn ok(words: impl Into<String>) -> Result<Vec<Content>, String> {
    Ok(vec![Content::Text(words.into())])
}

/// The display a call names, or the one the PC is showing.
fn display(peer: &Peer, args: &Value) -> Result<(usize, i32, i32, i32, i32), String> {
    let index = args.get("display").and_then(Value::as_u64).map(|d| d as usize).unwrap_or(peer.current);
    let d = peer.displays.get(index).ok_or_else(|| format!("no display {index}; the PC has {}", peer.displays.len()))?;
    Ok((index, d.x, d.y, d.width, d.height))
}

/// Screenshot pixels on that display to the PC's desktop coordinates.
fn at(peer: &Peer, args: &Value, x: i32, y: i32) -> Result<(i32, i32), String> {
    let (_, ox, oy, _, _) = display(peer, args)?;
    let nudge = windows_truncation_nudge(peer);
    Ok((ox + x + nudge, oy + y + nudge))
}

// Stock RustDesk on Windows scales to SendInput's 0..65535 range rounding down, and Windows rounds down again, so every
// pointer position lands 1 px up and left; asking for the next pixel lands on the one meant.
fn windows_truncation_nudge(peer: &Peer) -> i32 {
    i32::from(peer.platform == "Windows")
}

fn scroll_rate(peer: &Peer) -> &'static str {
    if peer.platform == "Windows" {
        "1 notch is 3 lines unless the PC's WheelScrollLines setting differs; apps that scroll by pixels vary"
    } else {
        "how far a notch goes is up to the app"
    }
}

/// What a scroll did, in notches, with how far a notch goes.
fn scrolled(dx: i32, dy: i32, rate: &str) -> String {
    let notches = |n: i32, more: &str, less: &str| format!("{} notch{} {}", n.abs(), if n.abs() == 1 { "" } else { "es" }, if n > 0 { more } else { less });
    let moves: Vec<String> = [(dy != 0).then(|| notches(dy, "up", "down")), (dx != 0).then(|| notches(dx, "right", "left"))].into_iter().flatten().collect();
    if moves.is_empty() {
        return "scrolled nothing: dx and dy are 0".into();
    }
    format!("scrolled {} ({rate})", moves.join(" and "))
}

impl Desk for RemoteDesk {
    fn tools(&self) -> Vec<ToolSpec> {
        let obj = |props: Value, required: &[&str]| json!({"type": "object", "properties": props, "required": required});
        let disp = json!({"type": "integer", "description": "which display; the one the PC shows when left out"});
        vec![
            ToolSpec { name: "screenshot", read_only: true,
                description: "A PNG of one display right now. Pointer tools take x,y in this image's pixels.",
                input_schema: obj(json!({"display": disp}), &[]) },
            ToolSpec { name: "move", read_only: false, description: "Move the pointer to x,y (screenshot pixels).",
                input_schema: obj(json!({"x": {"type": "integer"}, "y": {"type": "integer"}, "display": disp}), &["x", "y"]) },
            ToolSpec { name: "click", read_only: false,
                description: "Click at x,y (screenshot pixels). button is left (default) or right; double true for a double-click.",
                input_schema: obj(json!({"x": {"type": "integer"}, "y": {"type": "integer"}, "button": {"type": "string"}, "double": {"type": "boolean"}, "display": disp}), &["x", "y"]) },
            ToolSpec { name: "drag", read_only: false, description: "Press at x1,y1, move to x2,y2 and release (screenshot pixels).",
                input_schema: obj(json!({"x1": {"type": "integer"}, "y1": {"type": "integer"}, "x2": {"type": "integer"}, "y2": {"type": "integer"}, "button": {"type": "string"}, "display": disp}), &["x1", "y1", "x2", "y2"]) },
            ToolSpec { name: "scroll", read_only: false, description: "Scroll by whole notches at the pointer: positive dy up, positive dx right. On Windows 1 notch is usually 3 lines; the result says.",
                input_schema: obj(json!({"dx": {"type": "integer"}, "dy": {"type": "integer"}}), &[]) },
            ToolSpec { name: "type", read_only: false, description: "Type a run of text at the focused control.",
                input_schema: obj(json!({"text": {"type": "string"}}), &["text"]) },
            ToolSpec { name: "key", read_only: false,
                description: "Press one named key: enter, tab, escape, backspace, delete, up/down/left/right, home, end, f1-f12, and so on.",
                input_schema: obj(json!({"key": {"type": "string"}}), &["key"]) },
            ToolSpec { name: "shortcut", read_only: false, description: "Press a chord such as ctrl+c, ctrl+shift+n, alt+f4 or win+r.",
                input_schema: obj(json!({"keys": {"type": "string"}}), &["keys"]) },
            ToolSpec { name: "wait", read_only: true, description: "Pause for ms milliseconds (capped at 10000) to let the UI settle.",
                input_schema: obj(json!({"ms": {"type": "integer"}}), &["ms"]) },
            ToolSpec { name: "clipboard", read_only: false,
                description: "Without text: the last clipboard text the PC sent since the session opened (copy first with ctrl+c). With text: set the PC's clipboard.",
                input_schema: obj(json!({"text": {"type": "string"}}), &[]) },
        ]
    }

    fn local_input_after(&self, since_ms: u64) -> bool {
        self.link.foreign_move_after(since_ms)
    }

    fn call(&self, tool: &str, args: &Value) -> Result<Called, String> {
        self.run(tool, args).map(Into::into)
    }
}

impl RemoteDesk {
    fn run(&self, tool: &str, args: &Value) -> Result<Vec<Content>, String> {
        if tool == "wait" {
            let ms = args.get("ms").and_then(Value::as_u64).unwrap_or(500).min(10_000);
            std::thread::sleep(Duration::from_millis(ms));
            return ok(format!("waited {ms} ms"));
        }
        let (out, peer) = self.link.ensure(CONNECT_WAIT)?;
        let send = |msgs| self.link.inject(&out, msgs);
        match tool {
            "screenshot" => {
                let (index, x, y, w, h) = display(&peer, args)?;
                let png = self.link.screenshot(&out, index, SCREENSHOT_WAIT)?;
                Ok(vec![
                    Content::Text(format!("display {index} of {}, {w}x{h} at {x},{y}, on {} ({})", peer.displays.len(), peer.hostname, peer.platform)),
                    Content::Png(png),
                ])
            }
            "move" => {
                let (x, y) = at(&peer, args, int(args, "x")?, int(args, "y")?)?;
                send(keys::move_to(x, y))?;
                ok("moved")
            }
            "click" => {
                let (x, y) = (int(args, "x")?, int(args, "y")?);
                let (dx, dy) = at(&peer, args, x, y)?;
                let btn = args.get("button").and_then(Value::as_str).unwrap_or("left");
                let double = args.get("double").and_then(Value::as_bool).unwrap_or(false);
                send(keys::click(dx, dy, btn, double))?;
                ok(format!("clicked {x},{y}"))
            }
            "drag" => {
                let from = at(&peer, args, int(args, "x1")?, int(args, "y1")?)?;
                let to = at(&peer, args, int(args, "x2")?, int(args, "y2")?)?;
                let btn = args.get("button").and_then(Value::as_str).unwrap_or("left");
                send(keys::drag(from, to, btn))?;
                ok("dragged")
            }
            "scroll" => {
                let (dx, dy) = (int(args, "dx").unwrap_or(0), int(args, "dy").unwrap_or(0));
                send(keys::scroll(dx, dy))?;
                ok(scrolled(dx, dy, scroll_rate(&peer)))
            }
            "type" => {
                let t = text(args, "text")?;
                send(keys::type_text(&t))?;
                ok(format!("typed {} characters", t.chars().count()))
            }
            "key" => {
                send(keys::press_key(&text(args, "key")?)?)?;
                ok("pressed")
            }
            "shortcut" => {
                send(keys::shortcut(&text(args, "keys")?)?)?;
                ok("pressed")
            }
            "clipboard" => match args.get("text").and_then(Value::as_str) {
                Some(t) => {
                    self.link.set_clipboard(&out, t)?;
                    ok("clipboard set")
                }
                None => match self.link.clipboard() {
                    Some(t) => ok(t),
                    None => Err("the PC has sent no clipboard text since the session opened; copy something first (ctrl+c), then ask again".into()),
                },
            },
            other => Err(format!("no tool {other} on a stock RustDesk PC")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::scrolled;

    #[test]
    fn a_scroll_reports_its_notches_and_the_rate() {
        assert_eq!(scrolled(0, -45, "1 notch is 3 lines"), "scrolled 45 notches down (1 notch is 3 lines)");
        assert_eq!(scrolled(1, 1, "r"), "scrolled 1 notch up and 1 notch right (r)");
    }
}
