//! The agent's one shell tool, `shell`: a command in; its output, exit status and whether it was cut or timed out back.
//! A RigDesk PC runs it as the signed-in user through the app; a stock RustDesk PC runs it in RustDesk's own terminal,
//! through the bridge. Both shape what they got here, so the agent sees one tool whichever way the PC runs it.

use std::time::Duration;

use serde_json::{json, Value};

use crate::desk::{Called, Content, ToolSpec};

/// What the agent is given of the output at most; the rest is cut from the middle and counted.
pub const OUTPUT_CAP: usize = 32 * 1024;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);

pub fn spec(shell: &'static str) -> ToolSpec {
    ToolSpec {
        name: "shell",
        description: shell,
        input_schema: json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "minLength": 1, "description": "The command line, as typed at the prompt."},
                "timeout_s": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT.as_secs(),
                              "description": format!("End it after this many seconds; {} by default.", DEFAULT_TIMEOUT.as_secs())},
            },
            "required": ["command"],
        }),
        read_only: false,
    }
}

/// One command as the agent asked it.
#[derive(Clone, Debug, PartialEq)]
pub struct Asked {
    pub command: String,
    pub timeout: Duration,
}

pub fn asked(args: &Value) -> Result<Asked, String> {
    let command = args.get("command").and_then(Value::as_str).filter(|c| !c.trim().is_empty()).ok_or("shell takes a command")?;
    let timeout = match args.get("timeout_s") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT,
        Some(given) => match given.as_u64() {
            Some(s) if (1..=MAX_TIMEOUT.as_secs()).contains(&s) => Duration::from_secs(s),
            _ => return Err(format!("timeout_s is a whole number of seconds from 1 to {}", MAX_TIMEOUT.as_secs())),
        },
    };
    Ok(Asked { command: command.to_owned(), timeout })
}

/// How the command ended.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ended {
    Exited(i32),
    /// It ran past its time limit and was ended.
    TimedOut(Duration),
}

/// What a command printed, as the PC sent it: all of it, or its start and end with the middle the PC already dropped.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Printed {
    pub head: Vec<u8>,
    /// Bytes the PC left out between `head` and `tail`.
    pub dropped: usize,
    pub tail: Vec<u8>,
}

impl Printed {
    pub fn whole(raw: Vec<u8>) -> Self {
        Printed { head: raw, dropped: 0, tail: Vec::new() }
    }
}

/// What the agent reads: the output with terminal codes taken out and cut to `OUTPUT_CAP`, and how it ended. A command over
/// its time limit is refused as timed out, with what it printed until then.
pub fn answer(printed: &Printed, ended: Ended) -> Result<Called, String> {
    let (output, cut) = capped(&plain(&printed.head), printed.dropped, &plain(&printed.tail), OUTPUT_CAP);
    match ended {
        Ended::Exited(code) => Ok(Called {
            contents: vec![Content::Text(json!({"exit_code": code, "output": output, "cut_bytes": cut}).to_string())],
            outcome: format!("exit {code}"),
        }),
        Ended::TimedOut(after) => Err(format!(
            "timed out: the command ran past {} s and was ended. What it printed until then{}:\n{output}",
            after.as_secs(),
            if cut > 0 { format!(" ({cut} bytes cut from the middle)") } else { String::new() }
        )),
    }
}

/// The text a terminal would show of `raw`, without its colour, cursor and title codes: escape sequences (CSI, OSC, and
/// two-byte ones) and other control characters go, tabs and line ends stay, and a carriage return that redraws a line
/// keeps only what was drawn last.
pub fn plain(raw: &[u8]) -> String {
    let text = without_escapes(&String::from_utf8_lossy(raw));
    let mut out = String::with_capacity(text.len());
    let mut line = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                out.push_str(line.trim_end());
                out.push('\n');
                line.clear();
            }
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    line.clear();
                }
            }
            '\t' => line.push('\t'),
            '\u{8}' => {
                line.pop();
            }
            c if c.is_control() => {}
            c => line.push(c),
        }
    }
    out.push_str(line.trim_end());
    out.trim_end_matches('\n').to_owned()
}

