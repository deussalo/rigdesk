//! An invite a device made, redeemed for one Rig agent: the login carries Rig's signature in place of a password (see
//! `base::invite`). Rig's pairing key is a 32-byte seed, base64, in a 0600 file; the device holds only its public half.

use std::io::Write;
use std::path::Path;

use base::{
    invite::{secret_hash, Claim},
    message_proto::{Hash, RigInvite},
};
use hbb_common::{
    base64::{engine::general_purpose::STANDARD, Engine},
    rand::{thread_rng, RngCore},
    serde_json,
    sodiumoxide::crypto::sign,
};

pub struct Invite {
    grant: String,
    secret_hash: [u8; 32],
    agent_id: String,
    agent_title: String,
    key: sign::SecretKey,
}

fn read_key(path: &Path) -> Result<(sign::PublicKey, sign::SecretKey), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read the pairing key file {}: {e}", path.display()))?;
    let seed = STANDARD.decode(text.trim()).ok().and_then(|raw| sign::Seed::from_slice(&raw));
    let seed = seed.ok_or_else(|| format!("the pairing key file {} holds no key", path.display()))?;
    Ok(sign::keypair_from_seed(&seed))
}

fn create_key(path: &Path) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("cannot create the pairing key file {}: {e}", path.display());
    let mut seed = [0u8; sign::SEEDBYTES];
    thread_rng().fill_bytes(&mut seed);
    let temp = path.with_extension(format!("new-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = options.open(&temp).and_then(|mut file| file.write_all(STANDARD.encode(seed).as_bytes()));
    // A link never replaces a file, so two first calls end up with one key.
    let linked = written.and_then(|()| match std::fs::hard_link(&temp, path) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    });
    std::fs::remove_file(&temp).ok();
    linked.map_err(failed)
}

/// Rig's public key, base64, the key being made first if the file does not exist.
pub fn pair_key(path: &Path) -> Result<String, String> {
    if !path.exists() {
        create_key(path)?;
    }
    let (public, _) = read_key(path)?;
    Ok(STANDARD.encode(public.0))
}

impl Invite {
    /// `file` holds `{grant, secret, agent_id, agent_title}`, from the link and from Rig's own record of the agent.
    pub fn load(file: &Path, key_file: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(file).map_err(|e| format!("cannot read the invite file {}: {e}", file.display()))?;
        let given: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("the invite file {} is not JSON: {e}", file.display()))?;
        let field = |name: &str| {
            let value = given[name].as_str().filter(|value| !value.is_empty());
            value.map(str::to_owned).ok_or_else(|| format!("the invite file {} has no {name}", file.display()))
        };
        let (_, key) = read_key(key_file)?;
        Ok(Invite {
            grant: field("grant")?,
            secret_hash: secret_hash(&field("secret")?),
            agent_id: field("agent_id")?,
            agent_title: field("agent_title")?,
            key,
        })
    }

    pub fn display_name(&self) -> String {
        format!("{} (Rig agent)", self.agent_title)
    }

    pub fn proof(&self, device_id: &str, hash: &Hash) -> RigInvite {
        let claim = Claim {
            device_id,
            grant: &self.grant,
            secret_hash: self.secret_hash,
            salt: &hash.salt,
            challenge: &hash.challenge,
            agent_id: &self.agent_id,
            agent_title: &self.agent_title,
        };
        RigInvite {
            grant: self.grant.clone(),
            signature: sign::sign_detached(&claim.message(), &self.key).to_bytes().to_vec().into(),
            agent_id: self.agent_id.clone(),
            agent_title: self.agent_title.clone(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rigdesk-invite-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_pairing_key_is_made_once_and_only_its_owner_reads_it() {
        let dir = dir("key");
        let path = dir.join("pair.key");
        let public = pair_key(&path).unwrap();
        assert_eq!(pair_key(&path).unwrap(), public);
        assert_eq!(STANDARD.decode(&public).unwrap().len(), 32);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_proof_verifies_with_the_public_key_for_this_login_alone() {
        let dir = dir("proof");
        let (key, file) = (dir.join("pair.key"), dir.join("invite.json"));
        let public = STANDARD.decode(pair_key(&key).unwrap()).unwrap();
        let public = sign::PublicKey::from_slice(&public).unwrap();
        std::fs::write(&file, r#"{"grant":"g1","secret":"s1","agent_id":"agent_a","agent_title":"Builder"}"#).unwrap();
        let invite = Invite::load(&file, &key).unwrap();
        let hash = Hash { salt: "salt".into(), challenge: "challenge".into(), ..Default::default() };

        let proof = invite.proof("123456789", &hash);
        let signature = sign::Signature::from_bytes(&proof.signature).unwrap();
        let mut claim = Claim {
            device_id: "123456789",
            grant: "g1",
            secret_hash: secret_hash("s1"),
            salt: "salt",
            challenge: "challenge",
            agent_id: "agent_a",
            agent_title: "Builder",
        };
        assert!(sign::verify_detached(&signature, &claim.message(), &public));
        claim.challenge = "another";
        assert!(!sign::verify_detached(&signature, &claim.message(), &public));
        std::fs::remove_dir_all(&dir).ok();
    }
}
