//! Reviews reference the run's pending deployment and its configured environment protections.
use super::{deployments::Deployment, environments::Environment, DbPool};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(sqlx::FromRow)]
pub struct Pending {
    #[sqlx(flatten)]
    pub environment: Environment,
    pub waiting_since: DateTime<Utc>,
    pub triggered_by_id: i64,
    #[sqlx(skip)]
    pub can_approve: bool,
}

#[derive(sqlx::FromRow)]
pub struct Review {
    pub environment_id: i64,
    pub environment_name: String,
    pub actor_id: Option<i64>,
    pub state: String,
    pub comment: String,
}

#[derive(Deserialize)]
pub struct Input {
    pub environment_ids: Vec<i64>,
    pub state: String,
    pub comment: String,
}
impl Input {
    pub(crate) fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.environment_ids.is_empty()
                && self.environment_ids.len() <= 100
                && self
                    .environment_ids
                    .iter()
                    .enumerate()
                    .all(|(i, id)| *id > 0 && !self.environment_ids[..i].contains(id))
                && matches!(self.state.as_str(), "approved" | "rejected")
                && self.comment.len() <= 65536,
            super::action_settings::Refusal::Invalid
        );
        Ok(())
    }
}

pub(crate) fn execution_state(status: &str) -> Result<&'static str> {
    Ok(match status {
        "running" => "in_progress",
        "success" => "success",
        "failed" => "failure",
        "cancelled" => "error",
        _ => anyhow::bail!(super::action_settings::Refusal::Invalid),
    })
}

native_operation!(crate::backend::db::postgres::deployment_reviews::pending,
    crate::backend::db::sqlite::deployment_reviews::pending,
    fn pending(pool: &DbPool, repo: i64, run: i64, actor: Option<i64>) -> Result<Vec<Pending>>);
native_operation!(crate::backend::db::postgres::deployment_reviews::review,
    crate::backend::db::sqlite::deployment_reviews::review,
    fn review(pool: &DbPool, actor: impl Into<super::credentials::Actor>, repo: i64, run: i64, input: &Input) -> Result<Vec<Deployment>>);
native_operation!(crate::backend::db::postgres::deployment_reviews::history,
    crate::backend::db::sqlite::deployment_reviews::history,
    fn history(pool: &DbPool, repo: i64, run: i64) -> Result<Vec<Review>>);
