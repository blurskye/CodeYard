//! OpenPGP enrollment verifies self-certifications and signing-subkey back signatures.
use crate::backend::db::{
    self,
    users::{
        accounts::{AccountCredential, AccountRestricted},
        gpg_keys::{GpgKey, GpgKeyRefusal, GpgMaterial, NewGpgKey},
    },
    DbPool,
};
use base64::Engine;
use chrono::{DateTime, Utc};
use pgp::{
    composed::{Deserializable, SignedPublicKey, SignedPublicSubKey},
    packet::{Packet, PacketParser, PacketTrait, Signature, SignatureType},
    types::{KeyDetails, Tag},
};
use std::io::Read;

static CRYPTO_WORK: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GpgError {
    #[error("Enter one valid armored OpenPGP public key with self-signed identities")]
    InvalidKey,
    #[error("Key names must be at most 100 characters")]
    InvalidName,
    #[error("This GPG key or one of its subkeys is already registered")]
    AlreadyUsed,
    #[error("At most 100 GPG keys may be registered")]
    Limit,
    #[error("A live unrestricted account credential is required")]
    Denied,
    #[error("GPG key not found")]
    Missing,
    #[error("GPG key processing is temporarily unavailable")]
    Unavailable,
}
fn write_error(error: anyhow::Error) -> GpgError {
    if error.is::<AccountRestricted>() {
        return GpgError::Denied;
    }
    match error.downcast_ref::<GpgKeyRefusal>() {
        Some(GpgKeyRefusal::AlreadyUsed) => GpgError::AlreadyUsed,
        Some(GpgKeyRefusal::Limit) => GpgError::Limit,
        None => GpgError::Unavailable,
    }
}

fn timestamp(seconds: u32) -> DateTime<Utc> {
    DateTime::from_timestamp(i64::from(seconds), 0).expect("OpenPGP timestamps fit chrono")
}
fn signature_live(signature: &Signature, now: i64) -> bool {
    signature.created().is_some_and(|created| {
        let created = i64::from(created.as_secs());
        created <= now
            && signature
                .signature_expiration_time()
                .is_none_or(|duration| {
                    duration.as_secs() == 0 || created + i64::from(duration.as_secs()) > now
                })
    })
}
fn expires_at(created: DateTime<Utc>, signature: &Signature) -> Option<DateTime<Utc>> {
    let key = signature
        .key_expiration_time()
        .filter(|duration| duration.as_secs() != 0)
        .map(|duration| created + chrono::Duration::seconds(i64::from(duration.as_secs())));
    let binding = signature
        .signature_expiration_time()
        .filter(|duration| duration.as_secs() != 0)
        .and_then(|duration| {
            signature.created().map(|created| {
                timestamp(created.as_secs())
                    + chrono::Duration::seconds(i64::from(duration.as_secs()))
            })
        });
    match (key, binding) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}
fn public_packet(key: &impl PacketTrait) -> Result<String, GpgError> {
    let mut bytes = Vec::new();
    key.to_writer_with_header(&mut bytes)
        .map_err(|_| GpgError::InvalidKey)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}
fn material<K: KeyDetails + PacketTrait>(
    key: &K,
    signature: &Signature,
    revoked: bool,
) -> Result<GpgMaterial, GpgError> {
    let flags = signature.key_flags();
    let created_at = timestamp(key.created_at().as_secs());
    Ok(GpgMaterial {
        public_key: public_packet(key)?,
        raw_key: None,
        emails: Vec::new(),
        can_sign: flags.sign() && key.algorithm().can_sign(),
        can_certify: flags.certify() && key.algorithm().can_sign(),
        can_encrypt_comms: flags.encrypt_comms() && key.algorithm().can_encrypt(),
        can_encrypt_storage: flags.encrypt_storage() && key.algorithm().can_encrypt(),
        created_at,
        expires_at: expires_at(created_at, signature),
        revoked,
    })
}
fn uid_email(uid: &str) -> Option<String> {
    let address = if let Some(start) = uid.rfind('<') {
        uid[start + 1..].split_once('>')?.0
    } else {
        uid
    };
    let address = address.trim().to_ascii_lowercase();
    (address.len() <= 255 && address.parse::<lettre::Address>().is_ok()).then_some(address)
}

