//! The viewer's one way past the tailnet: a stock RustDesk peer named by its ID is dialled through an ID server and its
//! relay, RustDesk's public ones unless the viewer's Network settings or the connection (`id@server?key=…`) name others.
//! Hosting never comes here: a RigDesk PC registers nowhere and takes peers on its direct port from the tailnet alone
//! (`rendezvous_mediator::start_all`), and CI fails if these hosts and keys appear in any other source file.

/// RustDesk's public ID server and its key, as stock RustDesk ships them.
pub const PUBLIC_ID_SERVER: &str = "rs-ny.rustdesk.com";
pub const PUBLIC_KEY: &str = "OeVuKk5nlHiXp+APNn0Y3pC1Iwpwn44JGqrQCsWqmBw=";

/// A peer named by a RustDesk ID (digits only), or by an ID on a server of its own (`id@server`); anything else is a
/// tailnet host, dialled directly.
pub fn by_id(peer: &str, own_server: bool) -> bool {
    own_server || (!peer.is_empty() && peer.chars().all(|c| c.is_ascii_digit()))
}

/// The ID server the viewer's settings name, or RustDesk's public one when they name none (the settings then read as
/// a bare port, `:21116`).
pub fn id_server(configured: String) -> String {
    if configured.split(':').next().unwrap_or_default().is_empty() {
        hbb_common::socket_client::check_port(PUBLIC_ID_SERVER, hbb_common::config::RENDEZVOUS_PORT)
    } else {
        configured
    }
}

/// The key to trust the ID server with: the one the settings or the connection give, else the public key for the
/// public server.
pub fn key(configured: &str, id_server: &str) -> String {
    if configured.is_empty() && id_server.split(':').next() == Some(PUBLIC_ID_SERVER) {
        PUBLIC_KEY.to_owned()
    } else {
        configured.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_goes_through_the_public_servers_unless_settings_name_others_and_a_tailnet_host_goes_direct() {
        assert!(by_id("123456789", false));
        assert!(by_id("abc", true));
        assert!(!by_id("dell", false));
        assert!(!by_id("dell.tail1234.ts.net", false));
        assert_eq!(id_server(":21116".to_owned()), "rs-ny.rustdesk.com:21116");
        assert_eq!(id_server("hbbs.example.org:21116".to_owned()), "hbbs.example.org:21116");
        assert_eq!(key("", "rs-ny.rustdesk.com:21116"), PUBLIC_KEY);
        assert_eq!(key("", "hbbs.example.org:21116"), "");
        assert_eq!(key("abc=", "rs-ny.rustdesk.com:21116"), "abc=");
    }
}
