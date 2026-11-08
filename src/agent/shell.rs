//! The agent's shell and clipboard, both bound to the signed-in user. PowerShell runs through the user's session
//! token (never SYSTEM); the clipboard is the user's own.

use hbb_common::log;

/// Run a PowerShell script as the signed-in user and return its combined output, truncated to a sane size.
pub fn powershell(script: &str, timeout_ms: u32) -> Result<String, String> {
    const CAP: usize = 256 * 1024;
    let mut out = crate::platform::run_powershell_as_user(script, timeout_ms).map_err(|e| e.to_string())?;
    if out.len() > CAP {
        out.truncate(CAP);
        out.push_str("\n... [output truncated]");
    }
    Ok(out)
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
