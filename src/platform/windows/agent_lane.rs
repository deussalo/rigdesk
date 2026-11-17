//! The management lane on Windows (see `rigdesk_agent::manage`): Rig's commands, and the PC's own health script, as
//! SYSTEM in session 0 with no console window. The service's `--server` is SYSTEM in the console session, where whatever
//! it started, a GUI program included, would show on the user's desktop with SYSTEM's rights; a copy of its token moved to
//! session 0, on the non-interactive window station, can draw nowhere anyone sees; the lane's own helpers (icacls,
//! taskkill) start there too, so nothing the lane does starts a process where the user is. `as: user` runs through
//! `run_shell_as_user`, as the desktop's own shell does.
//!
//! The lane's directories are SYSTEM's and administrators' alone. What ran is also kept, in brief, in a file every user of
//! the PC can read and no one but SYSTEM and administrators can change: the tray's "Managed by Rig" opens it.

use std::{
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use hbb_common::{log, tokio};
use rigdesk_agent::{
    events::EventKind,
    health::{Collector, Samples, INTERVAL},
    manage::{JobHandle, Lane, Refused, Runner},
    shell::{self, Ended, Printed},
    Api, Journal, Shared,
};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, HANDLE, LUID, STILL_ACTIVE, WAIT_OBJECT_0},
        Security::{
            AdjustTokenPrivileges, DuplicateTokenEx, LookupPrivilegeValueW, SecurityImpersonation, SetTokenInformation,
            TokenPrimary, TokenSessionId, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, SE_TCB_NAME, TOKEN_ADJUST_PRIVILEGES,
            TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_PRIVILEGES, TOKEN_QUERY,
        },
        System::Threading::{
            CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcess, OpenProcessToken,
            WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW,
        },
    },
};

const HEALTH_SCRIPT: &str = include_str!("agent_health.ps1");
/// What `-AgentShell` on the install line left, beside api.token: `on` or `off`.
const SWITCH_FILE: &str = "agent-shell";
/// How many commands the tray's list shows.
const DIGEST_LINES: usize = 200;
/// After a time-out, how long the ended process tree has to go before its status is read anyway.
const KILL_WAIT: Duration = Duration::from_secs(5);
/// How long a helper (icacls, taskkill) has before its exit code is read anyway.
const HELPER_WAIT: Duration = Duration::from_secs(30);

struct WindowsRunner {
    switch: PathBuf,
}

/// The lane, and the collector with it, on the agent API: only in the service's `--server`, which is SYSTEM.
pub fn manage(api: &Api, shared: &Arc<Shared>, journal: &Arc<Journal>, data_dir: Option<PathBuf>) {
    if !super::is_root() {
        log::info!("agent shell: this process is not SYSTEM, so it offers no management lane");
        return;
    }
    let Some(data) = data_dir else {
        log::warn!("agent shell: no data dir; no management lane");
        return;
    };
    let (lane_dir, health_dir) = (data.join("shell"), data.join("health"));
    for dir in [&lane_dir, &health_dir] {
        if let Err(why) = private_dir(dir) {
            log::error!("agent shell: {why}; no management lane");
            return;
        }
    }
    let runner = Arc::new(WindowsRunner { switch: data.join(SWITCH_FILE) });
    let on = runner.allowed().is_ok();
    let lane = Lane::new(runner, lane_dir, shared.clone(), journal.clone());
    let collector = Arc::new(Collector {
        lane: lane.clone(),
        shared: shared.clone(),
        samples: Arc::new(Samples::open(Some(data.join("health.jsonl")))),
        script: HEALTH_SCRIPT,
        dir: health_dir,
        rigdesk: crate::VERSION.to_string(),
        interval: INTERVAL,
    });
    api.manage(lane, Some(collector));
    match digest_file() {
        Some(digest) if on => {
            write_digest(&digest, journal);
            tokio::spawn(keep_digest(shared.clone(), journal.clone(), digest));
        }
        Some(digest) => {
            let _ = std::fs::remove_file(digest);
        }
        None => log::warn!("agent shell: no ProgramData; the tray lists no commands"),
    }
    log::info!("agent shell: management lane offered; the agent shell is {}", if on { "on" } else { "off" });
}

