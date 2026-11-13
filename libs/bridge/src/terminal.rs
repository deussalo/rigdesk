//! A stock PC's shell. Each command gets its own RustDesk terminal session beside the screen session: the bridge logs in
//! for a terminal, opens one, and types a short runner at the prompt. The runner runs the command with no input and a
//! time limit, its output kept in a private temp directory, then prints the exit status and that output in base64
//! between markers, and the shell exits. Base64 carries the output whole through the PC's pseudo-terminal, which wraps
//! and, on Windows, redraws what it shows; the runner is typed only as base64, so the echo of the typed lines never
//! holds a marker.

use std::time::Duration;

use base::message_proto::{
    login_request, login_response, message, misc, option_message::BoolOption, terminal_action, terminal_response,
    CloseTerminal, Message, OpenTerminal, OptionMessage, Terminal, TerminalAction, TerminalData,
};
use hbb_common::base64::{engine::general_purpose::STANDARD as B64, Engine};
use hbb_common::protobuf::Message as _;
use hbb_common::tcp::FramedStream;
use hbb_common::tokio::time::{sleep, timeout, Instant};
use rigdesk_agent::shell::{self, Asked, Ended, Printed};
use rigdesk_agent::Called;

use crate::link::Link;

const TERMINAL_ID: i32 = 1;
const ROWS: u32 = 50;
const COLS: u32 = 240;
const LOGIN_WAIT: Duration = Duration::from_secs(20);
/// The prompt is taken as drawn once the shell has been quiet this long, or after `PROMPT_WAIT` whatever it shows.
const PROMPT_QUIET: Duration = Duration::from_millis(500);
const PROMPT_WAIT: Duration = Duration::from_secs(15);
/// How long past the command's own limit the PC has to end it and send what it printed.
const REPORT_GRACE: Duration = Duration::from_secs(20);
/// The PC sends at most this much of the start and of the end of the output; the middle is counted, not sent.
const PART: usize = 64 * 1024;
/// Typed lines stay well under a pseudo-terminal's line buffer (4 KiB on Linux).
const LINE: usize = 1000;

/// The shell RustDesk opens: PowerShell on Windows, the user's POSIX shell elsewhere.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shell {
    Posix,
    PowerShell,
}

impl Shell {
    pub fn of(platform: &str) -> Self {
        if platform == "Windows" {
            Shell::PowerShell
        } else {
            Shell::Posix
        }
    }
}

/// The lines typed at the prompt, each followed by Enter: the runner in base64 chunks, then the line that runs it.
pub fn typed(shell: Shell, command: &str, limit: Duration, nonce: &str) -> Vec<String> {
    let encoded = B64.encode(runner(shell, command, limit, nonce));
    let chunks: Vec<&str> = encoded.as_bytes().chunks(LINE).map(|c| std::str::from_utf8(c).unwrap_or_default()).collect();
    let mut lines = Vec::with_capacity(chunks.len() + 2);
    match shell {
        Shell::Posix => {
            // Whatever the login shell, the rest is typed into sh, which keeps no history.
            lines.push("exec /bin/sh".to_string());
            for (i, chunk) in chunks.iter().enumerate() {
                lines.push(if i == 0 { format!("r='{chunk}'") } else { format!("r=\"$r\"'{chunk}'") });
            }
            lines.push("printf %s \"$r\" | base64 -d | sh; exit".to_string());
        }
        Shell::PowerShell => {
            lines.push("if (Get-Command Set-PSReadLineOption -EA 0) { Set-PSReadLineOption -HistorySaveStyle SaveNothing }".to_string());
            for (i, chunk) in chunks.iter().enumerate() {
                lines.push(if i == 0 { format!("$r='{chunk}'") } else { format!("$r+='{chunk}'") });
            }
            lines.push("iex ([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($r))); exit".to_string());
        }
    }
    lines
}

fn runner(shell: Shell, command: &str, limit: Duration, nonce: &str) -> String {
    let secs = limit.as_secs().max(1);
    match shell {
        Shell::Posix => POSIX_RUNNER
            .replace("@COMMAND@", &B64.encode(command))
            .replace("@LIMIT@", &secs.to_string())
            .replace("@PART@", &PART.to_string())
            .replace("@NONCE@", nonce),
        Shell::PowerShell => {
            // Windows PowerShell reads a script without a byte-order mark in the ANSI code page.
            let mut script = vec![0xef, 0xbb, 0xbf];
            script.extend_from_slice(command.as_bytes());
            POWERSHELL_RUNNER
                .replace("@COMMAND@", &B64.encode(script))
                .replace("@LIMIT_MS@", &(secs * 1000).to_string())
                .replace("@PART@", &PART.to_string())
                .replace("@NONCE@", nonce)
        }
    }
}

