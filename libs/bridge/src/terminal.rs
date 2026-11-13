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
use hbb_common::log;
use hbb_common::protobuf::Message as _;
use hbb_common::tcp::FramedStream;
use hbb_common::tokio::time::{sleep, timeout, Instant};
use rigdesk_agent::shell::{self, Asked, Ended, Printed, OUTPUT_CAP};
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
/// How much of a long output's start and end the report carries: enough for `shell::answer`'s cap (a quarter from the
/// start, three from the end) with room for escape codes it strips. A terminal shows a report at only about 2 KB/s
/// (seen on deus over the tailnet), so it carries no more.
const HEAD_PART: usize = OUTPUT_CAP * 3 / 8;
const TAIL_PART: usize = OUTPUT_CAP * 7 / 8;
/// Typed lines stay well under a pseudo-terminal's line buffer (4 KiB on Linux).
const LINE: usize = 1000;
/// The report's base64 comes in numbered lines of this many characters, narrower than the terminal: Windows' console
/// redraws wrapped lines as it scrolls, repeating parts of them, so each line is taken once, by its number and length.
const REPORT_LINE: usize = 152;
/// Windows' console loses some of a long report's lines as it scrolls (seen on deus: line 18 of 252, at 20000 lines of
/// output); the PowerShell runner prints lines named back to it again, for this many rounds of at most `REPAIR_ASK` lines.
const REPAIR_ROUNDS: usize = 5;
const REPAIR_ASK: usize = 60;

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
pub fn typed_lines(shell: Shell, command: &str, limit: Duration, nonce: &str) -> Vec<String> {
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
            .replace("@HEAD@", &HEAD_PART.to_string())
            .replace("@TAIL@", &TAIL_PART.to_string())
            .replace("@LINE@", &REPORT_LINE.to_string())
            .replace("@NONCE@", nonce),
        Shell::PowerShell => {
            // Windows PowerShell reads a script without a byte-order mark in the ANSI code page.
            let mut script = vec![0xef, 0xbb, 0xbf];
            script.extend_from_slice(command.as_bytes());
            POWERSHELL_RUNNER
                .replace("@COMMAND@", &B64.encode(script))
                .replace("@WRAPPER@", shell::POWERSHELL_WRAPPER)
                .replace("@LIMIT_MS@", &(secs * 1000).to_string())
                .replace("@HEAD@", &HEAD_PART.to_string())
                .replace("@TAIL@", &TAIL_PART.to_string())
                .replace("@LINE@", &REPORT_LINE.to_string())
                .replace("@ROUNDS@", &REPAIR_ROUNDS.to_string())
                .replace("@NONCE@", nonce)
        }
    }
}

// The report gives the status, whether the limit ended the command and the output's size, then the output's start (all
// of it when short) and its end in base64, in lines numbered from 0 per part.
//
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
lines() { base64 | tr -d '\n' | fold -w @LINE@ | awk '{ printf "~%d~%s\n", NR - 1, $0 }'; }
printf '\n#RIG@NONCE@.%s.%s.%s#\n' "$s" "$t" "$n"
if [ "$n" -gt $((@HEAD@ + @TAIL@)) ]; then
  head -c @HEAD@ "$d/o" | lines; printf '#RIG@NONCE@.tail#\n'; tail -c @TAIL@ "$d/o" | lines
else
  lines <"$d/o"; printf '#RIG@NONCE@.tail#\n'
fi
printf '#RIG@NONCE@.end#\n'
rm -rf "$d"
"##;

