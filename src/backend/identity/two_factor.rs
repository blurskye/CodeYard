//! Authenticator-app second factor: RFC 6238 codes (SHA-1, 6 digits, 30 seconds, one step of
//! clock drift either side), one-time recovery codes and the sealed tokens that carry setup and
//! sign-in state through the browser. A sealed token is AES-GCM under the master key, so the
//! browser can carry it but cannot read or change it.

use super::{
    admission::{self, AdmissionError},
    passwords::{self, PasswordError},
    sessions::{self, AccountAuthorityError},
};
use crate::backend::crypto::SecretEncryptor;
use crate::backend::db::{
    self,
    models::User,
    users::two_factor::{FactorAuthority, StoredTwoFactor},
    DbPool,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use data_encoding::BASE32_NOPAD;
use hmac::{Hmac, Mac};
use rand::{seq::SliceRandom, RngCore};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

const STEP_SECONDS: u64 = 30;
const DIGITS: u32 = 6;
const RECOVERY_CODE_COUNT: usize = 10;
/// Unambiguous characters only: no 0/O or 1/I/L.
const RECOVERY_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
/// How long a password that passed, but has not yet had its second factor, stays usable.
pub const PENDING_LOGIN_SECONDS: u64 = 300;
/// How long a setup that has shown its secret waits for the confirming code.
pub const SETUP_SECONDS: u64 = 900;

#[derive(Debug, PartialEq, Eq)]
pub enum TwoFactorError {
    /// The master key is not configured, so secrets and sealed tokens cannot be made.
    KeyUnavailable,
    Unavailable,
    Admission(AdmissionError),
    Password(PasswordError),
    InvalidPassword,
    PasswordRequired,
    InvalidCode,
    AlreadyEnabled,
    NotEnabled,
    /// The sealed setup token is missing, expired, forged or for another account.
    SetupExpired,
    Authority(AccountAuthorityError),
}

/// What the settings page needs to show a new secret. The secret and the URI are the same
/// value in two spellings; `token` is what the confirming form sends back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    pub secret: String,
    pub uri: String,
    pub token: String,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The six-digit RFC 6238 code for one time step (HMAC-SHA1, dynamic truncation).
pub fn totp_code(secret: &[u8], step: u64) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[19] & 0x0f) as usize;
    let binary = u32::from_be_bytes([
        digest[offset] & 0x7f,
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]);
    binary % 10u32.pow(DIGITS)
}

/// The time step a code belongs to, when it is valid one step either side of `now`.
pub fn matching_step(secret: &[u8], code: &str, now: u64) -> Option<i64> {
    let current = (now / STEP_SECONDS) as i64;
    (current - 1..=current + 1).find(|step| {
        format!(
            "{:0width$}",
            totp_code(secret, *step as u64),
            width = DIGITS as usize
        ) == code
    })
}

/// Users type codes with spaces or dashes and in any case. Both recovery codes and TOTP codes
/// are compared in this form.
pub fn normalize_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn is_totp_code(code: &str) -> bool {
    code.len() == DIGITS as usize && code.bytes().all(|b| b.is_ascii_digit())
}

pub fn recovery_code_hash(code: &str) -> String {
    hex::encode(Sha256::digest(normalize_code(code).as_bytes()))
}

fn new_secret() -> Vec<u8> {
    let mut secret = vec![0u8; 20];
    rand::thread_rng().fill_bytes(&mut secret);
    secret
}

/// Ten recovery codes, each shown once as four groups of four characters.
fn new_recovery_codes() -> Vec<String> {
    let mut rng = rand::thread_rng();
    (0..RECOVERY_CODE_COUNT)
        .map(|_| {
            let chars: Vec<char> = (0..16)
                .map(|_| *RECOVERY_ALPHABET.choose(&mut rng).expect("non-empty") as char)
                .collect();
            chars
                .chunks(4)
                .map(|group| group.iter().collect::<String>())
                .collect::<Vec<_>>()
                .join("-")
        })
        .collect()
}

/// The client a sign-in started from. A pending sign-in only completes from the same browser.
pub fn client_digest(user_agent: Option<&str>) -> String {
    hex::encode(Sha256::digest(user_agent.unwrap_or("").as_bytes()))
}

/// The digest of a stored password hash. A pending sign-in records it and only completes while
/// the account still has that password.
pub fn password_digest(password_hash: Option<&str>) -> String {
    hex::encode(Sha256::digest(password_hash.unwrap_or("").as_bytes()))
}

