//! Sigstore bundles are stored once; repository associations and subject indexes carry authority.
use super::DbPool;
use anyhow::Result;
use std::collections::BTreeSet;

pub const MAX_BUNDLE_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BUNDLE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Attestation administration denied")]
    Denied,
    #[error("Attestation not found")]
    Missing,
}

pub struct Bundle {
    pub hash: String,
    pub json: String,
    pub predicate: String,
    pub subjects: BTreeSet<String>,
}

pub fn valid_digest(value: &str) -> bool {
    let Some((algorithm, hex)) = value.split_once(':') else {
        return false;
    };
    let length = match algorithm {
        "sha256" => 64,
        "sha512" => 128,
        _ => return false,
    };
    hex.len() == length
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn valid_predicate(value: &str) -> bool {
    !value.is_empty() && value.len() <= 2048 && !value.chars().any(char::is_control)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Attestation {
    pub id: i64,
    pub repo_id: i64,
    pub bundle: String,
    pub predicate_type: String,
    pub initiator: String,
}

/// Retain the first over-budget row as cursor lookahead, never the entire requested collection.
pub(crate) fn append_page(rows: &mut Vec<Attestation>, bytes: &mut usize, row: Attestation) -> bool {
    *bytes += row.bundle.len() + 256;
    rows.push(row);
    *bytes <= MAX_RESPONSE_BUNDLE_BYTES
}

/// A listing row without its bundle: `subjects` is a JSON array of digests.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Summary {
    pub repository: String,
    pub owner_org_id: Option<i64>,
    pub owner_name: String,
    pub can_delete: bool,
    pub id: i64,
    pub repo_id: i64,
    pub predicate_type: String,
    pub subjects: String,
}

#[derive(Clone, Copy)]
pub enum Owner {
    Organization(i64),
    User(i64),
}
impl Owner {
    pub fn org(self) -> Option<i64> {
        if let Self::Organization(id) = self {
            Some(id)
        } else {
            None
        }
    }
    pub fn user(self) -> Option<i64> {
        if let Self::User(id) = self {
            Some(id)
        } else {
            None
        }
    }
}

pub struct Filter<'a> {
    pub actor: Option<i64>,
    pub session: Option<&'a str>,
    pub owner: Option<Owner>,
    pub repo: Option<i64>,
    pub digests: Option<&'a [String]>,
    pub predicate: Option<&'a str>,
    pub after: i64,
    pub before: i64,
    pub limit: i64,
}

native_operation!(crate::backend::db::postgres::attestations::collect, crate::backend::db::sqlite::attestations::collect,
    fn collect(pool: &DbPool) -> Result<u64>);
native_operation!(crate::backend::db::postgres::attestations::create, crate::backend::db::sqlite::attestations::create,
    fn create(pool: &DbPool, actor: impl Into<super::credentials::Actor>, repo: i64, bundle: &Bundle, initiator: &str) -> Result<i64>);
native_operation!(crate::backend::db::postgres::attestations::list, crate::backend::db::sqlite::attestations::list,
    fn list(pool: &DbPool, filter: &Filter<'_>) -> Result<Vec<Attestation>>);
native_operation!(crate::backend::db::postgres::attestations::summaries, crate::backend::db::sqlite::attestations::summaries,
    fn summaries(pool: &DbPool, filter: &Filter<'_>) -> Result<Vec<Summary>>);
native_operation!(crate::backend::db::postgres::attestations::repositories, crate::backend::db::sqlite::attestations::repositories,
    fn repositories(pool: &DbPool, filter: &Filter<'_>) -> Result<Vec<(i64, String)>>);
native_operation!(crate::backend::db::postgres::attestations::delete, crate::backend::db::sqlite::attestations::delete,
    fn delete(pool: &DbPool, actor: impl Into<super::credentials::Actor>, owner: Owner, ids: &[i64], digests: &[String]) -> Result<u64>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_are_lowercase_hex_of_the_named_length() {
        assert!(valid_digest(&format!("sha256:{}", "a".repeat(64))));
        assert!(valid_digest(&format!("sha512:{}", "b".repeat(128))));
        assert!(!valid_digest("sha256:abc"));
        assert!(!valid_digest(&format!("sha256:{}", "A".repeat(64))));
    }
}
