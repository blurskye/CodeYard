use crate::backend::db::models::{RepoContributor, RepoLanguage};
use crate::backend::db::DbPool;
use anyhow::Result;

#[derive(Debug, sqlx::FromRow)]
pub struct LanguageSource {
    pub repo_id: i64,
    pub default_branch: String,
    pub storage_node_id: Option<i64>,
    pub write_version: i64,
}

native_operation!(crate::backend::db::postgres::repository_stats::language_source,
    crate::backend::db::sqlite::repository_stats::language_source,
    fn language_source(pool: &DbPool, repo_id: i64) -> Result<Option<LanguageSource>>);

native_operation!(crate::backend::db::postgres::repository_stats::languages_computed,
    crate::backend::db::sqlite::repository_stats::languages_computed,
    fn languages_computed(pool: &DbPool, source: &LanguageSource, revision: &str) -> Result<bool>);

native_operation!(crate::backend::db::postgres::repository_stats::upsert_repo_languages,
    crate::backend::db::sqlite::repository_stats::upsert_repo_languages,
    fn upsert_repo_languages(
    pool: &DbPool,
    source: &LanguageSource,
    revision: &str,
    languages: &[(String, i64, f32, String)],
) -> Result<bool>);

native_operation!(crate::backend::db::postgres::repository_stats::language_revision,
    crate::backend::db::sqlite::repository_stats::language_revision,
    fn language_revision(pool: &DbPool, repo_id: i64, branch: &str) -> Result<Option<String>>);

native_operation!(crate::backend::db::postgres::repository_stats::get_repo_languages,
    crate::backend::db::sqlite::repository_stats::get_repo_languages,
    fn get_repo_languages(pool: &DbPool, repo_id: i64) -> Result<Vec<RepoLanguage>>);

native_operation!(crate::backend::db::postgres::repository_stats::get_primary_repo_languages,
    crate::backend::db::sqlite::repository_stats::get_primary_repo_languages,
    fn get_primary_repo_languages(pool: &DbPool, repo_ids: &[i64]) -> Result<Vec<RepoLanguage>>);

native_operation!(crate::backend::db::postgres::repository_stats::upsert_repo_contributors,
    crate::backend::db::sqlite::repository_stats::upsert_repo_contributors,
    fn upsert_repo_contributors(
    pool: &DbPool,
    repo_id: i64,
    contributors: &[(String, String, i32, Option<i64>)],
) -> Result<()>);

native_operation!(crate::backend::db::postgres::repository_stats::get_repo_contributors,
    crate::backend::db::sqlite::repository_stats::get_repo_contributors,
    fn get_repo_contributors(
        pool: &DbPool,
        repo_id: i64,
        limit: i64,
    ) -> Result<Vec<RepoContributor>>);