/// Packet and work limits are checked before cryptographic verification. Private packets and
/// concatenated certificates are rejected even if their outer armor claims to be public.
pub fn parse(armored: &str) -> Result<NewGpgKey, GpgError> {
    let armored = armored.trim();
    if armored.len() > 65536
        || !armored.is_ascii()
        || !armored.starts_with("-----BEGIN PGP PUBLIC KEY BLOCK-----")
        || !armored.ends_with("-----END PGP PUBLIC KEY BLOCK-----")
        || armored.matches("-----BEGIN").count() != 1
    {
        return Err(GpgError::InvalidKey);
    }
    let mut bytes = Vec::new();
    pgp::armor::Dearmor::new(armored.as_bytes())
        .take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| GpgError::InvalidKey)?;
    let mut primary_count = 0;
    let mut packet_count = 0;
    for packet in PacketParser::new(bytes.as_slice()) {
        packet_count += 1;
        if packet_count > 128 {
            return Err(GpgError::InvalidKey);
        }
        match packet.map_err(|_| GpgError::InvalidKey)? {
            Packet::PublicKey(_) => primary_count += 1,
            Packet::PublicSubkey(_)
            | Packet::Signature(_)
            | Packet::UserId(_)
            | Packet::UserAttribute(_)
            | Packet::Trust(_) => (),
            _ => return Err(GpgError::InvalidKey),
        }
    }
    if primary_count != 1 || bytes.len() > 65536 {
        return Err(GpgError::InvalidKey);
    }
    let mut cert =
        SignedPublicKey::from_bytes(bytes.as_slice()).map_err(|_| GpgError::InvalidKey)?;
    if cert.details.users.len() > 32 || cert.public_subkeys.len() > 32 {
        return Err(GpgError::InvalidKey);
    }
    let now = Utc::now().timestamp();
    let primary = &cert.primary_key;
    cert.details.direct_signatures.retain(|signature| {
        signature.typ() == Some(SignatureType::Key) && signature.verify_key(primary).is_ok()
    });
    cert.details.revocation_signatures.retain(|signature| {
        signature.typ() == Some(SignatureType::KeyRevocation)
            && signature.verify_key(primary).is_ok()
    });
    cert.details.user_attributes.clear();
    for user in &mut cert.details.users {
        // Third-party certifications never establish ownership of an identity or key policy.
        user.signatures.retain(|signature| {
            signature
                .verify_certification(primary, Tag::UserId, &user.id)
                .is_ok()
        });
    }
    cert.details
        .users
        .retain(|user| !user.signatures.is_empty());
    let active_users: Vec<_> = cert
        .details
        .users
        .iter()
        .filter_map(|user| {
            let latest = user
                .signatures
                .iter()
                .filter(|signature| {
                    signature
                        .created()
                        .is_some_and(|created| i64::from(created.as_secs()) <= now)
                })
                .max_by_key(|signature| signature.created())?;
            (latest.typ() != Some(SignatureType::CertRevocation)).then_some((user, latest))
        })
        .collect();
    let policy = cert
        .details
        .direct_signatures
        .iter()
        .chain(active_users.iter().map(|(_, signature)| *signature))
        .filter(|signature| {
            signature
                .created()
                .is_some_and(|created| i64::from(created.as_secs()) <= now)
        })
        .max_by_key(|signature| signature.created())
        .ok_or(GpgError::InvalidKey)?;
    let mut root = NewGpgKey {
        fingerprint: format!("{:X}", primary.fingerprint()),
        key_id: hex::encode_upper(primary.legacy_key_id().as_ref()),
        material: material(
            primary,
            policy,
            !cert.details.revocation_signatures.is_empty(),
        )?,
        subkeys: Vec::new(),
    };
    for (user, signature) in active_users {
        if signature_live(signature, now) {
            if let Some(email) = user.id.as_str().and_then(uid_email) {
                root.material.emails.push(email);
            }
        }
    }
    root.material.emails.sort();
    root.material.emails.dedup();
    for subkey in &mut cert.public_subkeys {
        subkey.signatures.retain(|signature| {
            signature
                .verify_subkey_binding(primary, &subkey.key)
                .is_ok()
        });
        let binding = subkey
            .signatures
            .iter()
            .filter(|signature| {
                signature.typ() == Some(SignatureType::SubkeyBinding)
                    && signature
                        .created()
                        .is_some_and(|created| i64::from(created.as_secs()) <= now)
            })
            .max_by_key(|signature| signature.created())
            .ok_or(GpgError::InvalidKey)?;
        SignedPublicSubKey {
            key: subkey.key.clone(),
            signatures: vec![binding.clone()],
        }
        .verify_bindings(primary)
        .map_err(|_| GpgError::InvalidKey)?;
        let revoked = subkey
            .signatures
            .iter()
            .any(|signature| signature.typ() == Some(SignatureType::SubkeyRevocation));
        root.subkeys.push(NewGpgKey {
            fingerprint: format!("{:X}", subkey.fingerprint()),
            key_id: hex::encode_upper(subkey.legacy_key_id().as_ref()),
            material: material(&subkey.key, binding, revoked)?,
            subkeys: Vec::new(),
        });
    }
    root.material.raw_key = Some(
        cert.to_armored_string(Default::default())
            .map_err(|_| GpgError::InvalidKey)?,
    );
    Ok(root)
}

pub async fn enroll(
    pool: &DbPool,
    actor: i64,
    credential: &AccountCredential<'_>,
    name: Option<&str>,
    armored: &str,
) -> Result<GpgKey, GpgError> {
    if name.is_some_and(|name| name.chars().count() > 100 || name.contains('\0')) {
        return Err(GpgError::InvalidName);
    }
    if armored.len() > 65536 {
        return Err(GpgError::InvalidKey);
    }
    // Parsing is bounded to two at a time; a busy upload waits briefly for a slot.
    let permit = tokio::time::timeout(std::time::Duration::from_secs(10), CRYPTO_WORK.acquire())
        .await
        .map_err(|_| GpgError::Unavailable)?
        .map_err(|_| GpgError::Unavailable)?;
    let armored = armored.to_owned();
    let key = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        parse(&armored)
    })
    .await
    .map_err(|_| GpgError::Unavailable)??;
    db::users::gpg_keys::add_gpg_key(pool, actor, credential, name, &key)
        .await
        .map_err(write_error)
}
pub async fn delete(
    pool: &DbPool,
    actor: i64,
    credential: &AccountCredential<'_>,
    id: i64,
) -> Result<(), GpgError> {
    if db::users::gpg_keys::delete_gpg_key(pool, actor, credential, id)
        .await
        .map_err(write_error)?
    {
        Ok(())
    } else {
        Err(GpgError::Missing)
    }
}