// The command runs in a fresh PowerShell (see `shell::POWERSHELL_WRAPPER`) from a file in the user's own temp directory,
// behind cmd, which writes its output and errors to one file in the order they came. Over its limit, its whole process
// tree is ended. After the report, the runner reads up to @ROUNDS@ lines naming report lines the terminal lost, such as
// `h18 t5`, and prints each again as `~h18~<base64>`, then the end marker; an empty line or `ok` ends it.
const POWERSHELL_RUNNER: &str = r##"try {
$ErrorActionPreference = 'Stop'
$d = Join-Path ([IO.Path]::GetTempPath()) 'rig-@NONCE@'
New-Item -ItemType Directory -Path $d -Force | Out-Null
[IO.File]::WriteAllBytes("$d\c.ps1", [Convert]::FromBase64String('@COMMAND@'))
[IO.File]::WriteAllText("$d\w.ps1", @'
@WRAPPER@'@)
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
  $f = [IO.File]::Open($o, 'Open', 'Read', 'ReadWrite')
  try { $b = New-Object byte[] $len; $null = $f.Seek($at, 0); $m = $f.Read($b, 0, $len); [Convert]::ToBase64String($b, 0, $m) } finally { $f.Close() }
}
if ($n -gt @HEAD@ + @TAIL@) { $h = Part 0 @HEAD@; $tl = Part ($n - @TAIL@) @TAIL@ } else { $h = Part 0 $n; $tl = '' }
$top = "#RIG@NONCE@.$s.$t.$n#"
} catch {
$m = [Text.Encoding]::UTF8.GetBytes(($_ | Out-String))
$top = "#RIG@NONCE@.x.0.$($m.Length)#"; $h = [Convert]::ToBase64String($m); $tl = ''
}
if ($d) { Remove-Item -Recurse -Force $d -EA 0 }
function Lines($b64) {
  for ($i = 0; $i * @LINE@ -lt $b64.Length; $i++) { [Console]::Out.WriteLine('~' + $i + '~' + $b64.Substring($i * @LINE@, [Math]::Min(@LINE@, $b64.Length - $i * @LINE@))) }
}
[Console]::Out.WriteLine('')
[Console]::Out.WriteLine($top)
Lines $h
[Console]::Out.WriteLine('#RIG@NONCE@.tail#')
Lines $tl
[Console]::Out.WriteLine('#RIG@NONCE@.end#')
function Ask {
  $q = ''; $by = [DateTime]::Now.AddSeconds(60)
  while ([DateTime]::Now -lt $by) {
    while ([Console]::KeyAvailable) { $c = [Console]::ReadKey($true); if ($c.Key -eq 'Enter') { return $q }; $q += $c.KeyChar }
    Start-Sleep -Milliseconds 50
  }
  ''
}
for ($k = 0; $k -lt @ROUNDS@; $k++) {
  $q = Ask
  if (-not $q -or $q -eq 'ok') { break }
  foreach ($w in ($q -split ' ' | Where-Object { $_.Length -gt 1 })) {
    $b = if ($w[0] -eq 'h') { $h } elseif ($w[0] -eq 't') { $tl } else { '' }
    $i = 0
    if ($b -and [int]::TryParse($w.Substring(1), [ref]$i) -and $i * @LINE@ -lt $b.Length) {
      [Console]::Out.WriteLine('~' + $w[0] + $i + '~' + $b.Substring($i * @LINE@, [Math]::Min(@LINE@, $b.Length - $i * @LINE@)))
    }
  }
  [Console]::Out.WriteLine('#RIG@NONCE@.end#')
}
"##;

/// What the terminal showed, with escape sequences, control characters and whitespace taken out: the runner's report
/// survives this whole, wherever the terminal wrapped or redrew it.
fn bare(raw: &[u8]) -> String {
    shell::without_escapes(&String::from_utf8_lossy(raw)).chars().filter(|c| !c.is_whitespace() && !c.is_control()).collect()
}

/// Which part of the output a report line belongs to, and the letter that names it when asked again.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Part {
    Head,
    Tail,
}

impl Part {
    fn tag(self) -> char {
        match self {
            Part::Head => 'h',
            Part::Tail => 't',
        }
    }
}

/// Why a complete report gave no answer.
#[derive(Debug, PartialEq)]
enum Unread {
    /// These report lines never showed whole; the PowerShell runner prints them again when named.
    Missing(Vec<(Part, usize)>),
    /// Anything else, said as the answer.
    Failed(String),
}

