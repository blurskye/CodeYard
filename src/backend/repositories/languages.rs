//! Stored default-branch languages, including successfully computed empty results.
use crate::backend::db;
use crate::backend::db::repository_stats::LanguageSource;
use crate::backend::git;
use crate::backend::git::LanguageStat;
use crate::backend::repositories::store::ErrorKind;
use crate::backend::repositories::store::RepoError;
use crate::backend::repositories::store::RepoStore;
use anyhow::Result;
use codeyard_db_core::DbPool;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
    time::Duration,
};

const REFRESH_TIMEOUT: Duration = Duration::from_secs(60);
type RefreshRequest = (HashSet<String>, Option<Option<i64>>);
static REFRESHES: LazyLock<Mutex<HashMap<PathBuf, Option<RefreshRequest>>>> =
    LazyLock::new(Default::default);
static COMPUTATIONS: LazyLock<Mutex<HashMap<i64, Weak<tokio::sync::Mutex<()>>>>> =
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

fn computation(repo_id: i64) -> Arc<tokio::sync::Mutex<()>> {
    let mut computations = COMPUTATIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    computations.retain(|_, lock| lock.strong_count() != 0);
    let lock = computations
        .get(&repo_id)
        .and_then(Weak::upgrade)
        .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
    computations.insert(repo_id, Arc::downgrade(&lock));
    lock
}

/// The owning primary computes immutable revisions and publishes under its repository
/// lease and the database placement/version check. Replicas never publish a cache.
pub async fn compute_and_store(
    pool: &DbPool,
    store: &RepoStore,
    repository: &codeyard_db_core::models::Repository,
    branch: &str,
) -> Result<Vec<LanguageStat>, RepoError> {
    let Some(path) = store.local_dir() else {
        // Do not hold a frontend single-flight lock across RPC: an owning store can
        // run in this process too, and must acquire the same lock itself.
        return tokio::time::timeout(REFRESH_TIMEOUT, store.languages_from_primary(branch))
            .await
            .map_err(|_| {
                RepoError::new(ErrorKind::Unavailable, "Language computation timed out")
            })?;
    };
    let computation = computation(repository.id);
    tokio::time::timeout(REFRESH_TIMEOUT, async {
        let _computation = computation.lock().await;
        let Some(source) = db::repository_stats::language_source(pool, repository.id).await? else {
            return Ok(Vec::new());
        };
        if source.default_branch != repository.default_branch
            || source.storage_node_id != repository.storage_node_id
        {
            return Ok(Vec::new());
        }
        let mut source = source;
        let mut branch = branch.to_owned();
        loop {
            let tip = git::branch_tip(path, &branch)?;
            let languages = compute_local(pool, path, &branch, &source).await?;
            let current = git::default_branch(path)
                .await?
                .unwrap_or_else(|| "main".into());
            if current == branch
                && git::branch_tip(path, &branch)? == tip
                && db::repository_stats::languages_computed(
                    pool,
                    &source,
                    &tip.clone().unwrap_or_default(),
                )
                .await?
            {
                return Ok::<_, anyhow::Error>(languages);
            }
            let Some(current_source) =
                db::repository_stats::language_source(pool, repository.id).await?
            else {
                return Ok(Vec::new());
            };
            if current_source.storage_node_id != repository.storage_node_id {
                return Ok(Vec::new());
            }
            source = current_source;
            branch = current;
        }
    })
    .await
    .map_err(|_| RepoError::new(ErrorKind::Unavailable, "Language computation timed out"))?
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
                let computation = computation(repo_id);
                let _computation = computation.lock().await;
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
    // A feature-branch push must not discard a completed scan of this unchanged tip.
    // Only publication is serialized: refresh the source/version under the same lease
    // as ref publishers, then retain the database placement CAS through commit.
    let _publication = git::locks::publication(path).await?;
    let Some(current) = db::repository_stats::language_source(pool, source.repo_id).await? else {
        return Ok(languages);
    };
    // Languages follow Git's default branch, which an adopted repository may name
    // differently from the stored default.
    let git_default = git::default_branch(path)
        .await?
        .unwrap_or_else(|| "main".into());
    if git_default == branch
        && current.storage_node_id == source.storage_node_id
        && git::branch_tip(path, branch)?.unwrap_or_default() == tip
    {
        match store_rows(pool, &current, &tip, &languages).await? {
            true => {}
            // Placement/version may still change independently of this process's lease.
            false => refresh_default_branch(
                pool.clone(),
                source.repo_id,
                path.to_owned(),
                source.default_branch.clone(),
                // The retry keeps this copy's placement, so a promotion meanwhile refuses it.
                Some(source.storage_node_id),
            ),
        }
    }
    Ok(languages)
}

async fn store_rows(
    pool: &DbPool,
    source: &LanguageSource,
    revision: &str,
    languages: &[LanguageStat],
) -> Result<bool> {
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
    // False when the version check lost to a newer write, and nothing was stored.
    db::repository_stats::upsert_repo_languages(pool, source, revision, &rows).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refresh_requests_coalesce_per_repository() {
        let raw = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_lazy("sqlite::memory:")
            .unwrap();
        let pool = codeyard_db_core::DbPool::Sqlite(raw);
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
