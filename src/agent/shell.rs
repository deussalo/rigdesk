//! The agent's shell and clipboard, both bound to the signed-in user. PowerShell runs through the user's session
//! token (never SYSTEM); the clipboard is the user's own.

use hbb_common::log;
use rigdesk_agent::shell::{self, Asked};
use rigdesk_agent::Called;

/// Run one command as the signed-in user and answer its output and how it ended.
pub fn run(asked: &Asked) -> Result<Called, String> {
    let (printed, ended) = crate::platform::run_shell_as_user(&asked.command, asked.timeout).map_err(|e| e.to_string())?;
    shell::answer(&printed, ended)
}

pub fn clipboard_get() -> Result<String, String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;
    cb.get_text().map_err(|e| format!("clipboard read failed: {e}"))
}

pub fn clipboard_set(text: &str) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;
    cb.set_text(text.to_owned()).map_err(|e| {
        log::warn!("agent clipboard set failed: {e}");
        format!("clipboard write failed: {e}")
    })
}