fn seal(encryptor: &SecretEncryptor, payload: &str) -> Result<String, TwoFactorError> {
    let (ciphertext, nonce) = encryptor
        .encrypt(payload.as_bytes())
        .map_err(|_| TwoFactorError::KeyUnavailable)?;
    Ok(format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(nonce),
        URL_SAFE_NO_PAD.encode(ciphertext)
    ))
}

fn open(encryptor: &SecretEncryptor, token: &str) -> Option<String> {
    let (nonce, ciphertext) = token.split_once('.')?;
    let nonce = URL_SAFE_NO_PAD.decode(nonce).ok()?;
    let ciphertext = URL_SAFE_NO_PAD.decode(ciphertext).ok()?;
    let plaintext = encryptor.decrypt(&ciphertext, &nonce).ok()?;
    String::from_utf8(plaintext).ok()
}

/// A sealed token that names its kind and an expiry, then the fields of its kind. Tokens are not
/// reusable across kinds or after they expire.
fn open_fields(
    encryptor: &SecretEncryptor,
    token: &str,
    kind: &str,
    now: u64,
) -> Option<Vec<String>> {
    let payload = open(encryptor, token)?;
    let fields: Vec<String> = payload.split('|').map(str::to_owned).collect();
    if fields.len() < 4 || fields[0] != kind {
        return None;
    }
    let expires: u64 = fields[1].parse().ok()?;
    (expires > now).then_some(fields)
}

/// The sealed token a password sign-in carries to the second-factor page. It names the password
/// that was verified, so a later password change ends the sign-in.
pub fn seal_pending_login(
    encryptor: &SecretEncryptor,
    user_id: i64,
    user_agent: Option<&str>,
    password_hash: Option<&str>,
    now: u64,
) -> Result<String, TwoFactorError> {
    seal(
        encryptor,
        &format!(
            "login|{}|{user_id}|{}|{}",
            now + PENDING_LOGIN_SECONDS,
            client_digest(user_agent),
            password_digest(password_hash)
        ),
    )
}

/// The account and password digest a pending sign-in belongs to, when it is current and the same
/// client sends it.
pub fn open_pending_login(
    encryptor: &SecretEncryptor,
    token: &str,
    user_agent: Option<&str>,
    now: u64,
) -> Option<(i64, String)> {
    let fields = open_fields(encryptor, token, "login", now)?;
    if fields.get(3)? != &client_digest(user_agent) {
        return None;
    }
    Some((fields[2].parse::<i64>().ok()?, fields.get(4)?.clone()))
}

/// Every change to the second factor needs the current password, and a wrong one counts against
/// the same account budget as a wrong sign-in password.
async fn verify_current_password(
    pool: &DbPool,
    user: &User,
    password: &str,
) -> Result<(), TwoFactorError> {
    let attempt = admission::admit_login(pool, None, Some(user.id), &user.username)
        .await
        .map_err(TwoFactorError::Admission)?;
    let verified = passwords::verify_password(password, user.password_hash.as_deref())
        .await
        .map_err(TwoFactorError::Password)?;
    if !verified.valid {
        return Err(TwoFactorError::InvalidPassword);
    }
    admission::forgive_login(pool, &attempt).await;
    Ok(())
}

fn otpauth_uri(username: &str, secret: &str) -> String {
    format!(
        "otpauth://totp/Codeyard:{}?secret={secret}&issuer=Codeyard&algorithm=SHA1&digits=6&period=30",
        crate::webui::encode_component(username)
    )
}

/// Starts a setup: a new secret the user must prove with a code before it is stored.
pub async fn begin_setup(
    pool: &DbPool,
    encryptor: &SecretEncryptor,
    user: &User,
    session_id: &str,
    password: &str,
) -> Result<Setup, TwoFactorError> {
    if user.password_hash.is_none() {
        return Err(TwoFactorError::PasswordRequired);
    }
    verify_current_password(pool, user, password).await?;
    if db::users::two_factor::get_two_factor(pool, user.id)
        .await
        .map_err(|_| TwoFactorError::Unavailable)?
        .is_some()
    {
        return Err(TwoFactorError::AlreadyEnabled);
    }
    let secret = new_secret();
    let encoded = BASE32_NOPAD.encode(&secret);
    let token = seal(
        encryptor,
        &format!(
            "setup|{}|{}|{encoded}|{}|{}",
            now_unix() + SETUP_SECONDS,
            user.id,
            client_digest(Some(session_id)),
            password_digest(user.password_hash.as_deref())
        ),
    )?;
    Ok(Setup {
        uri: otpauth_uri(&user.username, &encoded),
        secret: encoded,
        token,
    })
}

