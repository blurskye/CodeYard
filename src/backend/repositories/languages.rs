//! Stored default-branch languages, including successfully computed empty results.
use crate::backend::{
    db::{self, repository_stats::LanguageSource, DbPool},
    git::{self, LanguageStat},
    repositories::store::{ErrorKind, RepoError, RepoStore},
};
use anyhow::Result;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::Duration,
};

const REFRESH_TIMEOUT: Duration = Duration::from_secs(60);
type RefreshRequest = (HashSet<String>, Option<Option<i64>>);
static REFRESHES: LazyLock<Mutex<HashMap<PathBuf, Option<RefreshRequest>>>> =
    LazyLock::new(Default::default);

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

/// Remote computations use an immutable primary revision and publish under the same database
/// placement/version check as local computations. A replica cannot publish an older tree.
pub async fn compute_and_store(
    pool: &DbPool,
    store: &RepoStore,
    repository: &db::models::Repository,
    branch: &str,
) -> Result<Vec<LanguageStat>, RepoError> {
    tokio::time::timeout(REFRESH_TIMEOUT, async {
        let Some(source) = db::repository_stats::language_source(pool, repository.id).await? else {
            return Ok(Vec::new());
        };
        if source.default_branch != repository.default_branch || source.storage_node_id != repository.storage_node_id {
            return Ok(Vec::new());
        }
        if let Some(path) = store.local_dir() {
            let mut source = source;
            let mut branch = branch.to_owned();
            loop {
                let tip = git::branch_tip(path, &branch)?;
                let languages = compute_local(pool, path, &branch, &source).await?;
                let current = git::default_branch(path).await?.unwrap_or_else(|| "main".into());
                if current == branch && git::branch_tip(path, &branch)? == tip {
                    return Ok(languages);
                }
                let Some(current_source) = db::repository_stats::language_source(pool, repository.id).await? else {
                    return Ok(Vec::new());
                };
                if current_source.storage_node_id != repository.storage_node_id {
                    return Ok(Vec::new());
                }
                source = current_source;
                branch = current;
            }
        }
        let reference = format!("refs/heads/{branch}");
        let revision = store.refs_from_primary().await?
            .remove(&reference).unwrap_or_default();
        if db::repository_stats::languages_computed(pool, &source, &revision).await? {
            return stored(pool, repository.id).await;
        }
        let languages = if revision.is_empty() { Vec::new() } else {
            store.languages(&revision).await?
        };
        if let Err(error) = store_rows(pool, &source, &revision, &languages).await {
            tracing::warn!(repo_id = source.repo_id, %error, "could not store repository languages");
        }
        Ok::<_, anyhow::Error>(languages)
    }).await.map_err(|_| RepoError::new(ErrorKind::Unavailable, "Language computation timed out"))?
        .map_err(RepoError::from)
}

/// Keep only the latest pending request while a repository's worker is scanning.
pub(crate) fn refresh_default_branch(
    pool: DbPool,
    repo_id: i64,
    path: PathBuf,
    branch: String,
    expected_node: Option<Option<i64>>,
) {
    {
        let mut refreshes = REFRESHES.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(pending) = refreshes.get_mut(&path) {
            let (branches, node) = pending.get_or_insert_with(|| (HashSet::new(), expected_node));
            branches.insert(branch);
            *node = expected_node;
            return;
        }
        refreshes.insert(path.clone(), Some((HashSet::from([branch]), expected_node)));
    }
    tokio::spawn(async move {
        loop {
            let request = {
                let mut refreshes = REFRESHES.lock().unwrap_or_else(|error| error.into_inner());
                let request = refreshes.get_mut(&path).and_then(Option::take);
                if request.is_none() {
                    refreshes.remove(&path);
                }
                request
            };
            let Some((branches, expected_node)) = request else {
                return;
            };
            let work = async {
                let Some(source) = db::repository_stats::language_source(&pool, repo_id).await?
                else {
                    return Ok(());
                };
                if branches.contains(&source.default_branch)
                    && !expected_node.is_some_and(|node| node != source.storage_node_id)
                {
                    compute_local(&pool, &path, &source.default_branch, &source).await?;
                }
                Ok::<_, anyhow::Error>(())
            };
            match tokio::time::timeout(REFRESH_TIMEOUT, work).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(repo_id, %error, "could not store repository languages")
                }
                Err(_) => tracing::warn!(repo_id, "storing repository languages timed out"),
            }
        }
    });
}

async fn compute_local(
    pool: &DbPool,
    path: &Path,
    branch: &str,
    source: &LanguageSource,
) -> Result<Vec<LanguageStat>> {
    let tip = git::branch_tip(path, branch)?.unwrap_or_default();
    if db::repository_stats::languages_computed(pool, source, &tip).await? {
        return stored(pool, source.repo_id).await;
    }
    let languages = if tip.is_empty() {
        Vec::new()
    } else {
        git::compute_language_stats(path, &tip).await?
    };
    // Do not publish a scan if this copy moved while it was running. Its push queued a refresh.
    if git::branch_tip(path, branch)?.unwrap_or_default() == tip {
        if let Err(error) = store_rows(pool, source, &tip, &languages).await {
            tracing::warn!(repo_id = source.repo_id, %error, "could not store repository languages");
        }
    }
    Ok(languages)
}

async fn store_rows(
    pool: &DbPool,
    source: &LanguageSource,
    revision: &str,
    languages: &[LanguageStat],
) -> Result<()> {
    let rows: Vec<_> = languages
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
    db::repository_stats::upsert_repo_languages(pool, source, revision, &rows).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refresh_requests_coalesce_per_repository() {
        let raw = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_lazy("sqlite::memory:")
            .unwrap();
        let pool = DbPool::Sqlite(raw);
        let path = PathBuf::from(format!("/unused/{}.git", uuid::Uuid::new_v4()));
        for _ in 0..100 {
            refresh_default_branch(pool.clone(), 1, path.clone(), "main".into(), Some(None));
            refresh_default_branch(pool.clone(), 1, path.clone(), "feature".into(), Some(None));
        }
        {
            let refreshes = REFRESHES.lock().unwrap();
            let (branches, _) = refreshes[&path].as_ref().unwrap();
            assert_eq!(
                branches,
                &HashSet::from(["main".to_owned(), "feature".to_owned()])
            );
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if !REFRESHES.lock().unwrap().contains_key(&path) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        pool.close().await;
    }
}
