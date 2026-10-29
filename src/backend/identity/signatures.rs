//! Commit and tag signatures are checked against the signer's registered GPG and SSH signing
//! keys, and the committer address must be a verified address of the key's owner. The reasons
//! are GitHub's verification reasons.
use crate::backend::db::{self, models::User, DbPool};
use anyhow::Result;
use base64::Engine;
use pgp::{
    composed::{Deserializable, DetachedSignature},
    crypto::hash::HashAlgorithm,
    packet::{Packet, PacketParser, SignatureType},
};
use serde_json::{json, Value};

/// Armored signatures larger than this are not parsed.
const MAX_SIGNATURE: usize = 64 * 1024;

/// A signed Git object: its signature, the bytes the signature covers, and the committer's or
/// tagger's address.
#[derive(Clone, Copy)]
pub struct Signed<'a> {
    pub signature: Option<&'a str>,
    pub payload: Option<&'a [u8]>,
    pub email: Option<&'a str>,
}

/// The `verification` object of commit and tag documents.
pub async fn verification(pool: &DbPool, signed: Signed<'_>) -> Result<Value> {
    let reason = check(pool, signed).await?;
    let verified = reason == "valid";
    Ok(json!({
        "verified": verified, "reason": reason,
        "signature": signed.signature, "payload": signed.payload.map(String::from_utf8_lossy),
        "verified_at": verified.then(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
    }))
}

/// GitHub's verification reason; `valid` is the only verified one.
pub async fn check(pool: &DbPool, signed: Signed<'_>) -> Result<&'static str> {
    let Some(signature) = signed.signature else {
        return Ok("unsigned");
    };
    let Some(payload) = signed.payload else {
        return Ok("malformed_signature");
    };
    if signature.len() > MAX_SIGNATURE {
        return Ok("malformed_signature");
    }
    let email = signed.email.unwrap_or_default();
    let armored = signature.trim();
    if armored.starts_with("-----BEGIN PGP SIGNATURE-----") {
        gpg(pool, armored, payload, email).await
    } else if armored.starts_with("-----BEGIN SSH SIGNATURE-----") {
        ssh(pool, armored, payload, email).await
    } else if armored.starts_with("-----BEGIN SIGNED MESSAGE-----") {
        Ok("unknown_signature_type")
    } else {
        Ok("malformed_signature")
    }
}

/// Every address of the account and whether it is verified. The generated noreply address
/// belongs to the account, so it always counts as verified.
pub async fn addresses(pool: &DbPool, user: &User) -> Result<Vec<(String, bool)>> {
    let mut addresses: Vec<(String, bool)> = user
        .email
        .iter()
        .map(|email| (email.clone(), user.email_verified))
        .collect();
    addresses.extend(
        db::users::emails::list_emails(pool, user.id)
            .await?
            .into_iter()
            .map(|email| (email.email, email.verified)),
    );
    addresses.push((format!("{}@users.noreply", user.username), true));
    Ok(addresses)
}

async fn owner_address(pool: &DbPool, owner: i64, email: &str) -> Result<&'static str> {
    let Some(user) = db::users::accounts::get_user_by_id(pool, owner).await? else {
        return Ok("no_user");
    };
    let matches: Vec<bool> = addresses(pool, &user)
        .await?
        .into_iter()
        // Generated addresses name a username, which account lookup matches exactly.
        .filter(|(address, _)| super::credentials::email_matches(address, email))
        .map(|(_, verified)| verified)
        .collect();
    // A verified match wins even when an unverified entry with the same text comes first.
    Ok(if matches.iter().any(|verified| *verified) {
        "valid"
    } else if matches.is_empty() {
        "no_user"
    } else {
        "unverified_email"
    })
}

fn pgp_verifies(signature: &DetachedSignature, public_key: &str, payload: &[u8]) -> bool {
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(public_key) else {
        return false;
    };
    match PacketParser::new(bytes.as_slice()).next() {
        Some(Ok(Packet::PublicKey(key))) => signature.verify(&key, payload).is_ok(),
        Some(Ok(Packet::PublicSubkey(key))) => signature.verify(&key, payload).is_ok(),
        _ => false,
    }
}

async fn gpg(pool: &DbPool, armored: &str, payload: &[u8], email: &str) -> Result<&'static str> {
    let Ok((signature, _)) = DetachedSignature::from_string(armored) else {
        return Ok("malformed_signature");
    };
    if !matches!(signature.signature.typ(), Some(SignatureType::Binary | SignatureType::Text)) {
        return Ok("unknown_signature_type");
    }
    if !matches!(signature.signature.hash_alg(), Some(HashAlgorithm::Sha224 | HashAlgorithm::Sha256 | HashAlgorithm::Sha384 | HashAlgorithm::Sha512 | HashAlgorithm::Sha3_256 | HashAlgorithm::Sha3_512)) {
        return Ok("invalid");
    }
    let issuer = match signature.signature.issuer_fingerprint().first() {
        Some(fingerprint) => format!("{fingerprint:X}"),
        None => match signature.signature.issuer_key_id().first() {
            Some(id) => {
                let id: &[u8] = id.as_ref();
                hex::encode_upper(id)
            }
            None => return Ok("malformed_signature"),
        },
    };
    let keys = db::users::gpg_keys::find_signing_keys(pool, &issuer).await?;
    if keys.is_empty() {
        return Ok("unknown_key");
    }
    let Some(key) = keys
        .into_iter()
        .find(|key| pgp_verifies(&signature, &key.material.public_key, payload))
    else {
        return Ok("invalid");
    };
    // Identities and revocation of the whole certificate live on the primary key.
    let primary = match key.primary_key_id {
        Some(id) => match db::users::gpg_keys::get_gpg_key(pool, key.user_id, id).await? {
            Some(primary) => primary,
            None => return Ok("unknown_key"),
        },
        None => key.clone(),
    };
    if key.material.revoked || primary.material.revoked {
        return Ok("invalid");
    }
    if !key.material.can_sign {
        return Ok("not_signing_key");
    }
    let signed_at = signature
        .signature
        .created()
        .map(|created| i64::from(created.as_secs()));
    let expired = |expires: Option<chrono::DateTime<chrono::Utc>>| {
        expires.is_some_and(|expires| signed_at.is_none_or(|signed| signed >= expires.timestamp()))
    };
    if expired(key.material.expires_at) || expired(primary.material.expires_at) {
        return Ok("expired_key");
    }
    if !primary
        .material
        .emails
        .iter()
        .any(|identity| identity.eq_ignore_ascii_case(email))
    {
        return Ok("bad_email");
    }
    owner_address(pool, key.user_id, email).await
}

async fn ssh(pool: &DbPool, armored: &str, payload: &[u8], email: &str) -> Result<&'static str> {
    let Ok(signature) = ssh_key::SshSig::from_pem(armored) else {
        return Ok("malformed_signature");
    };
    // ssh-key verifies SK signature mathematics but does not enforce user presence.
    if matches!(signature.public_key(), ssh_key::public::KeyData::SkEd25519(_) | ssh_key::public::KeyData::SkEcdsaSha2NistP256(_)) {
        return Ok("unknown_signature_type");
    }
    let fingerprint = signature
        .public_key()
        .fingerprint(ssh_key::HashAlg::Sha256)
        .to_string();
    let Some(key) = db::users::signing_keys::find_signing_key(pool, &fingerprint).await? else {
        return Ok("unknown_key");
    };
    let Ok(public_key) = ssh_key::PublicKey::from_openssh(&key.key) else {
        return Ok("invalid");
    };
    if public_key.verify("git", payload, &signature).is_err() {
        return Ok("invalid");
    }
    owner_address(pool, key.user_id, email).await
}
