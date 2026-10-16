//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::{SshKey, UserWithKey};
use crate::backend::db::DbPool;
use anyhow::Result;
use ssh_key::{HashAlg, PublicKey};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SshKeyWriteRefusal {
    #[error("Invalid SSH public key")]
    InvalidKey,
    #[error("This SSH public key is already registered")]
    AlreadyUsed,
    #[error("SSH keys cannot be changed while the account is restricted")]
    AccountRestricted,
    #[error("SSH key enrollment requires a live session")]
    InvalidSession,
}

fn parse_key_parts(key_type: &str, blob: &str) -> Result<PublicKey, SshKeyWriteRefusal> {
    use SshKeyWriteRefusal::InvalidKey;
    if !matches!(
        key_type,
        "ssh-ed25519"
            | "ssh-rsa"
            | "ecdsa-sha2-nistp256"
            | "ecdsa-sha2-nistp384"
            | "ecdsa-sha2-nistp521"
            | "sk-ssh-ed25519@openssh.com"
            | "sk-ecdsa-sha2-nistp256@openssh.com"
    ) || blob.is_empty()
        || blob.len() > 16384
        || !blob
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
    {
        return Err(InvalidKey);
    }
    let key = PublicKey::from_openssh(&format!("{key_type} {blob}")).map_err(|_| InvalidKey)?;
    if key.algorithm().to_string() != key_type {
        return Err(InvalidKey);
    }
    Ok(key)
}

/// One complete-key policy for enrollment and both SSH transports; comments are labels only.
pub(crate) fn parse_public_key(public_key: &str) -> Result<PublicKey, SshKeyWriteRefusal> {
    use SshKeyWriteRefusal::InvalidKey;
    let public_key = public_key.trim();
    if public_key.len() > 32768 || public_key.contains(['\r', '\n', '\0']) {
        return Err(InvalidKey);
    }
    let mut fields = public_key.split_whitespace();
    parse_key_parts(
        fields.next().ok_or(InvalidKey)?,
        fields.next().ok_or(InvalidKey)?,
    )
}

pub(crate) fn canonical_key(key_type: &str, blob: &str) -> Result<String> {
    Ok(parse_key_parts(key_type, blob)?.to_openssh()?)
}

pub(crate) fn valid_stored_key(key: &SshKey) -> bool {
    parse_public_key(&key.public_key).is_ok_and(|parsed| {
        parsed.algorithm().to_string() == key.key_type
            && parsed.fingerprint(HashAlg::Sha256).to_string() == key.fingerprint
    })
}