// The command runs in the user's shell from a file, so it may be any number of lines. `timeout` (GNU coreutils) ends
// the command's whole process group; without it, the command alone is ended.
const POSIX_RUNNER: &str = r##"d=$(mktemp -d) || exit 97
printf %s '@COMMAND@' | base64 -d >"$d/c" || exit 97
s0=$(date +%s)
if command -v timeout >/dev/null 2>&1; then
  timeout -k 5 @LIMIT@ "${SHELL:-/bin/sh}" "$d/c" </dev/null >"$d/o" 2>&1; s=$?
else
  "${SHELL:-/bin/sh}" "$d/c" </dev/null >"$d/o" 2>&1 & p=$!
  ( sleep @LIMIT@; kill -TERM $p; sleep 5; kill -KILL $p ) >/dev/null 2>&1 & w=$!
  wait $p; s=$?
  kill $w 2>/dev/null
fi
t=0
case $s in 124|137|143) [ $(( $(date +%s) - s0 )) -ge @LIMIT@ ] && t=1 ;; esac
n=$(wc -c <"$d/o" | tr -d ' ')
printf '\n#RIG@NONCE@.%s.%s.%s#' "$s" "$t" "$n"
if [ "$n" -gt $((2 * @PART@)) ]; then
  head -c @PART@ "$d/o" | base64; printf '#RIG@NONCE@.tail#'; tail -c @PART@ "$d/o" | base64
else
  base64 <"$d/o"; printf '#RIG@NONCE@.tail#'
fi
printf '#RIG@NONCE@.end#\n'
rm -rf "$d"
"##;

// The command runs in a fresh PowerShell from a file in the user's own temp directory, behind cmd, which writes its
// output and errors to one file in the order they came. Its status: the code it exits with, 1 for an error it does not
// catch, else the exit code of the last program it ran (0 when none). Over its limit, its whole process tree is ended.
const POWERSHELL_RUNNER: &str = r##"try {
$ErrorActionPreference = 'Stop'
$d = Join-Path ([IO.Path]::GetTempPath()) 'rig-@NONCE@'
New-Item -ItemType Directory -Path $d -Force | Out-Null
[IO.File]::WriteAllBytes("$d\c.ps1", [Convert]::FromBase64String('@COMMAND@'))
[IO.File]::WriteAllText("$d\w.ps1", @'
[Console]::OutputEncoding = New-Object Text.UTF8Encoding $false
$global:LASTEXITCODE = 0
try { . "$PSScriptRoot\c.ps1" } catch { [Console]::Error.WriteLine(($_ | Out-String)); exit 1 }
exit $LASTEXITCODE
'@)
$exe = (Get-Process -Id $PID).Path
$a = '/d /c ""' + $exe + '" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + $d + '\w.ps1" > "' + $d + '\o" 2>&1"'
$p = Start-Process -FilePath $env:ComSpec -ArgumentList $a -WindowStyle Hidden -PassThru
$null = $p.Handle
$t = 0
if (-not $p.WaitForExit(@LIMIT_MS@)) { & taskkill.exe /T /F /PID $p.Id 2>&1 | Out-Null; $null = $p.WaitForExit(5000); $t = 1 }
$s = $p.ExitCode
$o = "$d\o"
$n = 0
if (Test-Path $o) { $n = (Get-Item $o).Length }
function Part($at, $len) {
  if ($len -le 0) { return '' }
  $f = [IO.File]::OpenRead($o)
  try { $b = New-Object byte[] $len; $null = $f.Seek($at, 0); $m = $f.Read($b, 0, $len); [Convert]::ToBase64String($b, 0, $m) } finally { $f.Close() }
}
if ($n -gt 2 * @PART@) { $h = Part 0 @PART@; $tl = Part ($n - @PART@) @PART@ } else { $h = Part 0 $n; $tl = '' }
$out = "#RIG@NONCE@.$s.$t.$n#" + $h + '#RIG@NONCE@.tail#' + $tl + '#RIG@NONCE@.end#'
} catch {
$out = '#RIG@NONCE@.x.0.0#' + [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes(($_ | Out-String))) + '#RIG@NONCE@.tail##RIG@NONCE@.end#'
}
[Console]::Out.WriteLine('')
for ($i = 0; $i -lt $out.Length; $i += 1000) { [Console]::Out.WriteLine($out.Substring($i, [Math]::Min(1000, $out.Length - $i))) }
if ($d) { Remove-Item -Recurse -Force $d -EA 0 }
"##;

