//! Durable SARIF uploads and alert instances, shared by REST and repository pages.
use super::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Not Found")]
    Missing,
    #[error("Repository write denied")]
    Denied,
    #[error("API rate limit exceeded for SARIF uploads")]
    RateLimited,
    #[error("Analysis specified is not deletable.")]
    DeleteOrder,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Upload {
    pub sequence: i64,
    pub id: String,
    pub repo_id: i64,
    pub actor_id: Option<i64>,
    pub ref_name: String,
    pub commit_sha: String,
    pub sarif: String,
    pub checkout_uri: Option<String>,
    pub processing_status: String,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
pub struct UploadStatus {
    pub processing_status: String,
    pub error: Option<String>,
}

#[derive(Clone, Copy)]
pub enum AlertScope {
    Repository(i64),
    Organization(i64),
}
impl AlertScope {
    pub(crate) fn ids(self) -> (Option<i64>, Option<i64>) {
        match self {
            Self::Repository(id) => (Some(id), None),
            Self::Organization(id) => (None, Some(id)),
        }
    }
}

#[derive(Clone, Copy, Default)]
pub enum AlertSort {
    #[default]
    CreatedDesc,
    CreatedAsc,
    UpdatedDesc,
    UpdatedAsc,
}
impl AlertSort {
    pub(crate) fn sql(self) -> (&'static str, &'static str) {
        match self {
            Self::CreatedDesc => ("created_at", "DESC"),
            Self::CreatedAsc => ("created_at", "ASC"),
            Self::UpdatedDesc => ("updated_at", "DESC"),
            Self::UpdatedAsc => ("updated_at", "ASC"),
        }
    }
}

#[derive(Default)]
pub struct AlertFilters<'a> {
    pub reference: Option<&'a str>,
    pub tool_name: Option<&'a str>,
    pub tool_guid: Option<&'a str>,
    pub state: Option<&'a str>,
    pub severity: Option<&'a str>,
    pub sort: AlertSort,
}

#[derive(sqlx::FromRow)]
pub(crate) struct BranchAlert {
    #[sqlx(flatten)]
    pub alert: Alert,
    pub ref_state: String,
}
impl BranchAlert {
    pub(crate) fn into_alert(mut self) -> Alert {
        self.alert.state = self.ref_state;
        self.alert
    }
}

pub struct NewUpload<'a> {
    pub actor: i64,
    pub repo: i64,
    pub ref_name: &'a str,
    pub commit_sha: &'a str,
    pub sarif: &'a str,
    pub checkout_uri: Option<&'a str>,
}

pub(crate) const ANALYSIS_COLUMNS: &str = "a.id,a.repo_id,a.upload_id,a.upload_sequence,a.ref_name,a.commit_sha,a.tool_key,a.tool,a.category,a.results_count,a.rules_count,a.created_at";
pub(crate) const ANALYSIS_DELETABLE: &str = "NOT EXISTS(SELECT 1 FROM code_scanning_analyses newer WHERE newer.repo_id=a.repo_id AND newer.ref_name=a.ref_name AND newer.tool_key=a.tool_key AND newer.category=a.category AND newer.upload_sequence>a.upload_sequence)";

#[derive(Default)]
pub struct AnalysisFilters<'a> {
    pub reference: Option<&'a str>,
    pub tool_name: Option<&'a str>,
    pub tool_guid: Option<&'a str>,
    pub sarif_id: Option<&'a str>,
    pub pull_request: Option<i32>,
    pub ascending: bool,
}

#[derive(sqlx::FromRow)]
pub struct AnalysisEntry {
    #[sqlx(flatten)]
    pub analysis: Analysis,
    pub deletable: bool,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Analysis {
    pub id: i64,
    pub repo_id: i64,
    pub upload_id: String,
    pub upload_sequence: i64,
    pub ref_name: String,
    pub commit_sha: String,
    pub tool_key: String,
    pub tool: String,
    pub category: String,
    pub results_count: i64,
    pub rules_count: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Alert {
    pub id: i64,
    pub repo_id: i64,
    pub number: i64,
    pub fingerprint: String,
    pub tool_key: String,
    pub tool: String,
    pub rule: String,
    pub state: String,
    pub dismissed_by: Option<i64>,
    pub dismissed_at: Option<DateTime<Utc>>,
    pub dismissed_reason: Option<String>,
    pub dismissed_comment: Option<String>,
    pub fixed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Instance {
    pub alert_id: i64,
    pub ref_name: String,
    pub category: String,
    pub commit_sha: String,
    pub state: String,
    pub message: String,
    pub location: String,
    pub upload_sequence: i64,
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub priority: u8,
    pub source_index: i64,
    pub fingerprint: String,
    pub rule: String,
    pub message: String,
    pub location: String,
}

#[derive(Clone, Debug)]
pub struct Run {
    pub tool_key: String,
    pub tool: String,
    pub category: String,
    pub rules_count: i64,
    pub results_count: i64,
    pub sarif: String,
    pub findings: Vec<Finding>,
}

#[derive(sqlx::FromRow)]
pub struct NewFinding {
    pub number: i64,
    pub rule: String,
    pub message: String,
    pub location: String,
    pub total_count: i64,
}

pub struct Change {
    pub alert: Alert,
    pub action: &'static str,
}

pub(crate) fn changes(before: &[(i64, String)], after: Vec<Alert>) -> Vec<Change> {
    let before: std::collections::HashMap<_, _> = before
        .iter()
        .map(|(id, state)| (*id, state.as_str()))
        .collect();
    after
        .into_iter()
        .filter_map(|alert| {
            let old = before.get(&alert.id);
            let action = match old {
                None => "created",
                Some(old) if *old == alert.state.as_str() => return None,
                Some(_) if alert.state == "fixed" => "fixed",
                Some(_) => "reopened",
            };
            Some(Change { alert, action })
        })
        .collect()
}

pub struct Dismissal<'a> {
    pub state: &'a str,
    pub reason: Option<&'a str>,
    pub comment: Option<&'a str>,
}
impl Dismissal<'_> {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            ["open", "dismissed"].contains(&self.state),
            "Invalid alert state"
        );
        anyhow::ensure!(
            self.state != "dismissed"
                || self.reason.is_some_and(|reason| [
                    "false positive",
                    "won't fix",
                    "used in tests",
                    "mitigated"
                ]
                .contains(&reason)),
            "A dismissal reason is required"
        );
        anyhow::ensure!(
            self.comment.is_none_or(|s| s.chars().count() <= 280),
            "Dismissal comments must not exceed 280 characters"
        );
        Ok(())
    }
}

