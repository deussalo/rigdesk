//! The agent's `shell` on a RigDesk Linux PC: one command through the user's login shell (`$SHELL -c`, else `sh -c`),
//! run by the session's `--server`, which the service starts as the signed-in user on X11. It never runs as root.
//!
//! The command's output and errors go, in the order they came, to one file in a fresh directory only the user can read.
//! It runs in its own process group with no input, so over its limit the group is ended whole; within it, what it
//! starts in the background keeps running, as a server the agent starts this way must.

use std::fs::{DirBuilder, File};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hbb_common::{anyhow::anyhow, bail, ResultType};
use rigdesk_agent::shell::{self, Ended, Printed};

/// After a time-out, how long the ended group has to go before its status is read anyway.
const KILL_WAIT: Duration = Duration::from_secs(5);

pub fn run_shell_as_user(command: &str, limit: Duration) -> ResultType<(Printed, Ended)> {
    if super::is_root() {
        bail!("cannot run the command as the user: this RigDesk process runs as root (a login screen, or a session served from the service), and the agent shell never runs as root");
    }
    let dir = std::env::temp_dir().join(format!("rd-{:08x}", hbb_common::rand::random::<u32>()));
    DirBuilder::new().mode(0o700).create(&dir).map_err(|e| anyhow!("cannot make {}: {e}", dir.display()))?;
    let ran = run_in(&dir, command, limit);
    let _ = std::fs::remove_dir_all(&dir);
    ran
}

fn run_in(dir: &Path, command: &str, limit: Duration) -> ResultType<(Printed, Ended)> {
    let out = dir.join("o");
    let file = File::create(&out)?;
    let login = std::env::var("SHELL").ok().filter(|s| Path::new(s).is_absolute()).unwrap_or_else(|| "/bin/sh".into());
    let mut child = Command::new(&login)
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .current_dir(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
        .process_group(0)
        .spawn()
        .map_err(|e| anyhow!("cannot start {login}: {e}"))?;
    let deadline = Instant::now() + limit;
    let (code, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (exit_code(status), false);
        }
        if Instant::now() >= deadline {
            end_group(child.id());
            let gone = Instant::now() + KILL_WAIT;
            let status = loop {
                if let Some(status) = child.try_wait()? {
                    break Some(status);
                }
                if Instant::now() >= gone {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            break (status.map_or(-1, exit_code), true);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let printed = shell::printed_file(&out)?;
    Ok((printed, if timed_out { Ended::TimedOut(limit) } else { Ended::Exited(code) }))
}

/// A shell's status: its exit code, or 128 plus the signal that ended it, as the shell itself reports it.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Ends the command's process group: what it left running in the background goes with it.
fn end_group(pid: u32) {
    let _ = Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).stderr(Stdio::null()).status();
}