/// What the terminal showed, with escape sequences, control characters and whitespace taken out: the runner's report
/// survives this whole, wherever the terminal wrapped or redrew it.
fn bare(raw: &[u8]) -> String {
    shell::without_escapes(&String::from_utf8_lossy(raw)).chars().filter(|c| !c.is_whitespace() && !c.is_control()).collect()
}

/// The runner's report in what the terminal showed so far, once it is all there.
pub fn report(shown: &[u8], nonce: &str, limit: Duration) -> Option<Result<(Printed, Ended), String>> {
    let shown = bare(shown);
    let open = format!("#RIG{nonce}.");
    let rest = &shown[shown.find(&open)? + open.len()..];
    let body = &rest[..rest.find(&format!("#RIG{nonce}.end#"))?];
    Some(parse_report(body, nonce, limit))
}

fn parse_report(body: &str, nonce: &str, limit: Duration) -> Result<(Printed, Ended), String> {
    let garbled = || format!("the PC's report came back garbled: {body:.200}");
    let (fields, data) = body.split_once('#').ok_or_else(garbled)?;
    let (head, tail) = data.split_once(&format!("#RIG{nonce}.tail#")).ok_or_else(garbled)?;
    let decode = |part: &str| B64.decode(part).map_err(|e| format!("the PC's report came back garbled ({e})"));
    let (head, tail) = (decode(head)?, decode(tail)?);
    let mut fields = fields.split('.');
    let (status, timed_out, total) = (fields.next(), fields.next(), fields.next());
    if status == Some("x") {
        return Err(format!("the PC could not run the command: {}", shell::plain(&head)));
    }
    let status: i32 = status.and_then(|s| s.parse().ok()).ok_or_else(garbled)?;
    let total: usize = total.and_then(|s| s.parse().ok()).ok_or_else(garbled)?;
    let dropped = total.saturating_sub(head.len() + tail.len());
    let ended = if timed_out == Some("1") { Ended::TimedOut(limit) } else { Ended::Exited(status) };
    Ok((Printed { head, dropped, tail }, ended))
}

fn terminal(action: terminal_action::Union) -> Message {
    let mut msg = Message::new();
    msg.set_terminal_action(TerminalAction { union: Some(action), ..Default::default() });
    msg
}

/// One message from the PC before `until`; `None` once it passes.
async fn next(stream: &mut FramedStream, until: Instant) -> Result<Option<Message>, String> {
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        let bytes = match timeout(left, stream.next()).await {
            Err(_) => return Ok(None),
            Ok(None) => return Err("the PC closed the terminal connection".into()),
            Ok(Some(Err(e))) => return Err(format!("terminal connection error: {e}")),
            Ok(Some(Ok(bytes))) => bytes,
        };
        let Ok(msg) = Message::parse_from_bytes(&bytes) else { continue };
        match &msg.union {
            Some(message::Union::TestDelay(t)) if !t.from_client => {
                let mut reply = Message::new();
                reply.set_test_delay(t.clone());
                stream.send(&reply).await.map_err(|e| format!("terminal connection error: {e}"))?;
            }
            Some(message::Union::Misc(m)) => {
                if let Some(misc::Union::CloseReason(reason)) = &m.union {
                    return Err(format!("the PC closed the terminal: {reason}"));
                }
            }
            Some(message::Union::MessageBox(mb)) if mb.msgtype.contains("error") => {
                return Err(format!("the PC refused the terminal: {} {}", mb.title, mb.text));
            }
            _ => return Ok(Some(msg)),
        }
    }
}

async fn send(stream: &mut FramedStream, msg: &Message) -> Result<(), String> {
    stream.send(msg).await.map_err(|e| format!("terminal connection error: {e}"))
}

