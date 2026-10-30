//! Publisher-owned checks. Native CI keeps its own queue and cannot be overwritten here.
use super::DbPool;
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{types::Json, FromRow};

#[derive(Clone, Copy)]
pub enum Credential<'a> {
    Token(i64),
    Session(&'a str),
}

#[derive(FromRow)]
pub struct Suite {
    pub id: i64,
    pub repo_id: i64,
    pub publisher: String,
    pub app_id: Option<i64>,
    /// Set for the suite GitHub Actions publishes for a workflow run. Its stored status and
    /// conclusion are unused; the run's own state is the suite's.
    pub ci_run_id: Option<i64>,
    pub head_sha: String,
    pub head_branch: Option<String>,
    pub status: String,
    pub conclusion: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const ACTIONS_APP_ID: i64 = 15368;

#[derive(Default)]
pub struct AutomaticSuites {
    pub after: i64,
    pub has_next: bool,
    pub suites: Vec<Suite>,
}

pub struct RunChange {
    pub run: Run,
    pub completed_suite: Option<Suite>,
}

#[derive(FromRow)]
pub struct Run {
    pub id: i64,
    pub suite_id: i64,
    pub repo_id: i64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub data: Json<RunData>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct RunData {
    pub details_url: Option<String>,
    pub external_id: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub text: Option<String>,
    pub images: Vec<Image>,
    pub actions: Vec<Action>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Annotation {
    pub path: String,
    pub start_line: i32,
    pub end_line: i32,
    pub start_column: Option<i32>,
    pub end_column: Option<i32>,
    pub annotation_level: String,
    pub title: Option<String>,
    pub message: String,
    pub raw_details: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Image {
    pub alt: String,
    pub image_url: String,
    pub caption: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Action {
    pub label: String,
    pub description: String,
    pub identifier: String,
}

#[derive(Default, Deserialize)]
pub struct RunPatch {
    pub name: Option<String>,
    pub details_url: Option<String>,
    pub external_id: Option<String>,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub output: Option<Output>,
    pub actions: Option<Vec<Action>>,
}
#[derive(Deserialize)]
pub struct Output {
    pub title: Option<String>,
    pub summary: String,
    pub text: Option<String>,
    pub images: Option<Vec<Image>>,
    #[serde(default)]
    pub annotations: Vec<Annotation>,
}
#[derive(Default)]
pub struct Filter {
    pub suite_id: Option<i64>,
    pub head_sha: Option<String>,
    pub name: Option<String>,
    pub status: Option<String>,
    pub app_id: Option<i64>,
    pub all: bool,
}
#[derive(FromRow)]
pub struct Pull {
    pub id: i64,
    pub number: i64,
    pub head_branch: String,
    pub base_branch: String,
    pub head_sha: String,
    pub base_sha: String,
}

#[derive(Serialize, Deserialize, FromRow)]
pub struct Preference {
    pub app_id: i64,
    pub setting: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Check not found")]
    Missing,
    #[error("Checks write denied")]
    Denied,
    #[error("This credential cannot modify checks from another publisher")]
    Publisher,
    #[error("Invalid check")]
    Invalid,
}

fn text(value: &str, max: usize) -> bool {
    value.chars().count() <= max && !value.contains('\0')
}
fn url(value: &str) -> bool {
    text(value, 2048)
        && url::Url::parse(value).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
        })
}

impl RunPatch {
    pub fn validate(&self, creating: bool) -> Result<()> {
        ensure!(!creating || self.name.is_some(), Refusal::Invalid);
        if let Some(name) = &self.name {
            ensure!(!name.trim().is_empty() && text(name, 255), Refusal::Invalid);
        }
        ensure!(
            self.status
                .as_deref()
                .is_none_or(|s| matches!(s, "queued" | "in_progress" | "completed")),
            Refusal::Invalid
        );
        ensure!(
            self.conclusion.as_deref().is_none_or(|s| matches!(
                s,
                "action_required"
                    | "cancelled"
                    | "failure"
                    | "neutral"
                    | "success"
                    | "skipped"
                    | "timed_out"
            )),
            Refusal::Invalid
        );
        ensure!(
            !(self.status.as_deref() == Some("completed") || self.completed_at.is_some())
                || self.conclusion.is_some(),
            Refusal::Invalid
        );
        ensure!(
            self.details_url
                .as_deref()
                .is_none_or(|s| s.is_empty() || url(s)),
            Refusal::Invalid
        );
        ensure!(
            self.external_id.as_deref().is_none_or(|s| text(s, 255)),
            Refusal::Invalid
        );
        if let Some(output) = &self.output {
            ensure!(!creating || output.title.is_some(), Refusal::Invalid);
            ensure!(
                output.title.as_deref().is_none_or(|s| text(s, 255))
                    && text(&output.summary, 65535)
                    && output.text.as_deref().is_none_or(|s| text(s, 65535)),
                Refusal::Invalid
            );
            ensure!(output.annotations.len() <= 50, Refusal::Invalid);
            for a in &output.annotations {
                ensure!(
                    !a.path.is_empty()
                        && text(&a.path, 4096)
                        && !a.path.starts_with('/')
                        && !a.path.split('/').any(|part| matches!(part, "." | "..")),
                    Refusal::Invalid
                );
                ensure!(
                    a.start_line > 0 && a.end_line >= a.start_line,
                    Refusal::Invalid
                );
                ensure!(
                    matches!((&a.start_column, &a.end_column), (None, None))
                        || (a.start_line == a.end_line
                            && a.start_column.is_some_and(|n| n > 0)
                            && a.end_column
                                .is_some_and(|n| n >= a.start_column.unwrap_or(1))),
                    Refusal::Invalid
                );
                ensure!(
                    matches!(
                        a.annotation_level.as_str(),
                        "notice" | "warning" | "failure"
                    ) && text(&a.message, 65535)
                        && a.title.as_deref().is_none_or(|s| text(s, 255))
                        && a.raw_details.as_deref().is_none_or(|s| text(s, 65535)),
                    Refusal::Invalid
                );
            }
            if let Some(images) = &output.images {
                ensure!(
                    images.len() <= 50
                        && images.iter().all(|i| text(&i.alt, 255)
                            && url(&i.image_url)
                            && i.caption.as_deref().is_none_or(|s| text(s, 255))),
                    Refusal::Invalid
                );
            }
        }
        if let Some(actions) = &self.actions {
            ensure!(
                actions.len() <= 3
                    && actions.iter().all(|a| !a.label.is_empty()
                        && text(&a.label, 20)
                        && !a.description.is_empty()
                        && text(&a.description, 40)
                        && !a.identifier.is_empty()
                        && text(&a.identifier, 20)),
                Refusal::Invalid
            );
        }
        Ok(())
    }

    pub(super) fn apply(&self, run: &mut Run) -> Result<()> {
        self.validate(false)?;
        if let Some(value) = &self.name {
            run.name.clone_from(value);
        }
        if let Some(value) = &self.status {
            run.status.clone_from(value);
        }
        if let Some(value) = &self.conclusion {
            run.conclusion = Some(value.clone());
            run.status = "completed".into();
            run.data.completed_at = Some(self.completed_at.unwrap_or_else(Utc::now));
        } else if self.status.as_deref().is_some_and(|s| s != "completed") {
            run.conclusion = None;
            run.data.completed_at = None;
        }
        if let Some(value) = &self.details_url {
            run.data.details_url = Some(value.clone());
        }
        if let Some(value) = &self.external_id {
            run.data.external_id = Some(value.clone());
        }
        if let Some(value) = self.started_at {
            run.data.started_at = Some(value);
        }
        if let Some(output) = &self.output {
            if let Some(title) = &output.title {
                run.data.title = Some(title.clone());
            }
            run.data.summary = Some(output.summary.clone());
            if let Some(value) = &output.text {
                run.data.text = Some(value.clone());
            }
            if let Some(value) = &output.images {
                run.data.images.clone_from(value);
            }
        }
        if let Some(value) = &self.actions {
            run.data.actions.clone_from(value);
        }
        ensure!(
            !matches!((run.data.started_at, run.data.completed_at), (Some(start), Some(end)) if end < start),
            Refusal::Invalid
        );
        Ok(())
    }
}

pub(crate) fn validate_automatic(sha: &str, branch: &str, after: i64) -> Result<()> {
    ensure!(crate::backend::git::is_full_commit_id(sha) && !branch.is_empty() && text(branch, 1024) && after >= 0, Refusal::Invalid);
    Ok(())
}

native_operation!(crate::backend::db::postgres::checks::application, crate::backend::db::sqlite::checks::application,
    fn application(pool: &DbPool, id: i64) -> Result<Option<super::oauth_apps::Application>>);
native_operation!(crate::backend::db::postgres::checks::recent_app, crate::backend::db::sqlite::checks::recent_app,
    fn recent_app(pool: &DbPool, repo: i64, name: &str) -> Result<Option<i64>>);
native_operation!(crate::backend::db::postgres::checks::pulls, crate::backend::db::sqlite::checks::pulls,
    fn pulls(pool: &DbPool, repo: i64, sha: &str) -> Result<Vec<Pull>>);

native_operation!(crate::backend::db::postgres::checks::suite, crate::backend::db::sqlite::checks::suite,
    fn suite(pool: &DbPool, repo: i64, id: i64) -> Result<Option<Suite>>);
native_operation!(crate::backend::db::postgres::checks::run, crate::backend::db::sqlite::checks::run,
    fn run(pool: &DbPool, repo: i64, id: i64) -> Result<Option<Run>>);
native_operation!(crate::backend::db::postgres::checks::automatic_suites, crate::backend::db::sqlite::checks::automatic_suites,
    /// Newly requested suites for at most 100 active installed Apps. The cursor advances over
    /// eligible Apps even when their suite already exists; a repeated push never requests it twice.
    fn automatic_suites(pool: &DbPool, repo: &super::models::Repository, sha: &str, branch: &str, after: i64) -> Result<AutomaticSuites>);
native_operation!(crate::backend::db::postgres::checks::create_suite, crate::backend::db::sqlite::checks::create_suite,
    fn create_suite(pool: &DbPool, credential: Credential<'_>, repo: i64, sha: &str, branch: Option<&str>) -> Result<(Suite, bool)>);
native_operation!(crate::backend::db::postgres::checks::create_run, crate::backend::db::sqlite::checks::create_run,
    fn create_run(pool: &DbPool, credential: Credential<'_>, repo: i64, sha: &str, branch: Option<&str>, input: &RunPatch) -> Result<RunChange>);
native_operation!(crate::backend::db::postgres::checks::update_run, crate::backend::db::sqlite::checks::update_run,
    fn update_run(pool: &DbPool, credential: Credential<'_>, repo: i64, id: i64, input: &RunPatch) -> Result<RunChange>);
native_operation!(crate::backend::db::postgres::checks::runs, crate::backend::db::sqlite::checks::runs,
    fn runs(pool: &DbPool, repo: i64, filter: &Filter, offset: i64, limit: i64) -> Result<Vec<Run>>);
native_operation!(crate::backend::db::postgres::checks::run_count, crate::backend::db::sqlite::checks::run_count,
    fn run_count(pool: &DbPool, repo: i64, filter: &Filter) -> Result<i64>);
native_operation!(crate::backend::db::postgres::checks::suites, crate::backend::db::sqlite::checks::suites,
    fn suites(pool: &DbPool, repo: i64, filter: &Filter, offset: i64, limit: i64) -> Result<Vec<Suite>>);
native_operation!(crate::backend::db::postgres::checks::suite_count, crate::backend::db::sqlite::checks::suite_count,
    fn suite_count(pool: &DbPool, repo: i64, filter: &Filter) -> Result<i64>);
native_operation!(crate::backend::db::postgres::checks::annotations, crate::backend::db::sqlite::checks::annotations,
    fn annotations(pool: &DbPool, run: i64, offset: i64, limit: i64) -> Result<Vec<Json<Annotation>>>);
native_operation!(crate::backend::db::postgres::checks::annotation_count, crate::backend::db::sqlite::checks::annotation_count,
    fn annotation_count(pool: &DbPool, run: i64) -> Result<i64>);
native_operation!(crate::backend::db::postgres::checks::rerequest, crate::backend::db::sqlite::checks::rerequest,
    fn rerequest(pool: &DbPool, credential: Credential<'_>, repo: i64, suite: i64, run: Option<i64>) -> Result<()>);
native_operation!(crate::backend::db::postgres::checks::preferences, crate::backend::db::sqlite::checks::preferences,
    fn preferences(pool: &DbPool, credential: Credential<'_>, repo: i64, preferences: &[Preference]) -> Result<Vec<Preference>>);
