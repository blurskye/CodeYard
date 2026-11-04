use anyhow::Result;
use codeyard_db_core::models::RepoLanguage;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

native_operation!(crate::postgres::repository_stats::readable_snapshots,
    crate::sqlite::repository_stats::readable_snapshots,
    fn readable_snapshots(pool: &DbPool, viewer: Option<i64>, ids: &[i64]) -> Result<Vec<RepositorySnapshot>>);

native_operation!(crate::postgres::repository_stats::repository_document_facts,
    crate::sqlite::repository_stats::repository_document_facts,
    fn repository_document_facts(pool: &DbPool, repo: i64) -> Result<RepositoryDocumentFacts>);

#[derive(sqlx::FromRow)]
pub struct RepositoryDocumentMetadata {
    pub repo_id: i64,
    #[sqlx(flatten)]
    pub facts: RepositoryDocumentFacts,
    pub last_pushed_at: chrono::DateTime<chrono::Utc>,
    #[sqlx(flatten)]
    pub features: crate::repositories::records::RepositoryFeatures,
    pub discussions: bool,
}

native_operation!(crate::postgres::repository_stats::repository_document_metadata,
    crate::sqlite::repository_stats::repository_document_metadata,
    fn repository_document_metadata(pool: &DbPool, repos: &[i64]) -> Result<Vec<RepositoryDocumentMetadata>>);

#[derive(Debug, sqlx::FromRow)]
pub struct LanguageSource {
    pub repo_id: i64,
    pub default_branch: String,
    pub storage_node_id: Option<i64>,
    pub write_version: i64,
}

native_operation!(crate::postgres::repository_stats::language_source,
    crate::sqlite::repository_stats::language_source,
    fn language_source(pool: &DbPool, repo_id: i64) -> Result<Option<LanguageSource>>);

native_operation!(crate::postgres::repository_stats::forget_languages,
    crate::sqlite::repository_stats::forget_languages,
    /// Drops a repository's language cache marker, so its next scan reads Git again.
    fn forget_languages(pool: &DbPool, repo_id: i64) -> Result<()>);

native_operation!(crate::postgres::repository_stats::languages_computed,
    crate::sqlite::repository_stats::languages_computed,
    fn languages_computed(pool: &DbPool, source: &LanguageSource, revision: &str) -> Result<bool>);

native_operation!(crate::postgres::repository_stats::upsert_repo_languages,
    crate::sqlite::repository_stats::upsert_repo_languages,
    fn upsert_repo_languages(
    pool: &DbPool,
    source: &LanguageSource,
    revision: &str,
    languages: &[(String, i64, f32, String)],
) -> Result<bool>);

native_operation!(crate::postgres::repository_stats::language_revision,
    crate::sqlite::repository_stats::language_revision,
    fn language_revision(pool: &DbPool, repo_id: i64, branch: &str) -> Result<Option<String>>);

native_operation!(crate::postgres::repository_stats::get_repo_languages,
    crate::sqlite::repository_stats::get_repo_languages,
    fn get_repo_languages(pool: &DbPool, repo_id: i64) -> Result<Vec<RepoLanguage>>);

native_operation!(crate::postgres::repository_stats::get_primary_repo_languages,
    crate::sqlite::repository_stats::get_primary_repo_languages,
    fn get_primary_repo_languages(pool: &DbPool, repo_ids: &[i64]) -> Result<Vec<RepoLanguage>>);

/// What happened in one repository on one day.
#[derive(Debug, sqlx::FromRow)]
pub struct RepoActivityPoint {
    pub day: chrono::NaiveDate,
    /// Commits pushed, through Git or written in the web editor.
    pub commits: i64,
    pub pulls_opened: i64,
    pub pulls_merged: i64,
    pub issues_opened: i64,
    pub issues_closed: i64,
    /// Workflow runs started.
    pub runs: i64,
}

/// One workflow's runs in a timeframe.
#[derive(Debug, sqlx::FromRow)]
pub struct WorkflowUsage {
    pub workflow_path: String,
    pub runs: i64,
    pub succeeded: i64,
    pub failed: i64,
    /// The mean time from start to finish of the runs that finished.
    pub average_seconds: Option<f64>,
}

native_operation!(crate::postgres::repository_stats::list_repo_activity_points,
    crate::sqlite::repository_stats::list_repo_activity_points,
    fn list_repo_activity_points(pool: &DbPool, repo_id: i64, days: i32) -> Result<Vec<RepoActivityPoint>>);

native_operation!(crate::postgres::repository_stats::list_workflow_usage,
    crate::sqlite::repository_stats::list_workflow_usage,
    fn list_workflow_usage(pool: &DbPool, repo_id: i64, days: i32) -> Result<Vec<WorkflowUsage>>);

use codeyard_db_core::queries::repository_stats::RepositoryDocumentFacts;

use codeyard_db_core::queries::repository_stats::RepositorySnapshot;

use codeyard_db_core::queries::repository_stats::RepositoryDocumentFacts;

use codeyard_db_core::queries::repository_stats::RepositorySnapshot;
