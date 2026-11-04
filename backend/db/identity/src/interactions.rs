use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_db_core::native_operation;
use codeyard_db_core::queries::users::accounts::AccountCredential;
use codeyard_db_core::DbPool;
use sha2::Digest;
use sha2::Sha256;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ContributorSource {
    pub repo_id: i64,
    pub default_branch: String,
    pub storage_node_id: Option<i64>,
    pub write_version: i64,
}

#[derive(Clone, Copy, Debug)]
pub enum InteractionScope {
    User(i64),
    Organization(i64),
    Repository(i64),
}
impl InteractionScope {
    pub fn column(self) -> &'static str {
        match self {
            Self::User(_) => "user_id",
            Self::Organization(_) => "org_id",
            Self::Repository(_) => "repo_id",
        }
    }
    pub fn id(self) -> i64 {
        match self {
            Self::User(id) | Self::Organization(id) | Self::Repository(id) => id,
        }
    }
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct Restriction {
    pub limit: String,
    pub origin: String,
    pub expires_at: DateTime<Utc>,
}
#[derive(Debug, thiserror::Error)]
pub enum InteractionRefusal {
    #[error("Administration permission is required")]
    Denied,
    #[error("An owner-level interaction restriction overrides repository settings")]
    Conflict,
}
#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionGroup {
    ExistingUsers,
    ContributorsOnly,
    CollaboratorsOnly,
}
impl InteractionGroup {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExistingUsers => "existing_users",
            Self::ContributorsOnly => "contributors_only",
            Self::CollaboratorsOnly => "collaborators_only",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionExpiry {
    #[default]
    OneDay,
    ThreeDays,
    OneWeek,
    OneMonth,
    SixMonths,
}
impl InteractionExpiry {
    pub fn seconds(self) -> i64 {
        86400
            * match self {
                Self::OneDay => 1,
                Self::ThreeDays => 3,
                Self::OneWeek => 7,
                Self::OneMonth => 30,
                Self::SixMonths => 180,
            }
    }
}
#[derive(Clone, Copy, Debug, serde::Deserialize)]
pub struct NewRestriction {
    pub limit: InteractionGroup,
    #[serde(default)]
    pub expiry: InteractionExpiry,
}

pub(crate) fn email_digest(emails: &[String]) -> String {
    let mut emails: Vec<_> = emails
        .iter()
        .map(|email| email.to_ascii_lowercase())
        .collect();
    emails.sort();
    emails.dedup();
    let mut digest = Sha256::new();
    for email in emails {
        digest.update(email.as_bytes());
        digest.update([0]);
    }
    hex::encode(digest.finalize())
}

native_operation!(crate::postgres::interactions::get_restriction,crate::sqlite::interactions::get_restriction,
    fn get_restriction(pool: &DbPool, actor: i64, scope: InteractionScope) -> Result<Option<Restriction>>);
native_operation!(crate::postgres::interactions::set_restriction,crate::sqlite::interactions::set_restriction,
    fn set_restriction(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, scope: InteractionScope, input: Option<NewRestriction>) -> Result<Option<Restriction>>);
native_operation!(crate::postgres::interactions::effective_restriction,crate::sqlite::interactions::effective_restriction,
    fn effective_restriction(pool: &DbPool, repo: i64) -> Result<Option<Restriction>>);
native_operation!(crate::postgres::interactions::proof_required,crate::sqlite::interactions::proof_required,
    fn proof_required(pool: &DbPool, actor: i64, repo: i64) -> Result<bool>);
native_operation!(crate::postgres::interactions::verified_emails,crate::sqlite::interactions::verified_emails,
    fn verified_emails(pool: &DbPool, actor: i64) -> Result<Vec<String>>);
native_operation!(crate::postgres::interactions::proof_current,crate::sqlite::interactions::proof_current,
    fn proof_current(pool: &DbPool, actor: i64, source: &ContributorSource, emails: &[String]) -> Result<Option<bool>>);
native_operation!(crate::postgres::interactions::save_proof,crate::sqlite::interactions::save_proof,
    fn save_proof(pool: &DbPool, actor: i64, source: &ContributorSource, emails: &[String], contributed: bool) -> Result<bool>);
native_operation!(crate::postgres::interactions::source,crate::sqlite::interactions::source,
    fn source(pool: &DbPool,repo: i64) -> Result<Option<ContributorSource>>);
