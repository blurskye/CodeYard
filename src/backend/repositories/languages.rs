//! A repository's stored languages: the language mix of its default branch. Git computes the
//! mix. The stored rows are what the repository page and the Explore filter read. They are
//! written when the default branch moves, and computed for the first reader when none exist.
use crate::backend::{
    db::{self, DbPool},
    git::{self, LanguageStat},
    repositories::store::{RepoError, RepoStore},
};
use anyhow::Result;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// The longest a background refresh may run. A refresh that times out is logged, and the next
/// change to the default branch stores the languages again.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(60);

/// The stored languages of a repository, most used first. Empty until they are computed.
pub async fn stored(pool: &DbPool, repo_id: i64) -> Result<Vec<LanguageStat>> {
    Ok(db::repository_stats::get_repo_languages(pool, repo_id)
        .await?
        .into_iter()
        .map(|row| LanguageStat {
            language: row.language,
            bytes: row.bytes as u64,
            percentage: row.percentage as f32,
            color: row.color.unwrap_or_default(),
        })
        .collect())
}

/// Computes the languages of `branch` in Git, which must be the default branch because only that
/// branch's languages are stored. A node that holds the repository also stores them for the next
/// reader. A failure to store them is logged, and the languages are still returned.
pub async fn compute_and_store(
    pool: &DbPool,
    store: &RepoStore,
    repository: &db::models::Repository,
    branch: &str,
) -> Result<Vec<LanguageStat>, RepoError> {
    if let Some(path) = store.local_dir() {
        return tokio::time::timeout(
            REFRESH_TIMEOUT,
            compute_local(
                pool,
                repository.id,
                path,
                branch,
                &repository.default_branch,
            ),
        )
        .await
        .map_err(|_| {
            RepoError::new(
                crate::backend::repositories::store::ErrorKind::Unavailable,
                "Language computation timed out",
            )
        })?
        .map_err(RepoError::from);
    }
    store.languages(&format!("refs/heads/{branch}")).await
}

/// Stores the languages of `branch` after Git moved it, when it is still the default branch.
/// `path` is the copy this node holds. The work runs in the background, so the push, edit or
/// merge that moved the branch does not wait for it, and a failure never fails that write.
pub(crate) fn refresh_default_branch(pool: DbPool, repo_id: i64, path: PathBuf, branch: String) {
    tokio::spawn(async move {
        match tokio::time::timeout(REFRESH_TIMEOUT, refresh(&pool, repo_id, &path, &branch)).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::warn!(repo_id, %error, "could not store repository languages")
            }
            Err(_) => tracing::warn!(repo_id, "storing repository languages timed out"),
        }
    });
}

async fn refresh(
    pool: &DbPool,
    repo_id: i64,
    path: &Path,
    branch: &str,
) -> Result<Vec<LanguageStat>> {
    let Some(repository) = db::repositories::records::get_repository_by_id(pool, repo_id).await?
    else {
        return Ok(Vec::new());
    };
    if repository.default_branch != branch {
        return Ok(Vec::new());
    }
    compute_local(pool, repo_id, path, branch, &repository.default_branch).await
}

async fn compute_local(
    pool: &DbPool,
    repo_id: i64,
    path: &Path,
    branch: &str,
    expected_branch: &str,
) -> Result<Vec<LanguageStat>> {
    let mut branch = branch.to_owned();
    loop {
        // A push can move the branch while these languages are computed and stored. That push starts
        // its own refresh, but this one may store after it, so store again until the branch has not
        // moved during a pass.
        let tip = git::branch_tip(path, &branch)?;
        let revision = tip
            .clone()
            .unwrap_or_else(|| format!("refs/heads/{branch}"));
        let languages = git::compute_language_stats(path, &revision).await?;
        if let Err(error) = store_rows(pool, repo_id, expected_branch, &languages).await {
            tracing::warn!(repo_id, %error, "could not store repository languages");
            return Ok(languages);
        }
        let current = git::get_repo_info(path).await?.default_branch;
        if current == branch && git::branch_tip(path, &branch)? == tip {
            return Ok(languages);
        }
        branch = current;
    }
}

/// Replaces the stored languages of `repo_id` with `languages`.
async fn store_rows(
    pool: &DbPool,
    repo_id: i64,
    branch: &str,
    languages: &[LanguageStat],
) -> Result<()> {
    let rows: Vec<(String, i64, f32, String)> = languages
        .iter()
        .map(|language| {
            (
                language.language.clone(),
                language.bytes as i64,
                language.percentage,
                language.color.clone(),
            )
        })
        .collect();
    db::repository_stats::upsert_repo_languages(pool, repo_id, branch, &rows).await
}