/// Made if missing, then readable and writable by SYSTEM and administrators alone, which its files inherit.
fn private_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    icacls(dir, &["/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F"])
}

fn icacls(path: &Path, args: &[&str]) -> Result<(), String> {
    let line = format!("icacls.exe \"{}\" {}", path.display(), args.join(" "));
    match helper(&line, path.parent().unwrap_or(path)).map_err(|e| format!("icacls on {}: {e}", path.display()))? {
        0 => Ok(()),
        code => Err(format!("icacls on {} exited {code}", path.display())),
    }
}

/// Runs one of the lane's own helpers as SYSTEM in session 0, like its commands, and waits for its exit code.
fn helper(line: &str, dir: &Path) -> Result<u32, String> {
    let process = launch(line, dir)?;
    if unsafe { WaitForSingleObject(process.handle, HELPER_WAIT.as_millis() as u32) } != WAIT_OBJECT_0 {
        return Err(format!("still running after {} s", HELPER_WAIT.as_secs()));
    }
    let mut code = 0u32;
    unsafe { GetExitCodeProcess(process.handle, &mut code) }.map_err(|e| format!("cannot read its exit code: {e}"))?;
    Ok(code)
}

impl Runner for WindowsRunner {
    fn allowed(&self) -> Result<(), Refused> {
        match std::fs::read_to_string(&self.switch).map(|s| s.trim().to_string()) {
            Ok(on) if on == "on" => {}
            Ok(_) => return Err(Refused::Off("the install line turned it off (-AgentShell off); an administrator turns it on by running the install line again with -AgentShell on".into())),
            Err(_) => return Err(Refused::Off("this PC was enrolled before the agent shell existed; an administrator turns it on by running the install line again with -AgentShell on".into())),
        }
        if hbb_common::config::Config::get_option(base::config::keys::OPTION_AGENT_SHELL_PAUSED) == "Y" {
            return Err(Refused::Paused);
        }
        Ok(())
    }

    fn run_system(&self, command: &str, dir: &Path, limit: Duration) -> Result<Ended, String> {
        let process = launch(&line(dir, command, false)?, dir)?;
        let millis = limit.as_millis().min(u32::MAX as u128 - 1) as u32;
        let timed_out = unsafe { WaitForSingleObject(process.handle, millis) } != WAIT_OBJECT_0;
        if timed_out {
            end_tree(process.pid);
            unsafe { WaitForSingleObject(process.handle, KILL_WAIT.as_millis() as u32) };
        }
        let mut code = 0u32;
        let read = unsafe { GetExitCodeProcess(process.handle, &mut code) };
        read.map_err(|e| format!("cannot read the command's exit code: {e}"))?;
        Ok(if timed_out { Ended::TimedOut(limit) } else { Ended::Exited(code as i32) })
    }

    fn run_user(&self, command: &str, limit: Duration) -> Result<(Printed, Ended), String> {
        super::run_shell_as_user(command, limit).map_err(|e| e.to_string())
    }

    fn start_system(&self, command: &str, dir: &Path) -> Result<JobHandle, String> {
        Ok(JobHandle(launch(&line(dir, command, true)?, dir)?.pid.to_string()))
    }

    fn alive(&self, job: &JobHandle) -> bool {
        let Ok(pid) = job.0.parse::<u32>() else { return false };
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else { return false };
        let mut code = 0u32;
        let read = unsafe { GetExitCodeProcess(handle, &mut code) };
        unsafe {
            let _ = CloseHandle(handle);
        }
        read.is_ok() && code == STILL_ACTIVE.0 as u32
    }

