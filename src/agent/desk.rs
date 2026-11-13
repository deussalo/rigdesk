//! The Windows desktop behind the agent API: the tool catalog, the same as a stock PC's, and the dispatch that runs each
//! call through capture, input, the clipboard and the user's shell. The share and holder rules sit in front of this in the
//! `rigdesk-agent` crate; by the time `call` runs, the agent is allowed to act.

use rigdesk_agent::{Called, Content, Desk, ToolSpec};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{capture, input, shell};

const SHELL: &str = "Run a PowerShell command on the PC as the signed-in user, never as SYSTEM, and get its output and \
exit status: the code it exits with, 1 for an error it does not catch, else the last program's exit code. It reads no \
input, so nothing can wait at a prompt; output is plain text, cut from the middle past 32 KiB.";

pub struct WinDesk {
    /// When this desk last finished injecting input for the agent (wall-clock ms).
    injected_ms: AtomicU64,
}

impl WinDesk {
    pub fn new() -> Self {
        WinDesk { injected_ms: AtomicU64::new(0) }
    }
}

/// Input this soon after the agent's own is taken as the tail of it, not a person at the PC.
const OWN_INPUT_SLACK_MS: u64 = 500;

/// The wall-clock ms of the session's last mouse or keyboard input, injected or not.
fn last_input_ms() -> Option<u64> {
    use winapi::um::winuser::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return None;
    }
    let idle = unsafe { windows::Win32::System::SystemInformation::GetTickCount() }.wrapping_sub(info.dwTime);
    Some(rigdesk_agent::now_ms().saturating_sub(idle as u64))
}

/// How far one wheel notch goes on this PC, from its WheelScrollLines setting.
fn scroll_rate() -> String {
    use winapi::um::winuser::{SystemParametersInfoW, SPI_GETWHEELSCROLLLINES, WHEEL_PAGESCROLL};
    let mut lines: u32 = 0;
    if unsafe { SystemParametersInfoW(SPI_GETWHEELSCROLLLINES, 0, &mut lines as *mut u32 as *mut _, 0) } == 0 {
        return "this PC's lines per notch could not be read; apps that scroll by pixels vary".into();
    }
    match lines {
        WHEEL_PAGESCROLL => "1 notch is one page on this PC".into(),
        n => format!("1 notch is {n} line{} on this PC; apps that scroll by pixels vary", if n == 1 { "" } else { "s" }),
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

fn int(args: &Value, key: &str) -> Result<i32, String> {
    args.get(key).and_then(Value::as_i64).map(|v| v as i32).ok_or_else(|| format!("missing integer {key}"))
}

fn text(args: &Value, key: &str) -> Result<String, String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| format!("missing string {key}"))
}

fn ok(words: impl Into<String>) -> Result<Vec<Content>, String> {
    Ok(vec![Content::Text(words.into())])
}

impl Desk for WinDesk {
    fn tools(&self) -> Vec<ToolSpec> {
        let obj = |props: Value, required: &[&str]| json!({"type": "object", "properties": props, "required": required});
        let xy = json!({"x": {"type": "integer"}, "y": {"type": "integer"}});
        vec![
            ToolSpec { name: "screenshot", read_only: true, description: "A PNG of the primary display right now.",
                input_schema: json!({"type": "object", "properties": {}}) },
            ToolSpec { name: "move", read_only: false, description: "Move the pointer to absolute screen x,y.",
                input_schema: obj(xy.clone(), &["x", "y"]) },
            ToolSpec { name: "click", read_only: false, description: "Click at x,y. button is left (default) or right; double true for a double-click.",
                input_schema: obj(json!({"x": {"type": "integer"}, "y": {"type": "integer"}, "button": {"type": "string"}, "double": {"type": "boolean"}}), &["x", "y"]) },
            ToolSpec { name: "drag", read_only: false, description: "Press at x1,y1, move to x2,y2 and release. button is left (default) or right.",
                input_schema: obj(json!({"x1": {"type": "integer"}, "y1": {"type": "integer"}, "x2": {"type": "integer"}, "y2": {"type": "integer"}, "button": {"type": "string"}}), &["x1", "y1", "x2", "y2"]) },
            ToolSpec { name: "scroll", read_only: false, description: "Scroll by whole notches at the pointer: positive dy up, positive dx right. The result says how many lines a notch is on this PC.",
                input_schema: obj(json!({"dx": {"type": "integer"}, "dy": {"type": "integer"}}), &[]) },
            ToolSpec { name: "type", read_only: false, description: "Type a run of text at the focused control.",
                input_schema: obj(json!({"text": {"type": "string"}}), &["text"]) },
            ToolSpec { name: "key", read_only: false, description: "Press one named key: enter, tab, escape, backspace, delete, up/down/left/right, home, end, f1-f12, and so on.",
                input_schema: obj(json!({"key": {"type": "string"}}), &["key"]) },
            ToolSpec { name: "shortcut", read_only: false, description: "Press a chord such as ctrl+c, ctrl+shift+n or alt+f4.",
                input_schema: obj(json!({"keys": {"type": "string"}}), &["keys"]) },
            ToolSpec { name: "wait", read_only: true, description: "Pause for ms milliseconds (capped at 10000) to let the UI settle.",
                input_schema: obj(json!({"ms": {"type": "integer"}}), &["ms"]) },
            ToolSpec { name: "clipboard", read_only: false, description: "get the user's clipboard text, or set it when text is given.",
                input_schema: obj(json!({"text": {"type": "string"}}), &[]) },
            rigdesk_agent::shell::spec(SHELL),
        ]
    }

