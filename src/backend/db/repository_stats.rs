use crate::backend::db::models::{RepoContributor, RepoLanguage};
use crate::backend::db::DbPool;
use anyhow::Result;

native_operation!(crate::backend::db::postgres::repository_stats::upsert_repo_languages,
    crate::backend::db::sqlite::repository_stats::upsert_repo_languages,
    fn upsert_repo_languages(
    pool: &DbPool,
    repo_id: i64,
    languages: &[(String, i64, f32, String)],
) -> Result<()>);

native_operation!(crate::backend::db::postgres::repository_stats::get_repo_languages,
    crate::backend::db::sqlite::repository_stats::get_repo_languages,
    fn get_repo_languages(pool: &DbPool, repo_id: i64) -> Result<Vec<RepoLanguage>>);

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
