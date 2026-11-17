//! The management lane on a Linux PC (see [`crate::manage`]): RigDesk's root service runs each command, and each
//! background job, as its own transient systemd unit. A unit is outside the service's cgroup, so restarting or upgrading
//! RigDesk leaves it running, and ending a unit ends every process it started. A command as root starts from PID 1, with
//! a clean environment and no display. A command as the signed-in user runs in that user's own systemd manager, with the
//! environment their desktop gave it, the way a terminal they opened would.
//!
//! What a command prints goes to a file this process opened, which the unit is handed as its stdout and stderr
//! (`systemd-run --pipe`), so a command run as the user never needs to write where the lane keeps its files.

use std::fs::{DirBuilder, File};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::manage::{JobHandle, Refused, Runner, JOB_LIMIT};
use crate::shell::{self, Ended, Printed};

/// After a time-out, how long the killed unit has to go before its status is read anyway.
const KILL_WAIT: Duration = Duration::from_secs(5);
/// How much past its own limit systemd lets a unit run, should this process not be there to end it.
const BACKSTOP: Duration = Duration::from_secs(60);
/// What a command as root finds on its PATH.
const ROOT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const USER_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/local/games:/usr/games";

pub struct SystemdRunner {
    /// `on` or `off`, left by the installer. Read only while root owns it and no one else may write it.
    pub switch: PathBuf,
    /// Whether the person at the PC has paused the agent shell.
    pub paused: Box<dyn Fn() -> bool + Send + Sync>,
    /// Whom a command as the user runs as: on a PC, [`signed_in`].
    pub user: Box<dyn Fn() -> Result<SignedIn, String> + Send + Sync>,
}

/// The signed-in user a command as the user runs as: the active session on seat0, if it is a person's.
#[derive(Clone, Debug, PartialEq)]
pub struct SignedIn {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub shell: String,
}

