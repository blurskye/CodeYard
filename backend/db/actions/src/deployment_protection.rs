use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use codeyard_db_identity::github_apps::App;

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

/// Native environment holds; `d` and `e` are the deployment and its live environment.
pub(crate) fn native_pending_sql(pg: bool) -> String {
    let timer = if pg {
        "d.created_at + make_interval(mins => e.wait_timer::int) > clock_timestamp()"
    } else {
        "strftime('%Y-%m-%dT%H:%M:%f000Z',d.created_at,'+' || e.wait_timer || ' minutes') > strftime('%Y-%m-%dT%H:%M:%f000Z','now')"
    };
    format!("((e.wait_timer>0 AND {timer}) OR (d.status='pending' AND e.reviewers<>'[]'))")
}

/// Pending protection is a presentation phase, not a replacement for branch-policy admission.
pub(crate) fn waiting_sql(pg: bool, run: &str) -> String {
    let native = native_pending_sql(pg);
    format!("({run}.status='queued' AND NOT EXISTS(SELECT 1 FROM deployment_protection_gates g WHERE g.run_id={run}.id AND g.state='rejected') AND (EXISTS(SELECT 1 FROM deployment_protection_gates g WHERE g.run_id={run}.id AND g.state='pending') OR EXISTS(SELECT 1 FROM ci_deployments d JOIN deployment_environments e ON e.repo_id=d.repo_id AND e.name_key=d.environment_key WHERE d.run_id={run}.id AND d.repo_id={run}.repo_id AND d.status IN ('pending','approved') AND {native})))")
}

#[derive(Debug, sqlx::FromRow)]
pub struct Rule {
    pub id: i64,
    pub app_owner_id: i64,
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

pub fn provider(app: &App) -> bool {
    serde_json::from_str::<serde_json::Value>(&app.permissions).is_ok_and(|permissions| {
        codeyard_model::github_apps::allows(&permissions, "deployments", true)
    })
}

native_operation!(crate::postgres::deployment_protection::rules, crate::sqlite::deployment_protection::rules,
    fn rules(pool: &DbPool, environment: i64) -> Result<Vec<Rule>>);
native_operation!(crate::postgres::deployment_protection::integrations, crate::sqlite::deployment_protection::integrations,
    fn integrations(pool: &DbPool, repo: i64) -> Result<Vec<App>>);
native_operation!(crate::postgres::deployment_protection::create, crate::sqlite::deployment_protection::create,
    fn create(pool: &DbPool, actor: impl Into<codeyard_db_core::queries::credentials::Actor>, repo: i64, environment: i64, app: i64) -> Result<Rule>);
native_operation!(crate::postgres::deployment_protection::disable, crate::sqlite::deployment_protection::disable,
    fn disable(pool: &DbPool, actor: impl Into<codeyard_db_core::queries::credentials::Actor>, repo: i64, environment: i64, rule: i64) -> Result<()>);
native_operation!(crate::postgres::deployment_protection::gates, crate::sqlite::deployment_protection::gates,
    fn gates(pool: &DbPool, run: i64) -> Result<Vec<Gate>>);

/// Only an installation credential for the owning App may decide its latest attempt's gate.
#[derive(Default)]
pub struct Review<'a> {
    pub environment_name: &'a str,
    pub state: Option<&'a str>,
    pub comment: Option<&'a str>,
}
impl Review<'_> {
    pub fn validate(&self) -> Result<()> {
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
native_operation!(crate::postgres::deployment_protection::review, crate::sqlite::deployment_protection::review,
    fn review(pool: &DbPool, token: i64, repo: i64, run: i64, input: &Review<'_>) -> Result<()>);
