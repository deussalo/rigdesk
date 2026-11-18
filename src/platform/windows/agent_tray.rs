//! RigDesk: the tray in the signed-in session, and the name its notifications go out under.
//!
//! The installer starts the tray as the installing user's process. Over SSH, or from any session but the console's,
//! that process dies with its session, and the PC has no tray until the next sign-in: nobody there is told when an
//! agent joins, and nothing offers to end its session. So `--server`, which runs in the signed-in session, starts one
//! there if none runs.
//!
//! A notification needs an application identity Windows knows. Without one it goes out as "Windows PowerShell"; the
//! tray registers RigDesk's for the signed-in user.

use hbb_common::log;

/// Long enough for the sign-in's own startup entry to have started the tray first.
const TRAY_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
const APP_ID: &str = "Rig.RigDesk";

/// From `--server`: starts the tray in this session after `TRAY_GRACE`, unless one runs there.
pub fn ensure() {
    std::thread::spawn(|| {
        std::thread::sleep(TRAY_GRACE);
        if !super::is_cur_exe_the_installed() {
            return;
        }
        let Some(session) = super::get_current_process_session_id().filter(|s| *s != 0) else { return };
        let (_, _, _, exe) = super::get_install_info();
        let name = format!("{}.exe", crate::get_app_name());
        let trays = crate::platform::get_pids_of_process_with_args(&name, &["--tray"]);
        if trays.iter().any(|pid| super::get_session_id_of_process(pid.as_u32()) == Some(session)) {
            return;
        }
        log::info!("agent: no tray in session {session}; starting one");
        if let Err(e) = super::run_exe_in_session(&exe, vec!["--tray"], session, true) {
            log::error!("agent: the tray could not be started in session {session}: {e}");
        }
    });
}

/// The identity the tray's notifications go out under: RigDesk's, registered for this user on first use, or
/// PowerShell's when the registry refuses it.
pub fn toast_app_id() -> &'static str {
    static REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let registered = *REGISTERED.get_or_init(|| match register() {
        Ok(()) => true,
        Err(e) => {
            log::warn!("tray: RigDesk's notification identity was not registered: {e}");
            false
        }
    });
    if registered {
        APP_ID
    } else {
        tauri_winrt_notification::Toast::POWERSHELL_APP_ID
    }
}

fn register() -> std::io::Result<()> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(format!(r"Software\Classes\AppUserModelId\{APP_ID}"))?;
    key.set_value("DisplayName", &"RigDesk")
}
