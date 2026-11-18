use super::{sleep, Config, Connection, FailureScope, SEND_TIMEOUT_VIDEO};
use base::{invite, message_proto::LoginRequest};
use std::time::Duration;

impl Connection {
    /// RigDesk: a login presenting an invite the person at this device made, proved with Rig's paired key. It stands in
    /// for the password and the click, because the person already said yes by making the invite. It opens a plain
    /// desktop session and never gets around privacy mode or a 2FA requirement.
    pub(super) async fn rig_invite_login(&mut self, lr: LoginRequest) -> bool {
        let device_id = Config::get_id();
        let unfit = if lr.union.is_some() {
            Some("A Rig invite opens a desktop session only")
        } else if lr.username != device_id {
            Some(crate::client::LOGIN_MSG_OFFLINE)
        } else if self.require_2fa.is_some() {
            Some("A Rig invite cannot be used while 2FA is required")
        } else {
            None
        };
        if let Some(reason) = unfit {
            return self.refuse_rig_invite(reason).await;
        }
        if !self.check_privacy_mode_on().await {
            return false;
        }
        let (failure, allowed) = self.check_failure(0).await;
        if !allowed {
            return true;
        }
        let login = invite::Login {
            invite: &lr.rig_invite,
            device_id: &device_id,
            salt: &self.hash.salt,
            challenge: &self.hash.challenge,
            conn: self.inner.id,
        };
        let holder = match invite::redeem(&login) {
            Ok(holder) => holder,
            Err(refusal) => {
                // Past the signature, a wrong agent is Rig proving itself, not a guess, and must not lock Rig's address out.
                if refusal != invite::Refusal::ForOtherAgent {
                    self.update_failure_with_scope(failure, false, 0, FailureScope::Default);
                }
                return self.refuse_rig_invite(refusal.words()).await;
            }
        };
        self.update_failure_with_scope(failure, true, 0, FailureScope::Default);
        hbb_common::log::info!("Rig agent {} joined by invite", holder.agent);
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        crate::agent::invite::relay::joined(self.inner.id, &holder, self.tx_from_cm.clone());
        self.stream.set_send_timeout(SEND_TIMEOUT_VIDEO);
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        self.try_start_cm_ipc();
        if !self.send_logon_response_and_keep_alive().await {
            return false;
        }
        self.try_start_cm(lr.my_id, format!("{} (Rig agent)", holder.title), self.authorized);
        true
    }

    async fn refuse_rig_invite(&mut self, reason: &str) -> bool {
        hbb_common::throttled_log!(Duration::from_secs(60), warn, "Refused a Rig invite: {reason}");
        self.send_login_error(reason).await;
        sleep(1.).await;
        false
    }
}