/// The setup a sealed token stands for, so the page can show the same secret again after a wrong
/// code. The token must name this account and must not have expired.
pub fn open_setup(
    encryptor: &SecretEncryptor,
    user: &User,
    session_id: &str,
    token: &str,
    now: u64,
) -> Result<Setup, TwoFactorError> {
    let fields = open_fields(encryptor, token, "setup", now).ok_or(TwoFactorError::SetupExpired)?;
    if user.password_hash.is_none() {
        return Err(TwoFactorError::PasswordRequired);
    }
    if fields[2] != user.id.to_string()
        || fields.get(4) != Some(&client_digest(Some(session_id)))
        || fields.get(5) != Some(&password_digest(user.password_hash.as_deref()))
    {
        return Err(TwoFactorError::SetupExpired);
    }
    let secret = fields[3].clone();
    Ok(Setup {
        uri: otpauth_uri(&user.username, &secret),
        secret,
        token: token.to_owned(),
    })
}

/// Confirms a setup with a code from the new secret. Stores the encrypted secret and the hashed
/// recovery codes, and returns the codes to show once.
pub async fn confirm_setup(
    pool: &DbPool,
    encryptor: &SecretEncryptor,
    user: &User,
    session: &str,
    password: &str,
    token: &str,
    code: &str,
) -> Result<Vec<String>, TwoFactorError> {
    if user.password_hash.is_none() {
        return Err(TwoFactorError::PasswordRequired);
    }
    verify_current_password(pool, user, password).await?;
    let now = now_unix();
    let setup = open_setup(encryptor, user, session, token, now)?;
    let secret = BASE32_NOPAD
        .decode(setup.secret.as_bytes())
        .map_err(|_| TwoFactorError::SetupExpired)?;
    let step =
        matching_step(&secret, &normalize_code(code), now).ok_or(TwoFactorError::InvalidCode)?;
    let (sealed, nonce) = encryptor
        .encrypt(&secret)
        .map_err(|_| TwoFactorError::KeyUnavailable)?;
    let codes = new_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|code| recovery_code_hash(code)).collect();
    let enrollment = StoredTwoFactor {
        secret_encrypted: sealed,
        nonce: nonce.to_vec(),
        last_used_step: Some(step),
    };
    let authority = password_authority(user, session)?;
    let enabled =
        db::users::two_factor::enable_two_factor(pool, user.id, authority, &enrollment, &hashes)
            .await
            .map_err(|_| TwoFactorError::Unavailable)?;
    if !enabled {
        return Err(TwoFactorError::AlreadyEnabled);
    }
    Ok(codes)
}

/// Verifies a TOTP and returns its enrollment and step for atomic session issuance.
pub async fn verify_totp_code(
    pool: &DbPool,
    encryptor: &SecretEncryptor,
    user_id: i64,
    input: &str,
    now: u64,
) -> Result<Option<(Vec<u8>, i64)>, TwoFactorError> {
    let code = normalize_code(input);
    if !is_totp_code(&code) {
        return Ok(None);
    }
    let Some(stored) = db::users::two_factor::get_two_factor(pool, user_id)
        .await
        .map_err(|_| TwoFactorError::Unavailable)?
    else {
        return Ok(None);
    };
    let secret = encryptor
        .decrypt(&stored.secret_encrypted, &stored.nonce)
        .map_err(|_| TwoFactorError::KeyUnavailable)?;
    Ok(matching_step(&secret, &code, now).map(|step| (stored.nonce, step)))
}

/// Replaces the recovery codes after the current password is checked.
pub async fn regenerate_recovery_codes(
    pool: &DbPool,
    user: &User,
    session: &str,
    password: &str,
) -> Result<Vec<String>, TwoFactorError> {
    verify_current_password(pool, user, password).await?;
    let codes = new_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|code| recovery_code_hash(code)).collect();
    if !db::users::two_factor::replace_recovery_codes(
        pool,
        user.id,
        password_authority(user, session)?,
        &hashes,
    )
    .await
    .map_err(|_| TwoFactorError::Unavailable)?
    {
        return Err(TwoFactorError::NotEnabled);
    }
    Ok(codes)
}

/// Turns two-factor sign-in off after the current password is checked.
pub async fn disable(
    pool: &DbPool,
    user: &User,
    session: &str,
    password: &str,
) -> Result<(), TwoFactorError> {
    verify_current_password(pool, user, password).await?;
    turn_off(pool, user.id, password_authority(user, session)?).await
}

