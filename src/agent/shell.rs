//! The agent's shell and clipboard, both bound to the signed-in user. On Windows PowerShell runs through the user's
//! session token (never SYSTEM); on Linux the user's login shell runs in the user's own `--server` (never root). The
//! clipboard is the user's own.

use hbb_common::log;
use rigdesk_agent::shell::{self, Asked};
use rigdesk_agent::Called;

/// Run one command as the signed-in user and answer its output and how it ended.
pub fn run(asked: &Asked) -> Result<Called, String> {
    let (printed, ended) = crate::platform::run_shell_as_user(&asked.command, asked.timeout).map_err(|e| e.to_string())?;
    shell::answer(&printed, ended)
}

/// One clipboard for the life of the process: on X11 what is set stays only while its owner lives, so a clipboard made
/// and dropped per call would lose the text it was given at once.
fn with_clipboard<T>(work: impl FnOnce(&mut arboard::Clipboard) -> Result<T, String>) -> Result<T, String> {
    static CLIPBOARD: std::sync::Mutex<Option<arboard::Clipboard>> = std::sync::Mutex::new(None);
    let mut held = CLIPBOARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if held.is_none() {
        *held = Some(arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?);
    }
    work(held.as_mut().expect("set above"))
}

pub fn clipboard_get() -> Result<String, String> {
    with_clipboard(|cb| cb.get_text().map_err(|e| format!("clipboard read failed: {e}")))
}

pub fn clipboard_set(text: &str) -> Result<(), String> {
    with_clipboard(|cb| {
        cb.set_text(text.to_owned()).map_err(|e| {
            log::warn!("agent clipboard set failed: {e}");
            format!("clipboard write failed: {e}")
        })
    })
}
