use super::{sleep, Config, Connection, FailureScope, SEND_TIMEOUT_VIDEO};
use base::{
    message_proto::{LoginRequest, Message},
    rig_knock,
};
use hbb_common::password_security::{approve_mode, ApproveMode};
use std::time::Duration;

impl Connection {
    /// RigDesk Android: every connection's greeting carries its own nonce, which a knock's proof must cover.
    pub(super) fn rig_knock_hello(&mut self, greeting: &mut Message) {
        self.hash.rig_knock_nonce = rig_knock::fresh_nonce().into();
        greeting.mut_hash().rig_knock_nonce = self.hash.rig_knock_nonce.clone();
    }

    /// RigDesk Android: a passwordless login calling itself Rig. Proved with Rig's paired key it asks the person at the
    /// phone, who alone can let it in; anything else is refused before the person sees it. The nonce goes with the first
    /// try, so the proof works once.
    pub(super) async fn rig_knock_login(&mut self, lr: LoginRequest) -> bool {
        rig_knock::left(self.inner.id);
        let nonce = std::mem::take(&mut self.hash.rig_knock_nonce);
        if lr.union.is_some() {
            return self.refuse_rig_knock("A Rig agent's request opens a desktop session only").await;
        }
        if !crate::common::is_direct_ip_access(&lr.username) && lr.username != Config::get_id() {
            return self.refuse_rig_knock(crate::client::LOGIN_MSG_OFFLINE).await;
        }
        if !self.check_privacy_mode_on().await {
            return false;
        }
        let (failure, allowed) = self.check_failure(0).await;
        if !allowed {
            return true;
        }
        let Ok(dialer) = self.ip.parse() else {
            return self.refuse_rig_knock(rig_knock::Refusal::BadProof.words()).await;
        };
        let login = rig_knock::Login {
            device: &lr.username,
            salt: &self.hash.salt,
            challenge: &self.hash.challenge,
            nonce: &nonce,
            dialer,
        };
        let proved = match rig_knock::verify(lr.rig_knock.as_ref(), &login) {
            Ok(proved) => proved,
            Err(refusal) => {
                match refusal {
                    rig_knock::Refusal::OtherDialer => hbb_common::throttled_log!(
                        Duration::from_secs(60),
                        warn,
                        "Rig knock: good signature for {}, but the connection comes from {}",
                        lr.rig_knock.dialer,
                        self.ip
                    ),
                    rig_knock::Refusal::BadProof => hbb_common::throttled_log!(
                        Duration::from_secs(60),
                        warn,
                        "Rig knock: the proof does not verify with the paired key for this connection"
                    ),
                    _ => {}
                }
                return self.refuse_rig_knock(refusal.words()).await;
            }
        };
        self.update_failure_with_scope(failure, true, 0, FailureScope::Default);
        hbb_common::log::info!("Rig agent {} asks to join", proved.agent_id);
        rig_knock::mark_proved(self.inner.id);
        self.stream.set_send_timeout(SEND_TIMEOUT_VIDEO);
        self.try_start_cm(rig_knock::PEER_ID.to_owned(), proved.name, false);
        self.send_login_error(crate::client::LOGIN_MSG_NO_PASSWORD_ACCESS).await;
        true
    }

    pub(super) fn rig_knock_listed_as(&self, peer_id: String, name: String, authorized: bool) -> Option<(String, String)> {
        rig_knock::listed_as(peer_id, name, authorized, rig_knock::proved(self.inner.id))
    }

    /// Whether the yes that came from the app or the notification counts. This is where it is enforced, whatever they show.
    pub(super) fn rig_knock_may_authorize(&self) -> bool {
        let allowed = rig_knock::may_authorize(
            rig_knock::proved(self.inner.id),
            rig_knock::claims_rig(&self.lr.my_id, &self.lr.my_name),
            approve_mode() != ApproveMode::Password,
        );
        if !allowed {
            hbb_common::throttled_log!(Duration::from_secs(60), warn, "Refused to accept a login that has not proved it may be accepted");
        }
        allowed
    }

    async fn refuse_rig_knock(&mut self, reason: &str) -> bool {
        hbb_common::throttled_log!(Duration::from_secs(60), warn, "Refused a Rig agent's request: {reason}");
        self.send_login_error(reason).await;
        sleep(1.).await;
        false
    }
}
