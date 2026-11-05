//! Cell prefixes shared by ID allocation, credential issuers and request routing.
use crate::tokens::{is_hex_token, HEX_TOKEN_CHARS};
use serde::{Deserialize, Serialize};
use std::{fmt, ops::RangeInclusive, str::FromStr};

pub const SEQUENCE_BITS: u32 = 37;
pub const IDS_PER_PREFIX: i64 = 1_i64 << SEQUENCE_BITS;
pub const MAX_ID: i64 = (1_i64 << 53) - 1;

#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Prefix(u16);

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Invalid {
    #[error("Invalid cell prefix")]
    Prefix,
    #[error("Object ID must be positive and below 2^53")]
    Id,
    #[error("Invalid credential format")]
    Credential,
}

impl Prefix {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u16 {
        self.0
    }

    /// Serial sequences leave offset zero unused, as PostgreSQL's serial IDs do.
    pub fn ids(self) -> RangeInclusive<i64> {
        let base = i64::from(self.0) * IDS_PER_PREFIX;
        base + 1..=base + IDS_PER_PREFIX - 1
    }

    /// A cell reserves its next range once any sequence has used at least 80%.
    pub fn needs_next_prefix(self, last_id: i64) -> bool {
        if Self::for_id(last_id).ok() != Some(self) {
            return false;
        }
        let capacity = IDS_PER_PREFIX - 1;
        last_id - *self.ids().start() + 1 >= capacity - capacity / 5
    }

    pub fn for_id(id: i64) -> Result<Self, Invalid> {
        if !(1..=MAX_ID).contains(&id) {
            return Err(Invalid::Id);
        }
        Ok(Self((id >> SEQUENCE_BITS) as u16))
    }

    /// The complete cookie value, including its prefix, is the stored session identity.
    pub fn cookie(self, secret: &str) -> Result<String, Invalid> {
        validate_secret(secret)?;
        Ok(format!("{self}.{secret}"))
    }

    /// The namespace is an issuer's constant, for example tokens::PAT_PREFIX.
    pub fn token(self, namespace: &'static str, secret: &str) -> Result<String, Invalid> {
        validate_token_secret(namespace, secret)?;
        Ok(format!("{namespace}{self}_{secret}"))
    }
}

impl TryFrom<i64> for Prefix {
    type Error = Invalid;
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        u16::try_from(value).map(Self).map_err(|_| Invalid::Prefix)
    }
}

impl FromStr for Prefix {
    type Err = Invalid;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || value.len() > 5
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || (value.len() > 1 && value.starts_with('0'))
        {
            return Err(Invalid::Prefix);
        }
        value.parse::<u16>().map(Self).map_err(|_| Invalid::Prefix)
    }
}

impl fmt::Display for Prefix {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

fn validate_secret(secret: &str) -> Result<(), Invalid> {
    is_hex_token(secret, HEX_TOKEN_CHARS)
        .then_some(())
        .ok_or(Invalid::Credential)
}

fn validate_token_secret(namespace: &str, secret: &str) -> Result<(), Invalid> {
    if namespace == crate::tokens::INSTALLATION_PREFIX
        && secret.len() == crate::tokens::URL_TOKEN_CHARS
        && secret
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Ok(());
    }
    validate_secret(secret)
}

/// A parsed routing hint grants no authority; the home cell still verifies the secret.
/// Unprefixed values belong to cell zero, as required for an existing installation.
pub struct Secret<'a> {
    pub cell: Prefix,
    pub secret: &'a str,
}

impl fmt::Debug for Secret<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Secret")
            .field("cell", &self.cell)
            .finish_non_exhaustive()
    }
}

impl<'a> Secret<'a> {
    pub fn cookie(value: &'a str) -> Result<Self, Invalid> {
        Self::parse(value, '.', None)
    }

