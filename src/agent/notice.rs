//! The tray's part in a session started from Rig: two entries while it runs, saying which agent joined and how, and
//! Disconnect, which ends its session; on Windows a notification when it starts, pointing at that Disconnect (on Linux
//! the `--server` process sends that itself). The tray process asks `--server` over IPC once a second.

use std::sync::mpsc;

use hbb_common::log;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};

use super::operator::{disconnect, words, JoinedBy, IPC_END, IPC_SESSION};

struct Holding {
    agent_id: String,
    title: String,
    how: JoinedBy,
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
                let holding = super::operator::parse_session(&now)
                    .map(|(id, title, how)| Holding { agent_id: id.to_string(), title: title.to_string(), how });
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
            let (summary, _body) = words(&holding.title, holding.how);
            let said = MenuItem::new(&summary, false, None);
            let end = MenuItem::new(disconnect(&holding.title), true, None);
            menu.append_items(&[&said, &end]).ok();
            #[cfg(windows)]
            toast(&summary, &_body);
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
fn toast(summary: &str, body: &str) {
    use tauri_winrt_notification::{Duration, Sound, Toast};
    if let Err(e) = Toast::new(crate::platform::windows::agent_tray::toast_app_id()).title(summary).text1(body).sound(Some(Sound::Default)).duration(Duration::Long).show() {
        log::warn!("tray: the notification of a session started from Rig failed: {e}");
    }
}
