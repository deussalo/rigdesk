//! Per-PC secrets from the install line. The installer, running elevated, writes `enroll.json` beside the installed
//! binary (under Program Files, where only administrators can write) and restarts the service. The server applies it
//! once at startup, before it serves IPC, and deletes it. Nothing here logs or prints a secret.
//!
//! The permanent password is otherwise locked (`disable-change-permanent-password`), so this is the only way it
//! changes: through the install line again.

use std::path::{Path, PathBuf};

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
}

/// The enrollment file of the installed binary, if this process is that binary.
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
}

fn write_token(token: &str) -> std::io::Result<()> {
    let dir = super::data_dir().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no config dir"))?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("api.token"), token.trim())
}