fn password_authority<'a>(
    user: &'a User,
    session: &'a str,
) -> Result<FactorAuthority<'a>, TwoFactorError> {
    Ok(FactorAuthority::Password {
        session,
        expected: user
            .password_hash
            .as_deref()
            .ok_or(TwoFactorError::InvalidPassword)?,
    })
}

/// Administrator recovery for a locked-out account. The administrator must be signed in and not
/// restricted; the account's password and codes are not needed.
pub async fn admin_turn_off(
    pool: &DbPool,
    administrator_session: Option<&str>,
    user_id: i64,
) -> Result<(), TwoFactorError> {
    let actor = sessions::administrator(pool, administrator_session)
        .await
        .map_err(TwoFactorError::Authority)?;
    let session = administrator_session.ok_or(TwoFactorError::Authority(
        AccountAuthorityError::NotAuthenticated,
    ))?;
    turn_off(
        pool,
        user_id,
        FactorAuthority::Administrator {
            user_id: actor.id,
            session,
        },
    )
    .await
}

async fn turn_off(
    pool: &DbPool,
    user_id: i64,
    authority: FactorAuthority<'_>,
) -> Result<(), TwoFactorError> {
    if db::users::two_factor::disable_two_factor(pool, user_id, authority)
        .await
        .map_err(|_| TwoFactorError::Unavailable)?
    {
        Ok(())
    } else {
        Err(TwoFactorError::NotEnabled)
    }
}

pub async fn is_enabled(pool: &DbPool, user_id: i64) -> Result<bool, TwoFactorError> {
    Ok(db::users::two_factor::get_two_factor(pool, user_id)
        .await
        .map_err(|_| TwoFactorError::Unavailable)?
        .is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 Appendix B: SHA-1, secret "12345678901234567890", the 8-digit values modulo 10^6.
    #[test]
    fn totp_matches_rfc_6238_vectors() {
        let secret = b"12345678901234567890";
        for (time, expected) in [
            (59u64, 287082u32),
            (1111111109, 81804),
            (1111111111, 50471),
            (1234567890, 5924),
            (2000000000, 279037),
            (20000000000, 353130),
        ] {
            assert_eq!(totp_code(secret, time / 30), expected, "time {time}");
        }
    }

    #[test]
    fn a_code_is_valid_one_step_either_side_only() {
        let secret = b"12345678901234567890";
        let now = 1111111109u64;
        let step = (now / 30) as i64;
        let code = format!("{:06}", totp_code(secret, now / 30));
        assert_eq!(matching_step(secret, &code, now), Some(step));
        assert_eq!(matching_step(secret, &code, now + 30), Some(step));
        // Two steps on, the code is outside the one-step window.
        assert_eq!(matching_step(secret, &code, now + 60), None);
    }

    #[test]
    fn codes_are_normalized_and_recovery_hashes_ignore_format() {
        assert_eq!(normalize_code(" abcd-ef12 "), "ABCDEF12");
        assert_eq!(
            recovery_code_hash("abcd-efgh"),
            recovery_code_hash("ABCDEFGH")
        );
        assert!(is_totp_code("123456"));
        assert!(!is_totp_code("12345"));
    }

    #[test]
    fn recovery_codes_have_four_groups_and_no_lookalikes() {
        for code in new_recovery_codes() {
            assert_eq!(code.len(), 19);
            assert_eq!(code.split('-').count(), 4);
            assert!(!code.contains(['0', 'O', '1', 'I', 'L']));
        }
    }

    #[test]
    fn sealed_tokens_name_their_kind_expiry_client_and_password() {
        let encryptor = SecretEncryptor::new(&[7u8; 32]);
        let token =
            seal_pending_login(&encryptor, 42, Some("Firefox"), Some("hash-one"), 1000).unwrap();
        assert_eq!(
            open_pending_login(&encryptor, &token, Some("Firefox"), 1000),
            Some((42, password_digest(Some("hash-one"))))
        );
        assert_eq!(
            open_pending_login(&encryptor, &token, Some("Chrome"), 1000),
            None
        );
        assert_eq!(
            open_pending_login(
                &encryptor,
                &token,
                Some("Firefox"),
                1000 + PENDING_LOGIN_SECONDS
            ),
            None
        );
        assert_eq!(
            open_pending_login(&encryptor, &format!("{token}x"), Some("Firefox"), 1000),
            None
        );
        let other = SecretEncryptor::new(&[8u8; 32]);
        assert_eq!(
            open_pending_login(&other, &token, Some("Firefox"), 1000),
            None
        );
    }
}