impl Runner for SystemdRunner {
    fn allowed(&self) -> Result<(), Refused> {
        const TURN_ON: &str = "root turns it on by running the install again with --agent-shell on";
        let meta = match std::fs::metadata(&self.switch) {
            Ok(meta) => meta,
            Err(_) => return Err(Refused::Off(format!("this PC was installed without it; {TURN_ON}"))),
        };
        if meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            return Err(Refused::Off(format!("its switch, {}, is not root's alone, so it is not read; {TURN_ON}", self.switch.display())));
        }
        match std::fs::read_to_string(&self.switch).map(|s| s.trim().to_string()).as_deref() {
            Ok("on") => {}
            _ => return Err(Refused::Off(format!("the install turned it off (--agent-shell off); {TURN_ON}"))),
        }
        if (self.paused)() {
            return Err(Refused::Paused);
        }
        Ok(())
    }

    fn run_system(&self, command: &str, dir: &Path, limit: Duration) -> Result<Ended, String> {
        let out = File::create(dir.join("o")).map_err(|e| format!("cannot make the output file: {e}"))?;
        let unit = format!("rigdesk-run-{:08x}.service", rand::random::<u32>());
        let mut run = Command::new("systemd-run");
        run.args(["--quiet", "--collect", "--wait", "--pipe", "--uid=root"]).arg(format!("--unit={unit}"));
        unit_limits(&mut run, limit + BACKSTOP);
        run.arg(format!("--setenv=PATH={ROOT_PATH}")).arg("--").args(root_shell()).arg(unexpanded(command));
        let child = run.stdin(Stdio::null()).stdout(out.try_clone().map_err(|e| e.to_string())?).stderr(out).spawn().map_err(|e| format!("cannot start systemd-run: {e}"))?;
        wait(child, limit, || systemctl(None, &["kill", "--signal=SIGKILL", &unit]))
    }

    fn run_user(&self, command: &str, limit: Duration) -> Result<(Printed, Ended), String> {
        let user = (self.user)()?;
        let bus = format!("/run/user/{}/bus", user.uid);
        if !Path::new(&bus).exists() {
            return Err(format!("{} has no systemd user manager running, so nothing can run as them", user.name));
        }
        let dir = std::env::temp_dir().join(format!("rigdesk-user-{:08x}", rand::random::<u32>()));
        DirBuilder::new().mode(0o700).create(&dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
        let ran = (|| {
            let path = dir.join("o");
            let out = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).map_err(|e| format!("cannot make the output file: {e}"))?;
            let unit = format!("rigdesk-run-{:08x}.service", rand::random::<u32>());
            let mut run = as_user(&user, "systemd-run");
            run.args(["--user", "--quiet", "--collect", "--wait", "--pipe"]).arg(format!("--unit={unit}"));
            unit_limits(&mut run, limit + BACKSTOP);
            run.arg("--").arg(&user.shell).arg("-c").arg(unexpanded(command));
            let child = run.stdin(Stdio::null()).stdout(out.try_clone().map_err(|e| e.to_string())?).stderr(out).spawn().map_err(|e| format!("cannot start systemd-run as {}: {e}", user.name))?;
            let ended = wait(child, limit, || systemctl(Some(&user), &["--user", "kill", "--signal=SIGKILL", &unit]))?;
            Ok((shell::printed_file(&path).map_err(|e| format!("cannot read the command's output: {e}"))?, ended))
        })();
        let _ = std::fs::remove_dir_all(&dir);
        ran
    }

    fn start_system(&self, command: &str, dir: &Path) -> Result<JobHandle, String> {
        let shown = dir.to_str().filter(|d| !d.chars().any(char::is_whitespace)).ok_or_else(|| format!("the job directory {} cannot be named to systemd", dir.display()))?;
        let id = dir.file_name().and_then(|n| n.to_str()).ok_or("the job directory has no name")?;
        std::fs::write(dir.join("c.sh"), command).map_err(|e| format!("cannot write the command: {e}"))?;
        let unit = format!("rigdesk-job-{id}.service");
        let mut run = Command::new("systemd-run");
        run.args(["--quiet", "--collect", "--uid=root"]).arg(format!("--unit={unit}"));
        unit_limits(&mut run, JOB_LIMIT + BACKSTOP);
        run.arg(format!("--property=StandardOutput=append:{shown}/o")).arg(format!("--property=StandardError=append:{shown}/o"));
        let [shell, flag] = root_shell();
        run.arg(format!("--setenv=PATH={ROOT_PATH}")).arg("--").args([shell, flag]);
        // The command's exit code goes to x once it ends, beside its output; the lane reads both. The job runs in the
        // same shell as the wrapper, so a PC without bash runs it in sh too.
        run.arg(format!(r#"{shell} "$0" </dev/null; echo $? > "$1.new" && mv "$1.new" "$1""#)).arg(format!("{shown}/c.sh")).arg(format!("{shown}/x"));
        let done = run.stdin(Stdio::null()).output().map_err(|e| format!("cannot start systemd-run: {e}"))?;
        if !done.status.success() {
            return Err(format!("systemd-run would not start the job: {}", String::from_utf8_lossy(&done.stderr).trim()));
        }
        Ok(JobHandle(unit))
    }

    fn alive(&self, handle: &JobHandle) -> bool {
        Command::new("systemctl").args(["is-active", "--quiet", &handle.0]).stdin(Stdio::null()).status().map_or(false, |s| s.success())
    }

    fn end(&self, handle: &JobHandle) {
        systemctl(None, &["kill", "--signal=SIGKILL", &handle.0]);
    }

    fn boot(&self) -> String {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map_or_else(|_| "boot-unknown".into(), |id| format!("boot-{}", id.trim()))
    }
}

/// `command` as systemd-run must be given it to run it as written. systemd expands `${NAME}` and `$NAME` in a unit's
/// command line and turns `$$` into `$`, and systemd-run passes `$` through (before systemd 254 always, after it by
/// default), so each `$` goes as `$$`. It leaves `%` as written.
fn unexpanded(command: &str) -> String {
    command.replace('$', "$$")
}

/// bash where the PC has it, as Rig's agents expect; else sh.
fn root_shell() -> [&'static str; 2] {
    if Path::new("/bin/bash").exists() {
        ["/bin/bash", "-c"]
    } else {
        ["/bin/sh", "-c"]
    }
}

/// Every unit the lane starts: its home as its directory, ended whole, and never longer than `limit`.
fn unit_limits(run: &mut Command, limit: Duration) {
    run.arg("--property=WorkingDirectory=~").arg("--property=KillMode=control-group").arg(format!("--property=RuntimeMaxSec={}", limit.as_secs()));
}

/// Waits for a `systemd-run --wait` until `limit`, then kills its unit and says it timed out.
fn wait(mut child: Child, limit: Duration, kill: impl FnOnce()) -> Result<Ended, String> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok(Ended::Exited(status.code().unwrap_or(-1)));
        }
        if Instant::now() >= deadline {
            kill();
            let gone = Instant::now() + KILL_WAIT;
            while Instant::now() < gone && child.try_wait().map_err(|e| e.to_string())?.is_none() {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Ended::TimedOut(limit));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A systemd client run as `user`, talking to their own manager; the standard library drops root's groups with its ids.
fn as_user(user: &SignedIn, program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .uid(user.uid)
        .gid(user.gid)
        .env_clear()
        .env("XDG_RUNTIME_DIR", format!("/run/user/{}", user.uid))
        .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path=/run/user/{}/bus", user.uid))
        .env("HOME", &user.home)
        .env("USER", &user.name)
        .env("LOGNAME", &user.name)
        .env("PATH", USER_PATH)
        .current_dir("/");
    command
}

fn systemctl(user: Option<&SignedIn>, args: &[&str]) {
    let mut command = match user {
        Some(user) => as_user(user, "systemctl"),
        None => Command::new("systemctl"),
    };
    if let Err(e) = command.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status() {
        log::warn!("agent shell: systemctl {}: {e}", args.join(" "));
    }
}

/// The person signed in at the PC: seat0's active session, refused when it is a greeter's or root's or there is none.
pub fn signed_in() -> Result<SignedIn, String> {
    const NOBODY: &str = "nobody is signed in at this PC";
    let session = loginctl(&["show-seat", "seat0", "--property=ActiveSession", "--value"])?;
    if session.is_empty() {
        return Err(NOBODY.into());
    }
    let said = loginctl(&["show-session", &session, "--property=Name", "--property=User", "--property=Class", "--property=State"])?;
    let field = |key: &str| said.lines().find_map(|line| line.strip_prefix(key).and_then(|v| v.strip_prefix('='))).unwrap_or("").to_string();
    if field("Class") != "user" {
        return Err(format!("{NOBODY}: its screen shows the login screen"));
    }
    let uid: u32 = field("User").parse().map_err(|_| format!("loginctl gave no user for session {session}"))?;
    if uid == 0 {
        return Err(format!("{NOBODY} but root, and a command as the user never runs as root"));
    }
    user_entry(uid, &std::fs::read_to_string("/etc/passwd").unwrap_or_default()).ok_or_else(|| format!("user {uid}, signed in at seat0, is not in /etc/passwd"))
}

/// The user named `name` in /etc/passwd, for a host with no seat: the lane's smoke test, under a lingering user.
pub fn user_named(name: &str) -> Result<SignedIn, String> {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let uid = passwd.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix(":")?.split(':').nth(1)?.parse::<u32>().ok()).ok_or_else(|| format!("no user {name} in /etc/passwd"))?;
    user_entry(uid, &passwd).ok_or_else(|| format!("no user {name} in /etc/passwd"))
}

