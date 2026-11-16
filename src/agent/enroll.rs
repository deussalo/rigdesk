//! Per-PC secrets from the install line. On Windows the installer, running elevated, writes `enroll.json` beside the
//! installed binary (under Program Files, where only administrators can write) and restarts the service. On Linux the
//! server runs as the desktop user, so the file is that user's own, `enroll.json` in the agent directory, 0600; anyone
//! who can write it can already edit the config beside it. The server applies it once at startup, before it serves
//! IPC, and deletes it. Nothing here logs or prints a secret.
//!
//! The permanent password can also be set at the PC, on the Security settings page.

#[cfg(windows)]
use std::path::Path;
use std::path::PathBuf;

use base::config::keys;
use hbb_common::{
    config::{self, Config},
    log,
};
use serde_derive::Deserialize;

#[derive(Deserialize)]
struct Enrollment {
    #[serde(default)]
    password: String,
    #[serde(default)]
    pin: String,
    #[serde(default)]
    agent_token: String,
    /// `on` or `off`: whether agents may run commands on this PC without a session, from `-AgentShell`.
    #[serde(default)]
    agent_shell: String,
    /// `on` or `off`: whether the operator may start agent sessions on this PC from Rig, with no share link, from `-AgentJoin`.
    #[serde(default)]
    agent_join: String,
}

/// The enrollment file of the installed binary, if this process is that binary.
#[cfg(windows)]
fn enrollment_file() -> Option<PathBuf> {
    if !crate::is_server() || !crate::platform::is_installed() {
        return None;
    }
    let (_, dir, _, exe) = crate::platform::get_install_info();
    let current = std::env::current_exe().ok()?;
    if !current.to_string_lossy().eq_ignore_ascii_case(&exe) {
        return None;
    }
    Some(Path::new(&dir).join("enroll.json"))
}

/// The enrollment file in the desktop user's agent directory, if this process is the server.
#[cfg(target_os = "linux")]
fn enrollment_file() -> Option<PathBuf> {
    if !crate::is_server() {
        return None;
    }
    Some(super::data_dir()?.join("enroll.json"))
}

pub fn apply() {
    let Some(path) = enrollment_file() else { return };
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    if let Err(e) = std::fs::remove_file(&path) {
        log::error!("enroll: could not delete {}: {e}", path.display());
    }
    let enrollment = match serde_json::from_str::<Enrollment>(&text) {
        Ok(e) => e,
        Err(e) => {
            log::error!("enroll: {} is not valid enrollment JSON ({e}); nothing applied", path.display());
            return;
        }
    };
    if !enrollment.password.is_empty() {
        let flag = keys::OPTION_DISABLE_CHANGE_PERMANENT_PASSWORD;
        let locked = config::BUILTIN_SETTINGS.write().unwrap().remove(flag);
        let ok = Config::set_permanent_password(&enrollment.password);
        if let Some(v) = locked {
            config::BUILTIN_SETTINGS.write().unwrap().insert(flag.to_owned(), v);
        }
        log::info!("enroll: permanent password {}", if ok { "set" } else { "NOT set" });
    }
    if !enrollment.pin.is_empty() {
        Config::set_unlock_pin(&enrollment.pin);
        log::info!("enroll: admin settings PIN set");
    }
    if !enrollment.agent_token.is_empty() {
        match write_token(&enrollment.agent_token) {
            Ok(()) => log::info!("enroll: agent API token set"),
            Err(e) => log::error!("enroll: could not write the agent API token: {e}"),
        }
    }
    match enrollment.agent_shell.as_str() {
        "" => {}
        on @ ("on" | "off") => match super::data_dir().map(|dir| std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join("agent-shell"), on))) {
            Some(Ok(())) => log::info!("enroll: agent shell {on}"),
            Some(Err(e)) => log::error!("enroll: could not keep the agent shell switch: {e}"),
            None => log::error!("enroll: no config dir for the agent shell switch"),
        },
        other => log::error!("enroll: agent_shell is on or off, not {other}; the switch is left as it was"),
    }
    match enrollment.agent_join.as_str() {
        "" => {}
        on @ ("on" | "off") => match super::data_dir().map(|dir| std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(super::operator::SWITCH_FILE), on))) {
            Some(Ok(())) => log::info!("enroll: sessions started from Rig {on}"),
            Some(Err(e)) => log::error!("enroll: could not keep the switch for sessions started from Rig: {e}"),
            None => log::error!("enroll: no config dir for the switch for sessions started from Rig"),
        },
        other => log::error!("enroll: agent_join is on or off, not {other}; the switch is left as it was"),
    }
}

fn write_token(token: &str) -> std::io::Result<()> {
    let dir = super::data_dir().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("api.token");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        std::fs::write(&path, token.trim())?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    std::fs::write(&path, token.trim())
}