/// `text` without its terminal escape sequences: CSI, OSC and the other string sequences, and two-byte ones.
pub fn without_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                    while let Some(n) = chars.next() {
                        if n == '\u{7}' || (n == '\u{1b}' && chars.peek() == Some(&'\\')) {
                            if n == '\u{1b}' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some('(') | Some(')') | Some('*') | Some('+') => {
                    chars.next();
                }
                _ => {}
            },
            '\u{9b}' => {
                while let Some(n) = chars.next() {
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// The output within about `cap` bytes, its middle cut on character bounds and marked, and how many bytes went, counting
/// the `dropped` bytes the PC already left out between `head` and `tail`.
fn capped(head: &str, dropped: usize, tail: &str, cap: usize) -> (String, usize) {
    if dropped == 0 && head.len() + tail.len() <= cap {
        return (format!("{head}{tail}"), 0);
    }
    let floor = |text: &str, mut at: usize| {
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    if dropped == 0 {
        let text = format!("{head}{tail}");
        let kept_head = floor(&text, cap / 4);
        let from = floor(&text, text.len() - (cap - kept_head));
        let cut = from - kept_head;
        return (format!("{}\n[... {cut} bytes cut ...]\n{}", &text[..kept_head], &text[from..]), cut);
    }
    let kept_head = floor(head, head.len().min(cap / 4));
    let from = floor(tail, tail.len().saturating_sub(cap - kept_head));
    let cut = head.len() - kept_head + dropped + from;
    (format!("{}\n[... {cut} bytes cut ...]\n{}", &head[..kept_head], &tail[from..]), cut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_cursor_and_title_codes_go_and_a_redrawn_line_keeps_its_last_drawing() {
        let raw = b"\x1b]0;title\x07\x1b[1;32mgreen\x1b[0m text\r\n50%\r100%\n\x1b[?25lend\x1b[K\x1b(B";
        assert_eq!(plain(raw), "green text\n100%\nend");
    }

    #[test]
    fn output_over_the_cap_says_how_much_was_cut() {
        let text = "é".repeat(OUTPUT_CAP);
        let (kept, cut) = capped(&text, 0, "", OUTPUT_CAP);
        assert!(kept.len() < OUTPUT_CAP + 64);
        assert_eq!(cut, text.len() - (kept.len() - format!("\n[... {cut} bytes cut ...]\n").len()));
        assert!(kept.contains(&format!("[... {cut} bytes cut ...]")));
    }

    #[test]
    fn what_the_pc_already_dropped_counts_as_cut() {
        let (kept, cut) = capped(&"a".repeat(OUTPUT_CAP), 1000, &"z".repeat(OUTPUT_CAP), OUTPUT_CAP);
        assert_eq!(cut, OUTPUT_CAP + 1000);
        assert!(kept.starts_with(&"a".repeat(OUTPUT_CAP / 4)) && kept.ends_with(&"z".repeat(OUTPUT_CAP * 3 / 4)));
        assert_eq!(capped("ab", 3, "yz", OUTPUT_CAP), ("ab\n[... 3 bytes cut ...]\nyz".into(), 3));
    }

    #[test]
    fn a_failing_command_answers_its_status_and_a_slow_one_is_refused() {
        let failed = answer(&Printed::whole(b"nope\r\n".to_vec()), Ended::Exited(3)).unwrap();
        assert_eq!(failed.outcome, "exit 3");
        assert_eq!(failed.contents, vec![Content::Text(json!({"exit_code": 3, "output": "nope", "cut_bytes": 0}).to_string())]);
        let slow = answer(&Printed::whole(b"started\n".to_vec()), Ended::TimedOut(Duration::from_secs(5))).unwrap_err();
        assert!(slow.starts_with("timed out: the command ran past 5 s and was ended") && slow.ends_with("started"));
    }

    #[test]
    fn the_command_and_its_limit_are_read_at_the_boundary() {
        assert_eq!(asked(&json!({"command": "dir"})).unwrap(), Asked { command: "dir".into(), timeout: DEFAULT_TIMEOUT });
        assert!(asked(&json!({"command": " "})).is_err());
        assert!(asked(&json!({"command": "dir", "timeout_s": 0})).is_err());
        assert!(asked(&json!({"command": "dir", "timeout_s": 601})).is_err());
    }
}
