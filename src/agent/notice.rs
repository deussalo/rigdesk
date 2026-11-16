//! The tray's part in a session the operator started from Rig: two entries while it runs, naming the agent and ending
//! its session, and on Windows a notification when it starts (on Linux the `--server` process sends that itself). The
//! tray process asks `--server` over IPC once a second.

use std::sync::mpsc;

use hbb_common::log;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};

use super::operator::{IPC_END, IPC_SESSION};

struct Holding {
    agent_id: String,
    title: String,
}

pub struct Notice {
    rx: mpsc::Receiver<Option<Holding>>,
    shown: Option<(String, MenuItem, MenuItem)>,
}

/// Starts asking `--server` who holds an operator-started session.
pub fn start() -> Notice {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut last = String::new();
        loop {
            let now = crate::ipc::get_config(IPC_SESSION).ok().flatten().unwrap_or_default();
            if now != last {
                last = now.clone();
                let holding = now.split_once('\n').map(|(id, title)| Holding { agent_id: id.to_string(), title: title.to_string() });
                if tx.send(holding).is_err() {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
    Notice { rx, shown: None }
}

impl Notice {
    /// Shows or takes away the entries as the session starts and ends; called from the tray's event loop.
    pub fn tick(&mut self, menu: &Menu) {
        while let Ok(holding) = self.rx.try_recv() {
            if let Some((_, said, end)) = self.shown.take() {
                menu.remove(&said).ok();
                menu.remove(&end).ok();
            }
            let Some(holding) = holding else { continue };
            let said = MenuItem::new(format!("Rig agent {} is using this computer", holding.title), false, None);
            let end = MenuItem::new("End the agent's session", true, None);
            menu.append_items(&[&said, &end]).ok();
            #[cfg(windows)]
            toast(&holding.title);
            self.shown = Some((holding.agent_id, said, end));
        }
    }

    /// True when the event was the end entry, whose session it then ends.
    pub fn clicked(&self, event: &MenuEvent) -> bool {
        let Some((agent_id, _, end)) = &self.shown else { return false };
        if event.id != *end.id() {
            return false;
        }
        if let Err(e) = crate::ipc::set_config(IPC_END, agent_id.clone()) {
            log::error!("tray: ending the agent's session failed: {e}");
        }
        true
    }
}

#[cfg(windows)]
fn toast(title: &str) {
    use tauri_winrt_notification::{Duration, Sound, Toast};
    let (summary, body) = super::operator::words(title);
    if let Err(e) = Toast::new(Toast::POWERSHELL_APP_ID).title(&summary).text1(&body).sound(Some(Sound::Default)).duration(Duration::Long).show() {
        log::warn!("tray: the notification of a session started from Rig failed: {e}");
    }
}