impl Unread {
    /// The line typed back to the runner to ask for the missing lines.
    fn ask(lines: &[(Part, usize)]) -> String {
        lines.iter().take(REPAIR_ASK).map(|(part, n)| format!("{}{n}", part.tag())).collect::<Vec<_>>().join(" ")
    }

    fn said(self) -> String {
        match self {
            Unread::Missing(lines) => format!("the PC's report came back garbled: {} lines are missing ({})", lines.len(), Unread::ask(&lines)),
            Unread::Failed(why) => why,
        }
    }
}

/// The runner's report in what the terminal showed so far, once it is all there, with the lines printed again after it.
fn report(shown: &[u8], nonce: &str, limit: Duration) -> Option<Result<(Printed, Ended), Unread>> {
    let shown = bare(shown);
    let open = format!("#RIG{nonce}.");
    let end = format!("#RIG{nonce}.end#");
    let rest = &shown[shown.find(&open)? + open.len()..];
    let at = rest.find(&end)?;
    Some(parse_report(&rest[..at], &rest[at..], nonce, limit))
}

fn parse_report(body: &str, again: &str, nonce: &str, limit: Duration) -> Result<(Printed, Ended), Unread> {
    let garbled = || Unread::Failed(format!("the PC's report came back garbled: {body:.200}"));
    let (fields, data) = body.split_once('#').ok_or_else(garbled)?;
    let (head, tail) = data.split_once(&format!("#RIG{nonce}.tail#")).ok_or_else(garbled)?;
    let mut fields = fields.split('.');
    let (status, timed_out, total) = (fields.next(), fields.next(), fields.next());
    let total: usize = total.and_then(|s| s.parse().ok()).ok_or_else(garbled)?;
    let (head_bytes, tail_bytes) = if total > HEAD_PART + TAIL_PART { (HEAD_PART, TAIL_PART) } else { (total, 0) };
    let (head, tail) = match (lines(head, again, Part::Head, head_bytes), lines(tail, again, Part::Tail, tail_bytes)) {
        (Ok(head), Ok(tail)) => (decode(head)?, decode(tail)?),
        (head, tail) => {
            let missing = [head.err(), tail.err()].into_iter().flatten().flatten().collect();
            return Err(Unread::Missing(missing));
        }
    };
    if status == Some("x") {
        return Err(Unread::Failed(format!("the PC could not run the command: {}", shell::plain(&head))));
    }
    let status: i32 = status.and_then(|s| s.parse().ok()).ok_or_else(garbled)?;
    let dropped = total.saturating_sub(head.len() + tail.len());
    let ended = if timed_out == Some("1") { Ended::TimedOut(limit) } else { Ended::Exited(status) };
    Ok((Printed { head, dropped, tail }, ended))
}

fn decode(encoded: String) -> Result<Vec<u8>, Unread> {
    B64.decode(encoded).map_err(|e| Unread::Failed(format!("the PC's report came back garbled ({e})")))
}

/// One part's base64 from its numbered lines, `~<n>~<base64>`, as the terminal showed them, and from the lines printed
/// again after the report, `~h<n>~<base64>` or `~t<n>~<base64>`: a line may show more than once, or cut short by a
/// redraw, so each number takes the first showing of the length that line must have. Answers the numbers never found.
fn lines(shown: &str, again: &str, part: Part, bytes: usize) -> Result<String, Vec<(Part, usize)>> {
    let encoded = 4 * ((bytes + 2) / 3);
    let count = (encoded + REPORT_LINE - 1) / REPORT_LINE;
    let want = |n: usize| if n + 1 == count { encoded - REPORT_LINE * n } else { REPORT_LINE };
    let first: Vec<&str> = shown.split('~').collect();
    let second: Vec<&str> = again.split('~').collect();
    let labelled = first.windows(2).map(|p| (p[0].parse::<usize>().ok(), p[1]));
    let relabelled = second.windows(2).map(|p| (p[0].strip_prefix(part.tag()).and_then(|n| n.parse::<usize>().ok()), p[1]));
    let mut found: Vec<Option<&str>> = vec![None; count];
    for (label, line) in labelled.chain(relabelled) {
        let line = line.split('#').next().unwrap_or_default();
        if let Some(n) = label.filter(|&n| n < count && found[n].is_none() && line.len() == want(n)) {
            found[n] = Some(line);
        }
    }
    let missing: Vec<(Part, usize)> = (0..count).filter(|&n| found[n].is_none()).map(|n| (part, n)).collect();
    if !missing.is_empty() {
        return Err(missing);
    }
    Ok(found.into_iter().flatten().collect())
}

