//! The Windows desktop behind the agent API: the tool catalog of acceptance 6 and the dispatch that runs each call
//! through capture, input, the UI tree and the user's shell. The share and holder rules sit in front of this in the
//! `rigdesk-agent` crate; by the time `call` runs, the agent is allowed to act.

use rigdesk_agent::{Content, Desk, ToolSpec};
use serde_json::{json, Value};

use super::{capture, input, shell, uia};

pub struct WinDesk;

impl WinDesk {
    pub fn new() -> Self {
        WinDesk
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

impl Desk for WinDesk {
    fn tools(&self) -> Vec<ToolSpec> {
        let obj = |props: Value, required: &[&str]| json!({"type": "object", "properties": props, "required": required});
        let xy = json!({"x": {"type": "integer"}, "y": {"type": "integer"}});
        vec![
            ToolSpec { name: "screenshot", read_only: true, description: "A PNG of the primary display right now.",
                input_schema: json!({"type": "object", "properties": {}}) },
            ToolSpec { name: "snapshot", read_only: true, description: "The on-screen UI Automation tree: each element's role, name, automation id and clickable centre as @x,y.",
                input_schema: json!({"type": "object", "properties": {}}) },
            ToolSpec { name: "move", read_only: false, description: "Move the pointer to absolute screen x,y.",
                input_schema: obj(xy.clone(), &["x", "y"]) },
            ToolSpec { name: "click", read_only: false, description: "Click at x,y. button is left (default) or right; double true for a double-click.",
                input_schema: obj(json!({"x": {"type": "integer"}, "y": {"type": "integer"}, "button": {"type": "string"}, "double": {"type": "boolean"}}), &["x", "y"]) },
            ToolSpec { name: "drag", read_only: false, description: "Press at x1,y1, move to x2,y2 and release. button is left (default) or right.",
                input_schema: obj(json!({"x1": {"type": "integer"}, "y1": {"type": "integer"}, "x2": {"type": "integer"}, "y2": {"type": "integer"}, "button": {"type": "string"}}), &["x1", "y1", "x2", "y2"]) },
            ToolSpec { name: "scroll", read_only: false, description: "Scroll by whole notches at the pointer: positive dy up, positive dx right.",
                input_schema: obj(json!({"dx": {"type": "integer"}, "dy": {"type": "integer"}}), &[]) },
            ToolSpec { name: "type", read_only: false, description: "Type a run of text at the focused control.",
                input_schema: obj(json!({"text": {"type": "string"}}), &["text"]) },
            ToolSpec { name: "key", read_only: false, description: "Press one named key: enter, tab, escape, backspace, delete, up/down/left/right, home, end, f1-f12, and so on.",
                input_schema: obj(json!({"key": {"type": "string"}}), &["key"]) },
            ToolSpec { name: "shortcut", read_only: false, description: "Press a chord such as ctrl+c, ctrl+shift+n or alt+f4.",
                input_schema: obj(json!({"keys": {"type": "string"}}), &["keys"]) },
            ToolSpec { name: "wait", read_only: true, description: "Pause for ms milliseconds (capped at 10000) to let the UI settle.",
                input_schema: obj(json!({"ms": {"type": "integer"}}), &["ms"]) },
            ToolSpec { name: "app", read_only: false, description: "launch: start a program by name or path. switch: bring a window whose title contains the text to the front.",
                input_schema: obj(json!({"action": {"type": "string"}, "name": {"type": "string"}}), &["action", "name"]) },
            ToolSpec { name: "powershell", read_only: false, description: "Run a PowerShell script as the signed-in user and return its output. Never runs as SYSTEM.",
                input_schema: obj(json!({"script": {"type": "string"}, "timeout_ms": {"type": "integer"}}), &["script"]) },
            ToolSpec { name: "clipboard", read_only: false, description: "get the user's clipboard text, or set it when text is given.",
                input_schema: obj(json!({"text": {"type": "string"}}), &[]) },
        ]
    }

    fn call(&self, tool: &str, args: &Value) -> Result<Vec<Content>, String> {
        match tool {
            "screenshot" => {
                let (png, w, h) = capture::primary_png().map_err(|e| e.to_string())?;
                Ok(vec![Content::Text(format!("primary display {w}x{h}")), Content::Png(png)])
            }
            "snapshot" => ok(uia::snapshot()?),
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
                ok("scrolled")
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
            "app" => app(&text(args, "action")?, &text(args, "name")?),
            "powershell" => {
                let timeout = args.get("timeout_ms").and_then(Value::as_i64).unwrap_or(30_000).clamp(1_000, 120_000) as u32;
                ok(shell::powershell(&text(args, "script")?, timeout)?)
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

fn app(action: &str, name: &str) -> Result<Vec<Content>, String> {
    match action {
        "launch" => {
            let safe = name.replace('\'', "''");
            shell::powershell(&format!("Start-Process '{safe}'"), 15_000)?;
            ok(format!("launched {name}"))
        }
        "switch" => {
            let safe = name.replace('\'', "''");
            let out = shell::powershell(&format!("(New-Object -ComObject WScript.Shell).AppActivate('{safe}')"), 10_000)?;
            ok(format!("switch requested: {}", out.trim()))
        }
        other => Err(format!("app action must be launch or switch, not {other}")),
    }
}