    fn end(&self, job: &JobHandle) {
        match job.0.parse::<u32>() {
            Ok(pid) => end_tree(pid),
            Err(_) => log::warn!("agent shell: job handle {job} is not a process id; nothing ended"),
        }
    }

    fn boot(&self) -> String {
        boot_id()
    }
}

/// The command and `shell::POWERSHELL_WRAPPER` written to `dir`, and the line that runs them through cmd, which writes
/// their output and errors to `dir\o` in the order they came; a job's line then writes its exit code to `dir\x`.
fn line(dir: &Path, command: &str, job: bool) -> Result<String, String> {
    // Windows PowerShell reads a script without a byte-order mark in the ANSI code page.
    std::fs::write(dir.join("c.ps1"), [&[0xef, 0xbb, 0xbf][..], command.as_bytes()].concat()).map_err(|e| format!("cannot write the command: {e}"))?;
    std::fs::write(dir.join("w.ps1"), shell::POWERSHELL_WRAPPER).map_err(|e| format!("cannot write the wrapper: {e}"))?;
    let (w, o, x) = (dir.join("w.ps1"), dir.join("o"), dir.join("x"));
    let run = format!("powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\" > \"{}\" 2>&1", w.display(), o.display());
    // cmd strips the outer quotes and keeps the inner ones; /v:on reads !errorlevel! as the line runs, not as it is read.
    Ok(if job {
        format!("cmd.exe /d /v:on /c \"{run} & echo !errorlevel!> \"{x}.new\" & move /y \"{x}.new\" \"{x}\" > nul\"", x = x.display())
    } else {
        format!("cmd.exe /d /c \"{run}\"")
    })
}

struct Launched {
    handle: HANDLE,
    pid: u32,
}

impl Drop for Launched {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Starts `line` as SYSTEM in session 0, on the non-interactive window station, with no console window.
fn launch(line: &str, dir: &Path) -> Result<Launched, String> {
    let token = system_token_in_session_0()?;
    let mut line: Vec<u16> = std::ffi::OsStr::new(line).encode_wide().chain(Some(0)).collect();
    let dir: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    // An empty desktop name connects a LocalSystem process to the non-interactive window station, Service-0x0-3e7$.
    let mut desktop: Vec<u16> = vec![0];
    let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, lpDesktop: PWSTR::from_raw(desktop.as_mut_ptr()), ..Default::default() };
    let mut pi = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessAsUserW(Some(token.0), PCWSTR::null(), Some(PWSTR::from_raw(line.as_mut_ptr())), None, None, false, CREATE_NO_WINDOW, None, PCWSTR::from_raw(dir.as_ptr()), &si, &mut pi)
    };
    created.map_err(|e| format!("cannot start the command as SYSTEM in session 0: {e}"))?;
    unsafe {
        let _ = CloseHandle(pi.hThread);
    }
    Ok(Launched { handle: pi.hProcess, pid: pi.dwProcessId })
}

struct Token(HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// A primary copy of this process's SYSTEM token with its session set to 0, which SeTcbPrivilege allows.
fn system_token_in_session_0() -> Result<Token, String> {
    let mut own = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES, &mut own) }.map_err(|e| format!("cannot open this process's token: {e}"))?;
    let own = Token(own);
    let mut luid = LUID::default();
    unsafe { LookupPrivilegeValueW(PCWSTR::null(), SE_TCB_NAME, &mut luid) }.map_err(|e| format!("cannot look up SeTcbPrivilege: {e}"))?;
    let tcb = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }] };
    unsafe { AdjustTokenPrivileges(own.0, false, Some(&tcb), 0, None, None) }.map_err(|e| format!("cannot enable SeTcbPrivilege: {e}"))?;
    let mut copy = HANDLE::default();
    unsafe { DuplicateTokenEx(own.0, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut copy) }.map_err(|e| format!("cannot copy this process's token: {e}"))?;
    let copy = Token(copy);
    let session: u32 = 0;
    unsafe { SetTokenInformation(copy.0, TokenSessionId, &session as *const u32 as *const _, std::mem::size_of::<u32>() as u32) }
        .map_err(|e| format!("cannot move the SYSTEM token to session 0: {e}"))?;
    Ok(copy)
}

