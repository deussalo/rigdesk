//! Reaching a stock PC by its RustDesk ID through our own ID server (hbbs) and relay (hbbr), for PCs off the tailnet.
//!
//! The bridge always asks for the relay: it runs beside hbbr, so a hole punch would gain nothing and adds a path to
//! debug. hbbs vouches for the PC's key (it signs the PC's id and public key with the server key), so unlike direct
//! IP the session is end-to-end encrypted with RustDesk's own key exchange, and a PC whose signed identity does not
//! verify under the configured server key is refused rather than used in the clear.

use base::message_proto::{message, Message, PublicKey};
use hbb_common::protobuf::Message as _;
use hbb_common::rendezvous_proto::{
    punch_hole_response, rendezvous_message, ConnType, IdPk, NatType, PunchHoleRequest, RendezvousMessage,
    RequestRelay,
};
use hbb_common::sodiumoxide::crypto::{box_, secretbox, sign};
use hbb_common::tcp::{FramedStream, KxTranscript};
use hbb_common::{bail, log, ResultType};

/// hbbs's TCP rendezvous port and hbbr's relay port, used when an address names none.
pub const RENDEZVOUS_PORT: u16 = 21116;
pub const RELAY_PORT: u16 = 21117;
/// The client version we announce; hbbs hands a signed key only to clients from 1.1 on.
const VERSION: &str = "1.4.0";
const CONNECT_TIMEOUT_MS: u64 = 10_000;
/// How long the PC gets to answer hbbs's relay request by joining the relay.
const RELAY_WAIT_MS: u64 = 15_000;

/// An ID-server deployment: where hbbs listens and the base64 public key every PC on it was configured with.
#[derive(Clone, Debug)]
pub struct Server {
    pub host: String,
    pub key: String,
    verify: sign::PublicKey,
}

impl Server {
    pub fn parse(host: &str, key: &str) -> Result<Self, String> {
        let host = host.trim();
        if host.is_empty() {
            return Err("the ID server address is empty".into());
        }
        let key = key.trim().to_string();
        let raw = hbb_common::base64::Engine::decode(&hbb_common::base64::engine::general_purpose::STANDARD, &key).map_err(|_| "the ID server key is not base64".to_string())?;
        let verify = sign::PublicKey::from_slice(&raw).ok_or("the ID server key is not a 32-byte ed25519 key")?;
        Ok(Server { host: with_port(host, RENDEZVOUS_PORT), key, verify })
    }
}

/// A RustDesk ID: digits, as RustDesk shows it, spaces dropped; or a custom ID (letters, digits, `-` and `_`).
pub fn parse_id(id: &str) -> Result<String, String> {
    let id: String = id.chars().filter(|c| !c.is_whitespace()).collect();
    if id.len() < 6 || id.len() > 32 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!("{id:?} is not a RustDesk ID"));
    }
    Ok(id)
}

fn with_port(host: &str, port: u16) -> String {
    let bare_v6 = host.matches(':').count() > 1 && !host.starts_with('[');
    if bare_v6 {
        format!("[{host}]:{port}")
    } else if host.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        host.to_string()
    } else {
        format!("{host}:{port}")
    }
}

/// Asks hbbs to have the PC meet us on the relay, joins the relay, and completes the key exchange: the stream that
/// comes back is encrypted and the PC's next message is its login challenge.
pub async fn dial(server: &Server, id: &str) -> ResultType<FramedStream> {
    let mut hbbs = FramedStream::new(server.host.as_str(), None, CONNECT_TIMEOUT_MS).await?;
    let mut ask = RendezvousMessage::new();
    ask.set_punch_hole_request(PunchHoleRequest {
        id: id.to_string(),
        nat_type: NatType::SYMMETRIC.into(),
        licence_key: server.key.clone(),
        conn_type: ConnType::DEFAULT_CONN.into(),
        version: VERSION.into(),
        force_relay: true,
        ..Default::default()
    });
    hbbs.send(&ask).await?;
    let (uuid, relay_server, signed) = loop {
        let Some(bytes) = hbbs.next_timeout(RELAY_WAIT_MS).await else {
            bail!("the PC {id} did not answer through the ID server {} within {} s", server.host, RELAY_WAIT_MS / 1000);
        };
        let Ok(msg) = RendezvousMessage::parse_from_bytes(&bytes?) else { continue };
        match msg.union {
            Some(rendezvous_message::Union::PunchHoleResponse(ph)) => bail!("{}", punch_failure(&ph, id)),
            Some(rendezvous_message::Union::RelayResponse(rr)) => {
                if !rr.refuse_reason.is_empty() {
                    bail!("the PC {id} refused the relay: {}", rr.refuse_reason);
                }
                let signed = rr.pk().to_vec();
                break (rr.uuid, rr.relay_server, signed);
            }
            // hbbs's own key exchange and anything newer we do not speak.
            _ => continue,
        }
    };
    drop(hbbs);
    let their_sign_pk = verified_pk(&signed, &server.verify, id)?;
    if relay_server.is_empty() {
        bail!("the ID server named no relay; start hbbs with -r <relay host>");
    }
    let relay_addr = with_port(&relay_server, RELAY_PORT);
    log::info!("joining the PC {id} on the relay {relay_addr}");
    let mut stream = FramedStream::new(relay_addr.as_str(), None, CONNECT_TIMEOUT_MS).await?;
    let mut join = RendezvousMessage::new();
    join.set_request_relay(RequestRelay {
        id: id.to_string(),
        uuid,
        licence_key: server.key.clone(),
        conn_type: ConnType::DEFAULT_CONN.into(),
        ..Default::default()
    });
    stream.send(&join).await?;
    exchange_keys(&mut stream, &their_sign_pk, id).await?;
    Ok(stream)
}