native_operation!(super::postgres::code_scanning::pull_analyses, super::sqlite::code_scanning::pull_analyses,
    fn pull_analyses(pool: &DbPool, repo: i64, head: &str, number: i32, branch: Option<&str>) -> Result<Vec<Analysis>>);
native_operation!(super::postgres::code_scanning::pull_processing, super::sqlite::code_scanning::pull_processing,
    fn pull_processing(pool: &DbPool, repo: i64, head: &str, number: i32, branch: Option<&str>) -> Result<Option<(String,Option<String>)>>);
native_operation!(super::postgres::code_scanning::baseline_analysis, super::sqlite::code_scanning::baseline_analysis,
    fn baseline_analysis(pool: &DbPool, repo: i64, tool: &str, category: &str, reference: &str, commit: &str) -> Result<Option<i64>>);
native_operation!(super::postgres::code_scanning::new_findings, super::sqlite::code_scanning::new_findings,
    fn new_findings(pool: &DbPool, analysis: i64, baseline: Option<i64>) -> Result<Vec<NewFinding>>);
native_operation!(super::postgres::code_scanning::enqueue, super::sqlite::code_scanning::enqueue,
    fn enqueue(pool: &DbPool, input: &NewUpload<'_>) -> Result<Upload>);
native_operation!(super::postgres::code_scanning::upload_status, super::sqlite::code_scanning::upload_status,
    fn upload_status(pool: &DbPool, repo: i64, id: &str) -> Result<Option<UploadStatus>>);
native_operation!(super::postgres::code_scanning::pending, super::sqlite::code_scanning::pending,
    fn pending(pool: &DbPool) -> Result<Option<Upload>>);
native_operation!(super::postgres::code_scanning::finish, super::sqlite::code_scanning::finish,
    fn finish(pool: &DbPool, upload: &Upload, runs: &[Run], error: Option<&str>) -> Result<Vec<Change>>);
native_operation!(super::postgres::code_scanning::analysis_alerts, super::sqlite::code_scanning::analysis_alerts,
    fn analysis_alerts(pool: &DbPool, analysis: i64) -> Result<Vec<(i64,i64)>>);
native_operation!(super::postgres::code_scanning::analyses, super::sqlite::code_scanning::analyses,
    fn analyses(pool: &DbPool, repo: i64, filters: &AnalysisFilters<'_>, offset: i64, limit: i64) -> Result<Vec<AnalysisEntry>>);
native_operation!(super::postgres::code_scanning::analysis, super::sqlite::code_scanning::analysis,
    fn analysis(pool: &DbPool, repo: i64, id: i64) -> Result<Option<AnalysisEntry>>);
native_operation!(super::postgres::code_scanning::analysis_sarif, super::sqlite::code_scanning::analysis_sarif,
    fn analysis_sarif(pool: &DbPool, repo: i64, id: i64) -> Result<Option<String>>);
native_operation!(super::postgres::code_scanning::alert_page, super::sqlite::code_scanning::alert_page,
    fn alert_page(pool: &DbPool, scope: AlertScope, filters: &AlertFilters<'_>, offset: i64, limit: i64) -> Result<Vec<Alert>>);
native_operation!(super::postgres::code_scanning::alert, super::sqlite::code_scanning::alert,
    fn alert(pool: &DbPool, repo: i64, number: i64) -> Result<Option<Alert>>);
native_operation!(super::postgres::code_scanning::instance, super::sqlite::code_scanning::instance,
    fn instance(pool: &DbPool, alert: i64, reference: Option<&str>) -> Result<Option<Instance>>);
native_operation!(super::postgres::code_scanning::instances, super::sqlite::code_scanning::instances,
    fn instances(pool: &DbPool, alert: i64, reference: Option<&str>, offset: i64, limit: i64) -> Result<Vec<Instance>>);
native_operation!(super::postgres::code_scanning::dismiss, super::sqlite::code_scanning::dismiss,
    fn dismiss(pool: &DbPool, actor: i64, repo: i64, number: i64, input: &Dismissal<'_>) -> Result<Alert>);
native_operation!(super::postgres::code_scanning::delete_analysis, super::sqlite::code_scanning::delete_analysis,
    fn delete_analysis(pool: &DbPool, actor: i64, repo: i64, id: i64, confirm: bool) -> Result<Option<i64>>);
