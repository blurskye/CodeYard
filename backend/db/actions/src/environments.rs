use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_database::native_operation;
use codeyard_database::DbPool;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Environment administration denied")]
    Denied,
    #[error("Environment or branch policy not found")]
    NotFound,
    #[error("Invalid environment configuration")]
    Invalid,
    #[error("Branch policy already exists")]
    Duplicate,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Environment {
    pub id: i64,
    pub repo_id: i64,
    pub name: String,
    pub name_key: String,
    pub wait_timer: i64,
    pub prevent_self_review: bool,
    pub reviewers: String,
    pub branch_policy: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reviewer {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchPolicy {
    pub protected_branches: bool,
    pub custom_branch_policies: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub wait_timer: Option<i64>,
    pub prevent_self_review: Option<bool>,
    pub reviewers: Option<Vec<Reviewer>>,
    /// Omitted leaves the stored policy alone; an explicit null clears it.
    #[serde(
        default,
        deserialize_with = "codeyard_db_repos::repositories::records::nullable_change"
    )]
    pub deployment_branch_policy: Option<Option<BranchPolicy>>,
}

impl Update {
    pub fn validate(&self, name: &str) -> Result<()> {
        anyhow::ensure!(
            !name.trim().is_empty()
                && name.chars().count() <= 255
                && !name.chars().any(char::is_control),
            Refusal::Invalid
        );
        anyhow::ensure!(
            self.wait_timer
                .is_none_or(|timer| (0..=43200).contains(&timer)),
            Refusal::Invalid
        );
        if let Some(Some(policy)) = &self.deployment_branch_policy {
            anyhow::ensure!(
                policy.protected_branches != policy.custom_branch_policies,
                Refusal::Invalid
            );
        }
        if let Some(reviewers) = &self.reviewers {
            anyhow::ensure!(reviewers.len() <= 6, Refusal::Invalid);
            for (index, reviewer) in reviewers.iter().enumerate() {
                anyhow::ensure!(
                    reviewer.id > 0
                        && matches!(reviewer.kind.as_str(), "User" | "Team")
                        && !reviewers[..index]
                            .iter()
                            .any(|other| other.id == reviewer.id && other.kind == reviewer.kind),
                    Refusal::Invalid
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, sqlx::FromRow)]
pub struct Branch {
    pub id: i64,
    pub environment_id: i64,
    pub name: String,
    pub kind: String,
}

pub(crate) fn validate_branch(name: &str, kind: &str) -> Result<()> {
    anyhow::ensure!(
        !name.trim().is_empty()
            && name.chars().count() <= 255
            && !name.chars().any(char::is_control)
            && matches!(kind, "branch" | "tag"),
        Refusal::Invalid
    );
    Ok(())
}

native_operation!(crate::postgres::environments::get,
    crate::sqlite::environments::get,
    fn get(pool: &DbPool, repo: i64, name: &str) -> Result<Option<Environment>>);
native_operation!(crate::postgres::environments::list,
    crate::sqlite::environments::list,
    fn list(pool: &DbPool, repo: i64, offset: i64, limit: i64) -> Result<Vec<Environment>>);
native_operation!(crate::postgres::environments::count,
    crate::sqlite::environments::count,
    fn count(pool: &DbPool, repo: i64) -> Result<i64>);
native_operation!(crate::postgres::environments::upsert,
    crate::sqlite::environments::upsert,
    fn upsert(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, repo: i64, name: &str, input: &Update) -> Result<Environment>);
native_operation!(crate::postgres::environments::delete,
    crate::sqlite::environments::delete,
    fn delete(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, repo: i64, name: &str) -> Result<()>);
native_operation!(crate::postgres::environments::branches,
    crate::sqlite::environments::branches,
    fn branches(pool: &DbPool, environment: i64, offset: i64, limit: i64) -> Result<Vec<Branch>>);
native_operation!(crate::postgres::environments::branch,
    crate::sqlite::environments::branch,
    fn branch(pool: &DbPool, environment: i64, id: i64) -> Result<Option<Branch>>);
native_operation!(crate::postgres::environments::branch_count,
    crate::sqlite::environments::branch_count,
    fn branch_count(pool: &DbPool, environment: i64) -> Result<i64>);
native_operation!(crate::postgres::environments::save_branch,
    crate::sqlite::environments::save_branch,
    fn save_branch(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, repo: i64, environment: i64, id: Option<i64>, name: &str, kind: &str) -> Result<(Branch, bool)>);
native_operation!(crate::postgres::environments::delete_branch,
    crate::sqlite::environments::delete_branch,
    fn delete_branch(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, repo: i64, environment: i64, id: i64) -> Result<()>);