/// Ends a process and every process it started.
fn end_tree(pid: u32) {
    if let Err(why) = helper(&format!("taskkill.exe /T /F /PID {pid}"), &std::env::temp_dir()) {
        log::warn!("agent shell: taskkill of {pid}: {why}");
    }
}

/// Windows counts its boots; failing that, the minute it booted.
fn boot_id() -> String {
    use winreg::{enums::HKEY_LOCAL_MACHINE, RegKey};
    let counted = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters")
        .and_then(|key| key.get_value::<u32, _>("BootId"));
    match counted {
        Ok(id) => format!("boot-{id}"),
        Err(_) => {
            let up_ms = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() };
            format!("booted-{}", rigdesk_agent::now_ms().saturating_sub(up_ms) / 60_000)
        }
    }
}

/// The tray's list of what ran, under ProgramData.
pub fn digest_file() -> Option<PathBuf> {
    let data = std::env::var_os("ProgramData")?;
    Some(PathBuf::from(data).join(crate::get_app_name()).join("managed-by-rig.txt"))
}

fn write_digest(path: &Path, journal: &Journal) {
    let mut text = format!(
        "This PC is managed by Rig. Agents may run commands on it as SYSTEM, without a window, and every command is listed here,\r\nnewest first. Only an administrator can change or turn this off, by running the install line again.\r\n\r\n"
    );
    let entries: Vec<_> = journal.tail(4 * DIGEST_LINES).into_iter().filter(|e| e.what == "shell").collect();
    if entries.is_empty() {
        text.push_str("No commands have run yet.\r\n");
    }
    for entry in entries.iter().rev().take(DIGEST_LINES) {
        let at = chrono_free_utc(entry.at_ms);
        let detail = &entry.detail;
        let as_whom = detail.get("as").and_then(|v| v.as_str()).unwrap_or("-");
        let graph = detail.get("graph").and_then(|v| v.as_str()).unwrap_or("");
        let command: String = detail.get("command").and_then(|v| v.as_str()).unwrap_or("").chars().take(300).collect::<String>().replace(['\r', '\n'], " ");
        let took = detail.get("took_ms").and_then(|v| v.as_u64()).map_or(String::new(), |ms| format!(", {:.1} s", ms as f64 / 1000.0));
        text.push_str(&format!("{at}  {}  [{graph}] as {as_whom}: {}{took}\r\n    {command}\r\n", entry.who, entry.outcome));
    }
    let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(path, text));
    if let Err(e) = written {
        log::warn!("agent shell: the tray's list of commands is not written: {e}");
        return;
    }
    if let Err(why) = icacls(path, &["/inheritance:r", "/grant:r", "*S-1-5-18:F", "*S-1-5-32-544:F", "*S-1-5-32-545:R"]) {
        log::warn!("agent shell: {why}");
    }
}

fn chrono_free_utc(at_ms: u64) -> String {
    let secs = at_ms / 1000;
    let (days, day) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", day / 3600, day / 60 % 60)
}

async fn keep_digest(shared: Arc<Shared>, journal: Arc<Journal>, path: PathBuf) {
    use tokio::sync::broadcast::error::RecvError;
    let mut events = shared.events.subscribe();
    loop {
        match events.recv().await {
            Ok(event) if matches!(event.kind, EventKind::Shell { .. }) => {
                let (journal, path) = (journal.clone(), path.clone());
                let _ = tokio::task::spawn_blocking(move || write_digest(&path, &journal)).await;
            }
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => break,
        }
    }
}

/// Opens the tray's list of commands for the person at the PC; false when the agent shell is off here.
pub fn open_digest() -> bool {
    let Some(path) = digest_file().filter(|p| p.exists()) else { return false };
    std::process::Command::new("notepad.exe").arg(path).spawn().is_ok()
}