/// A part as the runner prints it, for the tests.
#[cfg(test)]
fn numbered(bytes: &[u8]) -> String {
    let encoded = B64.encode(bytes);
    encoded.as_bytes().chunks(REPORT_LINE).enumerate().map(|(n, c)| format!("~{n}~{}\r\n", std::str::from_utf8(c).unwrap())).collect()
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
            Ok(None) => return Err(CLOSED.into()),
            Ok(Some(Err(e))) => return Err(format!("{BROKEN}: {e}")),
            Ok(Some(Ok(bytes))) => bytes,
        };
        let Ok(msg) = Message::parse_from_bytes(&bytes) else { continue };
        match &msg.union {
            Some(message::Union::TestDelay(t)) if !t.from_client => {
                let mut reply = Message::new();
                reply.set_test_delay(t.clone());
                stream.send(&reply).await.map_err(|e| format!("{BROKEN}: {e}"))?;
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
    stream.send(msg).await.map_err(|e| format!("{BROKEN}: {e}"))
}

/// The end of what the terminal showed, for an answer that has nothing better to say.
fn last_shown(shown: &[u8]) -> String {
    let text = shell::plain(shown);
    let from = text.char_indices().rev().nth(800).map(|(i, _)| i).unwrap_or(0);
    text[from..].to_string()
}

const CLOSED: &str = "the PC closed the terminal connection";
const BROKEN: &str = "terminal connection error";
/// Stock RustDesk 1.4.5 on Windows closes or resets a terminal connection made right after the last one ended (seen on
/// deus: every second command of a quick pair); a later one is taken. Retried only while nothing has been typed, so no
/// command runs twice.
const CLOSED_RETRIES: [Duration; 3] = [Duration::from_millis(500), Duration::from_millis(1500), Duration::from_millis(3000)];

/// Run one command in a new terminal on the PC and answer what it printed and how it ended.
pub async fn run(link: &Link, asked: &Asked) -> Result<Called, String> {
    let mut waits = CLOSED_RETRIES.iter();
    loop {
        let mut typed = false;
        match attempt(link, asked, &mut typed).await {
            Err(why) if !typed && (why == CLOSED || why.starts_with(BROKEN)) => match waits.next() {
                Some(wait) => {
                    log::info!("terminal to {} lost before the command was typed ({why}); trying again", link.label());
                    sleep(*wait).await
                }
                None => return Err(format!("{why}, {} times before the command was typed", CLOSED_RETRIES.len() + 1)),
            },
            done => return done,
        }
    }
}

async fn attempt(link: &Link, asked: &Asked, typed: &mut bool) -> Result<Called, String> {
    let mut stream = link.dial().await.map_err(|e| format!("cannot reach the PC at {}: {e}", link.label()))?;
    let platform = log_in(link, &mut stream).await?;
    send(&mut stream, &terminal(terminal_action::Union::Open(OpenTerminal { terminal_id: TERMINAL_ID, rows: ROWS, cols: COLS, ..Default::default() })))
        .await?;
    let nonce = format!("{:016x}", hbb_common::rand::random::<u64>());
    let result = converse(&mut stream, Shell::of(&platform), asked, &nonce, typed).await;
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

async fn type_line(stream: &mut FramedStream, line: &str) -> Result<(), String> {
    let data = TerminalData { terminal_id: TERMINAL_ID, data: format!("{line}\r").into_bytes().into(), ..Default::default() };
    send(stream, &terminal(terminal_action::Union::Data(data))).await
}

/// Waits for the prompt, types the runner and reads until its report, asking a PowerShell runner again for lines the
/// terminal lost.
async fn converse(stream: &mut FramedStream, shell: Shell, asked: &Asked, nonce: &str, typed: &mut bool) -> Result<(Printed, Ended), String> {
    let mut shown: Vec<u8> = Vec::new();
    let mut opened = false;
    let mut last_data: Option<Instant> = None;
    let prompt_by = Instant::now() + PROMPT_WAIT;
    let mut report_by: Option<Instant> = None;
    let mut reporting = false;
    let open = format!("#RIG{nonce}.");
    let end = format!("#RIG{nonce}.end#");
    // Rounds of lines asked again, and how many end markers had shown when the last was asked.
    let mut asked_again = 0;
    let mut ends_when_asked = 0;
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
            let lines = typed_lines(shell, &asked.command, asked.timeout, nonce);
            let total = lines.len();
            *typed = true;
            for (i, line) in lines.into_iter().enumerate() {
                type_line(stream, &line).await?;
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
                if report_by.is_some() && !reporting && ends_marker {
                    reporting = bare(&shown).contains(&open);
                }
                // The command has ended once its report begins; however slowly the terminal shows the report, it may finish.
                if reporting {
                    report_by = Some(Instant::now() + REPORT_GRACE);
                }
                if report_by.is_some() && ends_marker {
                    let ends = bare(&shown).matches(&end).count();
                    match report(&shown, nonce, asked.timeout) {
                        None => {}
                        Some(Err(Unread::Missing(lines))) if shell == Shell::PowerShell && asked_again < REPAIR_ROUNDS => {
                            if ends > ends_when_asked {
                                log::info!("terminal lost {} report lines; asking again ({})", lines.len(), asked_again + 1);
                                asked_again += 1;
                                ends_when_asked = ends;
                                type_line(stream, &Unread::ask(&lines)).await?;
                            }
                        }
                        Some(found) => {
                            if shell == Shell::PowerShell {
                                let _ = type_line(stream, "ok").await;
                            }
                            return found.map_err(Unread::said);
                        }
                    }
                }
            }
            Some(terminal_response::Union::Closed(c)) => {
                if let Some(found) = report(&shown, nonce, asked.timeout) {
                    return found.map_err(Unread::said);
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
            let lines = typed_lines(shell, &command, Duration::from_secs(5), "abc");
            assert!(lines.iter().all(|l| l.len() < LINE + 20));
            assert!(!bare(lines.join("\r\n").as_bytes()).contains("#RIGabc."));
        }
    }

    #[test]
    fn the_report_is_read_through_wrapping_and_redrawing() {
        let out = numbered(b"hello\r\nworld\n");
        let (a, b) = out.split_at(5);
        let shown = format!("$ printf ...\r\n\x1b[?25l#RIGn1.3.0.13#\r\n{a}\r\n\x1b[2K{b}#RIGn1.ta\x1b[0mil#\r\n#RIGn1.end#\r\n$ ");
        let (printed, ended) = report(shown.as_bytes(), "n1", Duration::from_secs(9)).unwrap().unwrap();
        assert_eq!(ended, Ended::Exited(3));
        assert_eq!(printed, Printed::whole(b"hello\r\nworld\n".to_vec()));
        assert!(report(b"#RIGn1.3.0.13#~0~aGk=#RIGn1.tail#", "n1", Duration::from_secs(9)).is_none());
    }

    #[test]
    fn a_long_output_comes_as_its_ends_and_a_count_and_a_time_out_says_so() {
        let (start, end) = (vec![b's'; HEAD_PART], vec![b'e'; TAIL_PART]);
        let shown = format!("#RIGn.-1.1.{}#{}#RIGn.tail#{}#RIGn.end#", HEAD_PART + TAIL_PART + 7, numbered(&start), numbered(&end));
        let (printed, ended) = report(shown.as_bytes(), "n", Duration::from_secs(9)).unwrap().unwrap();
        assert_eq!(ended, Ended::TimedOut(Duration::from_secs(9)));
        assert_eq!(printed, Printed { head: start, dropped: 7, tail: end });
        let failed = format!("#RIGn.x.0.13#{}#RIGn.tail##RIGn.end#", numbered(b"Access denied"));
        let unread = report(failed.as_bytes(), "n", Duration::from_secs(9)).unwrap().unwrap_err();
        assert_eq!(unread, Unread::Failed("the PC could not run the command: Access denied".into()));
    }

    #[test]
    fn lines_the_console_repeats_or_cuts_short_are_taken_once() {
        let output: Vec<u8> = (0..2000u32).flat_map(|n| format!("line {n:06}\n").into_bytes()).collect();
        let lines = numbered(&output);
        let mut shown: Vec<&str> = lines.split_inclusive('\n').collect();
        let (third, fifth) = (shown[3], shown[5]);
        shown.insert(6, &third[..40]);
        shown.insert(6, fifth);
        let shown = format!("#RIGr.0.0.{}#{}#RIGr.tail##RIGr.end#", output.len(), shown.concat());
        let (printed, _) = report(shown.as_bytes(), "r", Duration::from_secs(9)).unwrap().unwrap();
        assert_eq!(printed, Printed::whole(output));
        let gap = format!("#RIGr.0.0.{}#{}#RIGr.tail##RIGr.end#", 1000, numbered(&[b'x'; 1000]).replacen("~2~", "~9~", 1));
        assert_eq!(report(gap.as_bytes(), "r", Duration::from_secs(9)).unwrap().unwrap_err(), Unread::Missing(vec![(Part::Head, 2)]));
    }

    #[test]
    fn lines_the_terminal_lost_are_taken_from_their_second_printing() {
        let (start, end) = (vec![b's'; HEAD_PART], vec![b'e'; TAIL_PART]);
        let (head, tail) = (numbered(&start), numbered(&end));
        let line = |text: &str, n: usize| text.lines().nth(n).unwrap().split('~').nth(2).unwrap().to_string();
        let total = HEAD_PART + TAIL_PART + 7;
        let lost = format!("#RIGn.0.0.{total}#{}#RIGn.tail#{}#RIGn.end#\r\n", head.replacen("~3~", "~", 1), tail.replacen("~0~", "", 1));
        let unread = report(lost.as_bytes(), "n", Duration::from_secs(9)).unwrap().unwrap_err();
        assert_eq!(unread, Unread::Missing(vec![(Part::Head, 3), (Part::Tail, 0)]));
        let Unread::Missing(missing) = unread else { unreachable!() };
        assert_eq!(Unread::ask(&missing), "h3 t0");
        // The typed request echoes before the answer, and the answer's last line runs into the end marker.
        let again = format!("{lost}h3 t0\r\n~h3~{}\r\n~t0~{}#RIGn.end#\r\n", line(&head, 3), line(&tail, 0));
        let (printed, _) = report(again.as_bytes(), "n", Duration::from_secs(9)).unwrap().unwrap();
        assert_eq!(printed, Printed { head: start, dropped: 7, tail: end });
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
    fn the_posix_runner_sends_the_ends_of_a_long_output() {
        let script = runner(Shell::Posix, "seq 1 40000", Duration::from_secs(5), "t3");
        let ran = std::process::Command::new("sh").arg("-c").arg(&script).output().unwrap();
        let (printed, ended) = report(&ran.stdout, "t3", Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(ended, Ended::Exited(0));
        assert!(printed.head.starts_with(b"1\n2\n") && printed.tail.ends_with(b"39999\n40000\n"));
        assert_eq!(printed.head.len() + printed.dropped + printed.tail.len(), (1..=40000).map(|n: u32| n.to_string().len() + 1).sum::<usize>());
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
