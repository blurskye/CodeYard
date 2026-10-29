//! Custom deployment rules and per-attempt App decisions shared with Actions.
use super::{github_apps::App, DbPool};
use anyhow::Result;

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Deployment protection administration denied")]
    Denied,
    #[error("Deployment protection rule not found")]
    NotFound,
    #[error("Invalid deployment protection rule")]
    Invalid,
    #[error("Deployment protection decision conflicts with the current state")]
    Conflict,
}

#[derive(Debug, sqlx::FromRow)]
pub struct Rule {
    pub id: i64,
    pub environment_id: i64,
    pub app_id: i64,
    pub slug: String,
    pub enabled: bool,
}

#[derive(Debug, sqlx::FromRow)]
pub struct Gate {
    pub rule_id: i64,
    pub app_id: i64,
    pub environment_name: String,
    pub state: String,
}

/// The kind and short name environment branch policies match against. Runs store a branch by its
/// short name and a tag by its full ref.
pub(crate) fn deployment_ref(branch: &str) -> (&'static str, &str) {
    match branch.strip_prefix("refs/tags/") {
        Some(tag) => ("tag", tag),
        None => (
            "branch",
            branch.strip_prefix("refs/heads/").unwrap_or(branch),
        ),
    }
}

pub(crate) fn provider(app: &App) -> bool {
    serde_json::from_str::<serde_json::Value>(&app.permissions).is_ok_and(|permissions| {
        crate::backend::identity::github_apps::allows(&permissions, "deployments", true)
    })
}

native_operation!(crate::backend::db::postgres::deployment_protection::rules, crate::backend::db::sqlite::deployment_protection::rules,
    fn rules(pool: &DbPool, environment: i64) -> Result<Vec<Rule>>);
native_operation!(crate::backend::db::postgres::deployment_protection::integrations, crate::backend::db::sqlite::deployment_protection::integrations,
    fn integrations(pool: &DbPool, repo: i64) -> Result<Vec<App>>);
native_operation!(crate::backend::db::postgres::deployment_protection::create, crate::backend::db::sqlite::deployment_protection::create,
    fn create(pool: &DbPool, actor: i64, repo: i64, environment: i64, app: i64) -> Result<Rule>);
native_operation!(crate::backend::db::postgres::deployment_protection::disable, crate::backend::db::sqlite::deployment_protection::disable,
    fn disable(pool: &DbPool, actor: i64, repo: i64, environment: i64, rule: i64) -> Result<()>);
native_operation!(crate::backend::db::postgres::deployment_protection::gates, crate::backend::db::sqlite::deployment_protection::gates,
    fn gates(pool: &DbPool, run: i64) -> Result<Vec<Gate>>);

/// Only an installation credential for the owning App may decide its latest attempt's gate.
#[derive(Default)]
pub struct Review<'a> {
    pub environment_name: &'a str,
    pub state: Option<&'a str>,
    pub comment: Option<&'a str>,
}
impl Review<'_> {
    pub(crate) fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.environment_name.trim().is_empty()
                && self.environment_name.chars().count() <= 255
                && !self.environment_name.chars().any(char::is_control)
                && self
                    .state
                    .is_none_or(|state| matches!(state, "approved" | "rejected"))
                && self
                    .comment
                    .is_none_or(|comment| comment.chars().count() <= 1024)
                && (self.state.is_some()
                    || self
                        .comment
                        .is_some_and(|comment| !comment.trim().is_empty())),
            Refusal::Invalid
        );
        Ok(())
    }
}
native_operation!(crate::backend::db::postgres::deployment_protection::review, crate::backend::db::sqlite::deployment_protection::review,
    fn review(pool: &DbPool, token: i64, repo: i64, run: i64, input: &Review<'_>) -> Result<()>);