/// The end of what the terminal showed, for an answer that has nothing better to say.
fn last_shown(shown: &[u8]) -> String {
    let text = shell::plain(shown);
    let from = text.char_indices().rev().nth(800).map(|(i, _)| i).unwrap_or(0);
    text[from..].to_string()
}

/// Run one command in a new terminal on the PC and answer what it printed and how it ended.
pub async fn run(link: &Link, asked: &Asked) -> Result<Called, String> {
    let mut stream = link.dial().await.map_err(|e| format!("cannot reach the PC at {}: {e}", link.label()))?;
    let platform = log_in(link, &mut stream).await?;
    send(&mut stream, &terminal(terminal_action::Union::Open(OpenTerminal { terminal_id: TERMINAL_ID, rows: ROWS, cols: COLS, ..Default::default() })))
        .await?;
    let nonce = format!("{:016x}", hbb_common::rand::random::<u64>());
    let result = converse(&mut stream, Shell::of(&platform), asked, &nonce).await;
    let _ = send(&mut stream, &terminal(terminal_action::Union::Close(CloseTerminal { terminal_id: TERMINAL_ID, ..Default::default() }))).await;
    let (printed, ended) = result?;
    shell::answer(&printed, ended)
}

/// The terminal login; answers the PC's platform.
async fn log_in(link: &Link, stream: &mut FramedStream) -> Result<String, String> {
    let until = Instant::now() + LOGIN_WAIT;
    loop {
        let Some(msg) = next(stream, until).await? else {
            return Err(format!("the PC at {} did not log the terminal in within {} s", link.label(), LOGIN_WAIT.as_secs()));
        };
        match msg.union {
            Some(message::Union::Hash(hash)) => {
                let option = OptionMessage { terminal_persistent: BoolOption::No.into(), ..Default::default() };
                send(stream, &link.login(&hash, option, Some(login_request::Union::Terminal(Terminal::new())))).await?;
            }
            Some(message::Union::LoginResponse(res)) => match res.union {
                Some(login_response::Union::Error(e)) => return Err(format!("the PC refused the terminal: {e}")),
                Some(login_response::Union::PeerInfo(pi)) => return Ok(pi.platform),
                _ => {}
            },
            _ => {}
        }
    }
}