fn punch_failure(ph: &hbb_common::rendezvous_proto::PunchHoleResponse, id: &str) -> String {
    if !ph.other_failure.is_empty() {
        return format!("the ID server refused: {}", ph.other_failure);
    }
    match ph.failure.enum_value() {
        Ok(punch_hole_response::Failure::ID_NOT_EXIST) => format!("no PC with the ID {id} has registered with the ID server"),
        Ok(punch_hole_response::Failure::OFFLINE) => format!("the PC {id} is offline (not registered with the ID server now)"),
        Ok(punch_hole_response::Failure::LICENSE_MISMATCH) => "the ID server refused the key in --key-file: it is not this hbbs's id_ed25519.pub".into(),
        Ok(punch_hole_response::Failure::LICENSE_OVERUSE) => "the ID server refused: key overuse".into(),
        _ => "the ID server answered a direct path although the bridge asked for the relay".into(),
    }
}

/// The PC's signing key, as hbbs vouches for it.
fn verified_pk(signed: &[u8], server: &sign::PublicKey, id: &str) -> ResultType<sign::PublicKey> {
    if signed.is_empty() {
        bail!("the ID server vouched for no key for the PC {id}: is hbbs running with this key (-k _)?");
    }
    let (signed_id, pk, _) = open_id_pk(signed, server).map_err(|e| hbb_common::anyhow::anyhow!("the PC {id}'s key from the ID server: {e}"))?;
    if signed_id != id {
        bail!("the ID server vouched for {signed_id}, not {id}");
    }
    Ok(sign::PublicKey(pk))
}

fn open_id_pk(signed: &[u8], key: &sign::PublicKey) -> ResultType<(String, [u8; 32], u32)> {
    let opened = sign::verify(signed, key).map_err(|_| hbb_common::anyhow::anyhow!("signature does not verify"))?;
    let res = IdPk::parse_from_bytes(&opened)?;
    let pk: [u8; 32] = res.pk[..].try_into().map_err(|_| hbb_common::anyhow::anyhow!("not a 32-byte key"))?;
    Ok((res.id, pk, res.kx_version))
}

/// The controller's half of RustDesk's handshake: the PC sends its id and session key signed with the key hbbs
/// vouched for; we seal a fresh symmetric key to it.
async fn exchange_keys(stream: &mut FramedStream, their_sign_pk: &sign::PublicKey, id: &str) -> ResultType<()> {
    let Some(bytes) = stream.next_timeout(CONNECT_TIMEOUT_MS).await else {
        bail!("the PC {id} did not start the key exchange on the relay");
    };
    let msg = Message::parse_from_bytes(&bytes?)?;
    let Some(message::Union::SignedId(si)) = msg.union else {
        bail!("the PC {id} opened the relay without signing its identity");
    };
    let (signed_id, their_box_pk, advertised) = open_id_pk(&si.id, their_sign_pk)
        .map_err(|e| hbb_common::anyhow::anyhow!("the PC {id}'s identity on the relay: {e}; refusing the session"))?;
    if signed_id != id {
        bail!("the relay delivered {signed_id}, not {id}; refusing the session");
    }
    let (our_pk, our_sk) = box_::gen_keypair();
    let key = secretbox::gen_key();
    let sealed = box_::seal(&key.0, &box_::Nonce([0u8; box_::NONCEBYTES]), &box_::PublicKey(their_box_pk), &our_sk);
    let picked = hbb_common::tcp::kx_version_for(advertised);
    let mut out = Message::new();
    out.set_public_key(PublicKey {
        asymmetric_value: our_pk.0.to_vec().into(),
        symmetric_value: sealed.into(),
        kx_version: picked,
        ..Default::default()
    });
    stream.send(&out).await?;
    match picked {
        0 => stream.set_key(key),
        _ => stream.set_key_split(
            key,
            true,
            &KxTranscript { initiator_pk: &our_pk.0, responder_pk: &their_box_pk, advertised, picked },
        )?,
    }
    log::info!("key exchange with the PC {id} done (version {picked})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_drop_spaces_and_refuse_junk() {
        assert_eq!(parse_id("123 456 789").unwrap(), "123456789");
        assert!(parse_id("12").is_err());
        assert!(parse_id("a.b.c.d.e").is_err());
    }

    #[test]
    fn addresses_get_the_default_port_once() {
        assert_eq!(with_port("1.2.3.4", 21116), "1.2.3.4:21116");
        assert_eq!(with_port("1.2.3.4:4000", 21116), "1.2.3.4:4000");
        assert_eq!(with_port("rd.example.com", 21117), "rd.example.com:21117");
        assert_eq!(with_port("fd7a::1", 21117), "[fd7a::1]:21117");
    }

    #[test]
    fn a_key_hbbs_did_not_sign_is_refused() {
        let (server_pk, server_sk) = sign::gen_keypair();
        let (other_pk, _) = sign::gen_keypair();
        let (pc_pk, _) = sign::gen_keypair();
        let idpk = IdPk { id: "123456789".into(), pk: pc_pk.0.to_vec().into(), ..Default::default() };
        let signed = sign::sign(&idpk.write_to_bytes().unwrap(), &server_sk);
        assert_eq!(verified_pk(&signed, &server_pk, "123456789").unwrap(), pc_pk);
        assert!(verified_pk(&signed, &other_pk, "123456789").is_err());
        assert!(verified_pk(&signed, &server_pk, "987654321").is_err());
        assert!(verified_pk(&[], &server_pk, "123456789").is_err());
    }
}