    pub fn token(value: &'a str, namespace: &'static str) -> Result<Self, Invalid> {
        Self::parse(
            value.strip_prefix(namespace).ok_or(Invalid::Credential)?,
            '_',
            Some(namespace),
        )
    }

    fn parse(value: &'a str, separator: char, namespace: Option<&str>) -> Result<Self, Invalid> {
        // Bound the complete input before parsing any attacker-controlled component.
        if value.len() > HEX_TOKEN_CHARS + 6 {
            return Err(Invalid::Credential);
        }
        let validate = |secret| match namespace {
            Some(namespace) => validate_token_secret(namespace, secret),
            None => validate_secret(secret),
        };
        // URL-safe cell-zero Actions secrets may themselves contain underscores.
        if validate(value).is_ok() {
            return Ok(Self {
                cell: Prefix::ZERO,
                secret: value,
            });
        }
        let (cell, secret) = match value.split_once(separator) {
            Some((prefix, secret)) => (prefix.parse().map_err(|_| Invalid::Credential)?, secret),
            None => (Prefix::ZERO, value),
        };
        validate(secret)?;
        Ok(Self { cell, secret })
    }
}

pub fn device_code_prefix(value: &str) -> Option<Prefix> {
    let value = value.trim();
    if value.len() > 15 {
        return None;
    }
    let parts: Vec<_> = value.split('-').collect();
    let (cell, code) = match parts.as_slice() {
        [first, second] => (Prefix::ZERO, [*first, *second]),
        [prefix, first, second] => (prefix.parse().ok()?, [*first, *second]),
        _ => return None,
    };
    code.iter()
        .all(|part| part.len() == 4 && part.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .then_some(cell)
}

pub fn normalize_device_code(code: &str) -> String {
    code.trim().to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::PAT_PREFIX;

    #[test]
    fn ranges_are_disjoint_and_stay_exact_in_javascript() {
        for prefix in [0, 1, 32768, u16::MAX] {
            let cell = Prefix::new(prefix);
            let range = cell.ids();
            for id in [*range.start(), *range.end()] {
                assert_eq!(Prefix::for_id(id), Ok(cell));
                assert_eq!((id as f64) as i64, id);
                assert!(id <= MAX_ID);
            }
        }
        assert!(*Prefix::ZERO.ids().end() < *Prefix::new(1).ids().start());
        for id in [i64::MIN, -1, 0, MAX_ID + 1, i64::MAX] {
            assert_eq!(Prefix::for_id(id), Err(Invalid::Id));
        }
    }

    #[test]
    fn hostile_prefixes_and_credentials_never_route() {
        for value in ["", "00", "01", "+1", "-1", "65536", "1.0", "1\n", " 1", "١"] {
            assert!(value.parse::<Prefix>().is_err(), "{value:?}");
        }
        let secret = "a".repeat(HEX_TOKEN_CHARS);
        let cell = Prefix::new(u16::MAX);
        let token = cell.token(PAT_PREFIX, &secret).unwrap();
        let parsed = Secret::token(&token, PAT_PREFIX).unwrap();
        assert_eq!(parsed.cell, cell);
        assert_eq!(parsed.secret, secret);
        assert!(!format!("{parsed:?}").contains(&secret));
        assert_eq!(
            Secret::cookie(&cell.cookie(&secret).unwrap()).unwrap().cell,
            cell
        );
        assert_eq!(Secret::cookie(&secret).unwrap().cell, Prefix::ZERO);
        assert_eq!(
            Secret::token(&format!("{PAT_PREFIX}{secret}"), PAT_PREFIX)
                .unwrap()
                .cell,
            Prefix::ZERO
        );
        for value in [
            format!("01.{secret}"),
            format!("65536.{secret}"),
            format!("1.{secret}."),
            format!("1.{}", "a".repeat(63)),
            format!("1.{}\0", "a".repeat(63)),
        ] {
            assert!(Secret::cookie(&value).is_err());
        }
    }
}
