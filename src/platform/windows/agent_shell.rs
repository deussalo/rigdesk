//! The agent's `shell` on a RigDesk PC: one PowerShell command as the signed-in user, never as SYSTEM.
//!
//! The command and `shell::POWERSHELL_WRAPPER` go in a fresh directory in the user's own temp directory, where only the
//! user, SYSTEM and administrators can look, and run through cmd, which writes their output and errors to one file there
//! in the order they came. As the user, the app runs it directly; as SYSTEM (the service-spawned `--server`), through the
//! user's session token. Over its limit, its whole process tree is ended.

use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hbb_common::{anyhow::anyhow, bail, ResultType};
use rigdesk_agent::shell::{self, Ended, Printed};
use winapi::um::{
    handleapi::CloseHandle,
    processthreadsapi::{GetExitCodeProcess, GetProcessId},
    synchapi::WaitForSingleObject,
    winbase::WAIT_OBJECT_0,
    winnt::HANDLE,
};
use windows::Win32::{
    Foundation::HANDLE as WinHANDLE,
    System::Com::CoTaskMemFree,
    UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath, KF_FLAG_DEFAULT},
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `LaunchProcessWin` copies the command line into a MAX_PATH buffer.
const LAUNCH_LINE_MAX: usize = 259;
/// After a time-out, how long the ended process tree has to go before its status is read anyway.
const KILL_WAIT: Duration = Duration::from_secs(5);

pub fn run_shell_as_user(command: &str, limit: Duration) -> ResultType<(Printed, Ended)> {
    let session = if super::is_root() {
        Some(super::get_current_process_session_id().ok_or_else(|| anyhow!("cannot run the command as the user: no session id for this process"))?)
    } else {
        None
    };
    let temp = match session {
        Some(id) => user_temp_dir(id)?,
        None => std::env::temp_dir(),
    };
    let dir = temp.join(format!("rd-{:08x}", hbb_common::rand::random::<u32>()));
    std::fs::create_dir(&dir).map_err(|e| anyhow!("cannot make {}: {e}", dir.display()))?;
    let ran = run_in(&dir, command, limit, session);
    let _ = std::fs::remove_dir_all(&dir);
    ran
}

/// The signed-in user's temp directory, found through their token: SYSTEM's own is elsewhere.
fn user_temp_dir(session: u32) -> ResultType<PathBuf> {
    let token = super::get_user_token(session, true);
    if token.is_null() {
        bail!("cannot run the command as the user: no user is signed in to session {session}");
    }
    let found = unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, Some(WinHANDLE(token as _))) };
    unsafe { CloseHandle(token) };
    let value = found.map_err(|e| anyhow!("cannot find the user's local app data: {e}"))?;
    let path = unsafe { value.to_string() };
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    Ok(PathBuf::from(path?).join("Temp"))
}

fn run_in(dir: &Path, command: &str, limit: Duration, session: Option<u32>) -> ResultType<(Printed, Ended)> {
    // Windows PowerShell reads a script without a byte-order mark in the ANSI code page.
    std::fs::write(dir.join("c.ps1"), [&[0xef, 0xbb, 0xbf][..], command.as_bytes()].concat())?;
    std::fs::write(dir.join("w.ps1"), shell::POWERSHELL_WRAPPER)?;
    let out = dir.join("o");
    // cmd strips the outer quotes and keeps the inner ones.
    let line = format!(
        "/d /c \"powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\" > \"{}\" 2>&1\"",
        dir.join("w.ps1").display(),
        out.display()
    );
    let (code, timed_out) = match session {
        Some(id) => launched_as_user(&format!("cmd.exe {line}"), id, limit)?,
        None => spawned(&line, limit)?,
    };
    let printed = shell::printed_file(&out)?;
    Ok((printed, if timed_out { Ended::TimedOut(limit) } else { Ended::Exited(code) }))
}

/// Ends a process and every process it started.
fn end_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill.exe").args(["/T", "/F", "/PID", &pid.to_string()]).creation_flags(CREATE_NO_WINDOW).output();
}

fn launched_as_user(line: &str, session: u32, limit: Duration) -> ResultType<(i32, bool)> {
    let wide: Vec<u16> = std::ffi::OsStr::new(line).encode_wide().chain(Some(0)).collect();
    if wide.len() > LAUNCH_LINE_MAX {
        bail!("cannot run the command as the user: its launch line is {} characters, over Windows' {LAUNCH_LINE_MAX} (the user's temp path is too long)", wide.len() - 1);
    }
    let mut token_pid = 0;
    let handle: HANDLE = unsafe { super::LaunchProcessWin(wide.as_ptr(), session, 1, 0, &mut token_pid) };
    if handle.is_null() {
        bail!("cannot run the command as the user (session {session}): {}", std::io::Error::last_os_error());
    }
    let millis = limit.as_millis().min(u32::MAX as u128 - 1) as u32;
    let timed_out = unsafe { WaitForSingleObject(handle, millis) } != WAIT_OBJECT_0;
    if timed_out {
        end_tree(unsafe { GetProcessId(handle) });
        unsafe { WaitForSingleObject(handle, KILL_WAIT.as_millis() as u32) };
    }
    let mut code = 0u32;
    let read = unsafe { GetExitCodeProcess(handle, &mut code) };
    unsafe { CloseHandle(handle) };
    if read == 0 {
        bail!("cannot read the command's exit code: {}", std::io::Error::last_os_error());
    }
    Ok((code as i32, timed_out))
}

fn spawned(line: &str, limit: Duration) -> ResultType<(i32, bool)> {
    let mut child = std::process::Command::new("cmd.exe").raw_arg(line).creation_flags(CREATE_NO_WINDOW).spawn()?;
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status.code().unwrap_or(-1), false));
        }
        if Instant::now() >= deadline {
            end_tree(child.id());
            let status = child.wait()?;
            return Ok((status.code().unwrap_or(-1), true));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
