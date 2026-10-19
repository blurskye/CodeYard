//! Authenticator-app second factor: the encrypted secret, the last accepted time step and the
//! hashed recovery codes. A row in `two_factor_auth` means two-factor sign-in is on.
use crate::backend::db::DbPool;
use anyhow::Result;

/// What the server keeps for a user with two-factor sign-in. The secret is ciphertext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTwoFactor {
    pub secret_encrypted: Vec<u8>,
    pub nonce: Vec<u8>,
    pub last_used_step: Option<i64>,
}

/// Authority that must still be current when changing an authenticator.
#[derive(Clone, Copy)]
pub enum FactorAuthority<'a> {
    Password { session: &'a str, expected: &'a str },
    Administrator { user_id: i64, session: &'a str },
}

native_operation!(crate::backend::db::postgres::users::two_factor::get_two_factor,
    crate::backend::db::sqlite::users::two_factor::get_two_factor,
    fn get_two_factor(pool: &DbPool, user_id: i64) -> Result<Option<StoredTwoFactor>>);

// Stores the secret and the recovery code hashes and sets the account flag, in one transaction.
// False when two-factor sign-in was already on.
native_operation!(crate::backend::db::postgres::users::two_factor::enable_two_factor,
    crate::backend::db::sqlite::users::two_factor::enable_two_factor,
    fn enable_two_factor(pool: &DbPool, user_id: i64, authority: FactorAuthority<'_>, enrollment: &StoredTwoFactor, recovery_hashes: &[String]) -> Result<bool>);

// Records a TOTP time step as used. False when that step or a later one was already used, so a code
// works only once.
native_operation!(crate::backend::db::postgres::users::two_factor::use_totp_step,
    crate::backend::db::sqlite::users::two_factor::use_totp_step,
    fn use_totp_step(pool: &DbPool, user_id: i64, nonce: &[u8], step: i64) -> Result<bool>);

// Replaces every recovery code. False when two-factor sign-in is off.
native_operation!(crate::backend::db::postgres::users::two_factor::replace_recovery_codes,
    crate::backend::db::sqlite::users::two_factor::replace_recovery_codes,
    fn replace_recovery_codes(pool: &DbPool, user_id: i64, authority: FactorAuthority<'_>, recovery_hashes: &[String]) -> Result<bool>);

// Turns two-factor sign-in off and forgets its secret and codes. False when it was already off.
native_operation!(crate::backend::db::postgres::users::two_factor::disable_two_factor,
    crate::backend::db::sqlite::users::two_factor::disable_two_factor,
    fn disable_two_factor(pool: &DbPool, user_id: i64, authority: FactorAuthority<'_>) -> Result<bool>);