/// Waits for the prompt, types the runner and reads until its report.
async fn converse(stream: &mut FramedStream, shell: Shell, asked: &Asked, nonce: &str) -> Result<(Printed, Ended), String> {
    let mut shown: Vec<u8> = Vec::new();
    let mut opened = false;
    let mut last_data: Option<Instant> = None;
    let prompt_by = Instant::now() + PROMPT_WAIT;
    let mut report_by: Option<Instant> = None;
    loop {
        let until = match (report_by, last_data) {
            (Some(by), _) => by,
            (None, Some(at)) if opened => (at + PROMPT_QUIET).min(prompt_by),
            (None, _) => prompt_by,
        };
        let Some(msg) = next(stream, until).await? else {
            if report_by.is_some() {
                return Err(format!(
                    "the PC did not report back within {} s of the command's {} s limit; the terminal last showed:\n{}",
                    REPORT_GRACE.as_secs(),
                    asked.timeout.as_secs(),
                    last_shown(&shown)
                ));
            }
            if !opened {
                return Err(format!("the PC did not open a terminal within {} s", PROMPT_WAIT.as_secs()));
            }
            let lines = typed(shell, &asked.command, asked.timeout, nonce);
            let total = lines.len();
            for (i, line) in lines.into_iter().enumerate() {
                let data = TerminalData { terminal_id: TERMINAL_ID, data: format!("{line}\r").into_bytes().into(), ..Default::default() };
                send(stream, &terminal(terminal_action::Union::Data(data))).await?;
                // The first line may change shells, and a new shell reads only what arrives after it starts.
                sleep(Duration::from_millis(if i == 0 && total > 1 { 400 } else { 15 })).await;
            }
            report_by = Some(Instant::now() + asked.timeout + REPORT_GRACE);
            continue;
        };
        let Some(message::Union::TerminalResponse(res)) = msg.union else { continue };
        match res.union {
            Some(terminal_response::Union::Opened(o)) => {
                if !o.success {
                    return Err(format!("the PC would not open a terminal: {}", o.message));
                }
                opened = true;
            }
            Some(terminal_response::Union::Data(d)) => {
                let data = if d.compressed { hbb_common::compress::decompress(&d.data) } else { d.data.to_vec() };
                last_data = Some(Instant::now());
                let ends_marker = data.contains(&b'#');
                shown.extend_from_slice(&data);
                if report_by.is_some() && ends_marker {
                    if let Some(found) = report(&shown, nonce, asked.timeout) {
                        return found;
                    }
                }
            }
            Some(terminal_response::Union::Closed(c)) => {
                if let Some(found) = report(&shown, nonce, asked.timeout) {
                    return found;
                }
                return Err(format!(
                    "the PC's shell ended (exit {}) before the command reported back; the terminal last showed:\n{}",
                    c.exit_code,
                    last_shown(&shown)
                ));
            }
            Some(terminal_response::Union::Error(e)) => return Err(format!("the PC's terminal failed: {}", e.message)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_typed_lines_never_hold_a_marker_and_stay_short() {
        let command = "echo #RIGabc.1.0.5# && ".repeat(200);
        for shell in [Shell::Posix, Shell::PowerShell] {
            let lines = typed(shell, &command, Duration::from_secs(5), "abc");
            assert!(lines.iter().all(|l| l.len() < LINE + 20));
            assert!(!bare(lines.join("\r\n").as_bytes()).contains("#RIGabc."));
        }
    }

    #[test]
    fn the_report_is_read_through_wrapping_and_redrawing() {
        let out = B64.encode("hello\r\nworld\n");
        let (a, b) = out.split_at(5);
        let shown = format!("$ printf ...\r\n\x1b[?25l#RIGn1.3.0.13#{a}\r\n\x1b[2K{b}#RIGn1.ta\x1b[0mil##RIGn1.end#\r\n$ ");
        let (printed, ended) = report(shown.as_bytes(), "n1", Duration::from_secs(9)).unwrap().unwrap();
        assert_eq!(ended, Ended::Exited(3));
        assert_eq!(printed, Printed::whole(b"hello\r\nworld\n".to_vec()));
        assert!(report(b"#RIGn1.3.0.13#aGk=#RIGn1.tail#", "n1", Duration::from_secs(9)).is_none());
    }

    #[test]
    fn a_long_output_comes_as_its_ends_and_a_count_and_a_time_out_says_so() {
        let shown = format!("#RIGn.-1.1.1000#{}#RIGn.tail#{}#RIGn.end#", B64.encode("start"), B64.encode("end"));
        let (printed, ended) = report(shown.as_bytes(), "n", Duration::from_secs(9)).unwrap().unwrap();
        assert_eq!(ended, Ended::TimedOut(Duration::from_secs(9)));
        assert_eq!(printed, Printed { head: b"start".to_vec(), dropped: 992, tail: b"end".to_vec() });
        let failed = format!("#RIGn.x.0.0#{}#RIGn.tail##RIGn.end#", B64.encode("Access denied"));
        assert_eq!(report(failed.as_bytes(), "n", Duration::from_secs(9)).unwrap().unwrap_err(), "the PC could not run the command: Access denied");
    }

    #[test]
    fn the_posix_runner_runs_and_reports() {
        let command = "echo out; echo err >&2; printf '\\033[31mred\\033[0m\\n'; exit 4";
        let script = runner(Shell::Posix, command, Duration::from_secs(5), "t1");
        let ran = std::process::Command::new("sh").arg("-c").arg(&script).output().unwrap();
        let (printed, ended) = report(&ran.stdout, "t1", Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(ended, Ended::Exited(4));
        assert_eq!(shell::plain(&printed.head), "out\nerr\nred");
    }

    #[test]
    fn the_posix_runner_ends_a_slow_command() {
        let script = runner(Shell::Posix, "echo begun; sleep 30", Duration::from_secs(1), "t2");
        let started = std::time::Instant::now();
        let ran = std::process::Command::new("sh").arg("-c").arg(&script).output().unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        let (printed, ended) = report(&ran.stdout, "t2", Duration::from_secs(1)).unwrap().unwrap();
        assert_eq!(ended, Ended::TimedOut(Duration::from_secs(1)));
        assert_eq!(shell::plain(&printed.head), "begun");
    }
}