pub fn calculate_fingerprint(public_key: &str) -> Result<String> {
    Ok(parse_public_key(public_key)?
        .fingerprint(HashAlg::Sha256)
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    const ED: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
    const EC256: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBNulZrf4sHZbqSp2mLJce/VC3rrYC0TuAptZIv0wy1ZZJjvdhkPl+5hakMGej2UxMvk5FVnh+OUOBBtM5N4FFnY=";

    #[test]
    fn fingerprints_match_openssh_and_ignore_public_comments() {
        for input in [
            ED.to_owned(),
            format!("  {ED} user comment with spaces  "),
            format!("\n{ED}\n"),
        ] {
            assert_eq!(
                calculate_fingerprint(&input).unwrap(),
                "SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU"
            );
        }
    }

    #[test]
    fn malformed_keys_options_and_trailing_wire_bytes_are_rejected() {
        for input in [
            "".to_owned(),
            "ssh-ed25519 AAAA".into(),
            "ssh-ed25519 !!!".into(),
            ED.replacen("ssh-ed25519", "ssh-rsa", 1),
            format!("command=bad {ED}"),
            format!("{ED}\n{ED}"),
            format!("{ED}\0"),
            "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA".into(),
        ] {
            assert_eq!(
                parse_public_key(&input).unwrap_err(),
                SshKeyWriteRefusal::InvalidKey,
                "{input}"
            );
        }
        let wire = base64::engine::general_purpose::STANDARD
            .decode(ED.split_whitespace().nth(1).unwrap())
            .unwrap();
        for bytes in [
            &wire[..wire.len() - 1],
            &[wire.as_slice(), b"garbage"].concat(),
        ] {
            let input = format!(
                "ssh-ed25519 {}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            );
            assert_eq!(
                parse_public_key(&input).unwrap_err(),
                SshKeyWriteRefusal::InvalidKey
            );
        }
        let blob = ED.split_whitespace().nth(1).unwrap();
        assert!(canonical_key("ssh-ed25519", &format!("{blob}\n")).is_err());
    }

    #[test]
    fn supported_rsa_ecdsa_and_security_keys_use_the_same_parser() {
        let rsa = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDYe6ruRitXiLJXGkQcJeqR3EVAP9w4+lUEYv6qEVxnoxKP+27ZP59uz3sCO1kstXuPIcyMLCH/uRBbu05ix/1OxYQjIhsz9S9NeoNy3zuMTfvLxV+3G7nOsFoXY2V5nqKHxcFeau3LVv4mf9ZDvO9lzEncNu6rvTGRIMaFAdiApVNWt+E+fJQ53pVL401NG/xsH6zdW3muV2ZAOpQJsVJi9ac4p4BCR3xOk4MLvVWw01Cf1IK46duTXhBZ+A+dN5KFSnJprDtKZYpexh1NNPVrE4OvK9WKgR/GHW9O5MxdGpfT2GeSd0XD/adrsRnIQprHqwV3C5Y5vur12yX1VyYD";
        for input in [rsa.to_owned(), EC256.to_owned(),
            "ecdsa-sha2-nistp384 AAAAE2VjZHNhLXNoYTItbmlzdHAzODQAAAAIbmlzdHAzODQAAABhBKPYPRFTZYNDhja3tf29JdT5eLowdgXgOSaUeElH3J84EUPj5T29Z8FpY2pU3Rt2rRthyoJxrRMoYbu1B79J4IHhaDK/Oewvxb9Fp+EOiDdS/W5wOV4LzdmBk22KK9NmzA==".into(),
            "ecdsa-sha2-nistp521 AAAAE2VjZHNhLXNoYTItbmlzdHA1MjEAAAAIbmlzdHA1MjEAAACFBAFFwDuN8oRVSPfDmU33T494SMJ76TMqZLeR8QkZihoTB6mLm6g1jM1b+7arO6mpXcmIoK7RII/y0mPO2uJbRL5c4ABx7VpKOcImP1kBzEd+ZOKHiN9p9Isq6NjEddSK9crjDOT7ilm2p2KnGJOZ8TrgnEKENExYtEbeg7I4x+eNHOo6/A==".into()] {
            assert!(calculate_fingerprint(&input).unwrap().starts_with("SHA256:"));
        }
        // Hardware-generated public keys add an application string to the normal fields.
        fn field(wire: &mut Vec<u8>, bytes: &[u8]) {
            wire.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            wire.extend_from_slice(bytes);
        }
        let ed = parse_public_key(ED).unwrap().to_bytes().unwrap();
        let ec = parse_public_key(EC256).unwrap().to_bytes().unwrap();
        for (kind, normal, offset) in [
            ("sk-ssh-ed25519@openssh.com", ed, 15),
            ("sk-ecdsa-sha2-nistp256@openssh.com", ec, 23),
        ] {
            let mut wire = Vec::new();
            field(&mut wire, kind.as_bytes());
            wire.extend_from_slice(&normal[offset..]);
            field(&mut wire, b"ssh:fixture");
            let input = format!(
                "{kind} {}",
                base64::engine::general_purpose::STANDARD.encode(wire)
            );
            assert_eq!(
                parse_public_key(&input).unwrap().algorithm().to_string(),
                kind
            );
        }
    }
}

native_operation!(crate::backend::db::postgres::users::ssh_keys::add_ssh_key,
    crate::backend::db::sqlite::users::ssh_keys::add_ssh_key,
    fn add_ssh_key(
    pool: &DbPool,
    user_id: i64,
    name: &str,
    public_key: &str,
) -> Result<SshKey>);

native_operation!(crate::backend::db::postgres::users::ssh_keys::add_ssh_key_for_session,
    crate::backend::db::sqlite::users::ssh_keys::add_ssh_key_for_session,
    fn add_ssh_key_for_session(
    pool: &DbPool,
    user_id: i64,
    session_id: &str,
    name: &str,
    public_key: &str,
) -> Result<SshKey>);

native_operation!(crate::backend::db::postgres::users::ssh_keys::get_user_by_key_fingerprint,
    crate::backend::db::sqlite::users::ssh_keys::get_user_by_key_fingerprint,
    fn get_user_by_key_fingerprint(
    pool: &DbPool,
    fingerprint: &str,
) -> Result<Option<UserWithKey>>);

native_operation!(crate::backend::db::postgres::users::ssh_keys::get_user_by_ssh_key_id,
    crate::backend::db::sqlite::users::ssh_keys::get_user_by_ssh_key_id,
    fn get_user_by_ssh_key_id(pool: &DbPool, key_id: i64) -> Result<Option<UserWithKey>>);

native_operation!(crate::backend::db::postgres::users::ssh_keys::list_user_keys,
    crate::backend::db::sqlite::users::ssh_keys::list_user_keys,
    fn list_user_keys(pool: &DbPool, user_id: i64) -> Result<Vec<SshKey>>);

native_operation!(crate::backend::db::postgres::users::ssh_keys::delete_user_ssh_key,
    crate::backend::db::sqlite::users::ssh_keys::delete_user_ssh_key,
    fn delete_user_ssh_key(pool: &DbPool, user_id: i64, key_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::users::ssh_keys::delete_ssh_key,
    crate::backend::db::sqlite::users::ssh_keys::delete_ssh_key,
    fn delete_ssh_key(pool: &DbPool, key_id: i64) -> Result<bool>);