    fn local_input_after(&self, since_ms: u64) -> bool {
        let quiet_from = since_ms.max(self.injected_ms.load(Ordering::SeqCst)) + OWN_INPUT_SLACK_MS;
        last_input_ms().map_or(false, |at| at > quiet_from)
    }

    fn call(&self, tool: &str, args: &Value) -> Result<Called, String> {
        if tool == "shell" {
            return shell::run(&rigdesk_agent::shell::asked(args)?);
        }
        let result = self.run(tool, args);
        if !matches!(tool, "screenshot" | "wait") {
            self.injected_ms.store(rigdesk_agent::now_ms(), Ordering::SeqCst);
        }
        result.map(Into::into)
    }
}

impl WinDesk {
    fn run(&self, tool: &str, args: &Value) -> Result<Vec<Content>, String> {
        match tool {
            "screenshot" => {
                let (png, w, h) = capture::primary_png().map_err(|e| e.to_string())?;
                Ok(vec![Content::Text(format!("primary display {w}x{h}")), Content::Png(png)])
            }
            "move" => {
                input::move_to(int(args, "x")?, int(args, "y")?);
                ok("moved")
            }
            "click" => {
                let (x, y) = (int(args, "x")?, int(args, "y")?);
                let btn = args.get("button").and_then(Value::as_str).unwrap_or("left");
                let double = args.get("double").and_then(Value::as_bool).unwrap_or(false);
                input::click(x, y, btn, double);
                ok(format!("clicked {x},{y}"))
            }
            "drag" => {
                let btn = args.get("button").and_then(Value::as_str).unwrap_or("left");
                input::drag((int(args, "x1")?, int(args, "y1")?), (int(args, "x2")?, int(args, "y2")?), btn);
                ok("dragged")
            }
            "scroll" => {
                let dx = args.get("dx").and_then(Value::as_i64).unwrap_or(0) as i32;
                let dy = args.get("dy").and_then(Value::as_i64).unwrap_or(0) as i32;
                input::scroll(dx, dy);
                ok(scrolled(dx, dy, &scroll_rate()))
            }
            "type" => {
                input::type_text(&text(args, "text")?);
                ok("typed")
            }
            "key" => {
                input::press_key(&text(args, "key")?)?;
                ok("pressed")
            }
            "shortcut" => {
                input::shortcut(&text(args, "keys")?)?;
                ok("pressed")
            }
            "wait" => {
                let ms = args.get("ms").and_then(Value::as_i64).unwrap_or(0).clamp(0, 10_000) as u64;
                std::thread::sleep(std::time::Duration::from_millis(ms));
                ok(format!("waited {ms}ms"))
            }
            "clipboard" => match args.get("text").and_then(Value::as_str) {
                Some(t) => {
                    shell::clipboard_set(t)?;
                    ok("clipboard set")
                }
                None => ok(shell::clipboard_get()?),
            },
            other => Err(format!("no such tool: {other}")),
        }
    }
}