/// The passwd entry for `uid`, its shell absolute or else sh.
fn user_entry(uid: u32, passwd: &str) -> Option<SignedIn> {
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 7 || f[2].parse::<u32>().ok()? != uid {
            return None;
        }
        let shell = if f[6].starts_with('/') && !f[6].ends_with("nologin") && f[6] != "/bin/false" { f[6] } else { "/bin/sh" };
        Some(SignedIn { name: f[0].into(), uid, gid: f[3].parse().ok()?, home: f[5].into(), shell: shell.into() })
    })
}

fn loginctl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("loginctl").args(args).stdin(Stdio::null()).output().map_err(|e| format!("cannot run loginctl: {e}"))?;
    if !out.status.success() {
        return Err(format!("loginctl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_passwd_entry_gives_the_user_and_a_shell_that_can_run_a_command() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\ndeus:x:1000:1000:Deus,,,:/home/deus:/bin/bash\nsvc:x:1001:1001::/srv:/usr/sbin/nologin\n";
        assert_eq!(user_entry(1000, passwd), Some(SignedIn { name: "deus".into(), uid: 1000, gid: 1000, home: "/home/deus".into(), shell: "/bin/bash".into() }));
        assert_eq!(user_entry(1001, passwd).unwrap().shell, "/bin/sh");
        assert_eq!(user_entry(4242, passwd), None);
    }
}
