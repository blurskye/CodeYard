//! Open pull requests follow their branches. Fork heads are retained in the base
//! repository so deleting a fork does not remove the commits being reviewed.

use super::workflows::queue_pull_request_ci;
use crate::backend::ci::CiRuns;
use crate::backend::db;
use crate::backend::db::models::Repository;
use crate::backend::db::DbPool;
use crate::backend::repositories::store::{repo_store, ErrorKind, RepoStore};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum HeadError {
    #[error("Head repository not found")]
    Missing,
    #[error("Head is outside this fork network")]
    OutsideFork,
    #[error("Head repository lookup is unavailable")]
    Unavailable,
}

#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error("Pull request records are unavailable")]
    Database,
    #[error("Pull request repository storage is unavailable")]
    Storage,
    #[error("Base repository not found")]
    MissingBase,
    #[error("Pull request revisions kept changing during refresh")]
    Conflict,
}

/// Only this repository and its direct forks are valid heads.
pub async fn head_repository(
    pool: &DbPool,
    base: &Repository,
    head_id: i64,
) -> Result<Repository, HeadError> {
    let head = db::repositories::records::get_repository_by_id(pool, head_id)
        .await
        .map_err(|_| HeadError::Unavailable)?
        .ok_or(HeadError::Missing)?;
    if head.id != base.id && head.forked_from_id != Some(base.id) {
        return Err(HeadError::OutsideFork);
    }
    Ok(head)
}

const MAX_REFRESH_ATTEMPTS: usize = 3;

/// Most branch names a refresh filters pull requests on. A bigger change refreshes every
/// open pull request of the repository instead, which keeps the database query bounded.
const MAX_FILTERED_BRANCHES: usize = 32;

/// The refs each repository listed during one refresh call, by repository id. The pull
/// requests of the call share these listings.
type ListedRefs = HashMap<i64, HashMap<String, String>>;

/// The commit that a branch of `repo_id` points at, or `None` when the branch is missing.
/// The first attempt of a pull request reuses the listing its call already made. A retry
/// lists the refs again, since the conflict may come from a branch that moved after that
/// listing.
async fn branch_commit(
    listed: &mut ListedRefs,
    store: &RepoStore,
    repo_id: i64,
    attempt: usize,
    branch: &str,
) -> Result<Option<String>, RefreshError> {
    if attempt > 0 || !listed.contains_key(&repo_id) {
        let refs = store
            .refs_from_primary()
            .await
            .map_err(|_| RefreshError::Storage)?;
        listed.insert(repo_id, refs);
    }
    let name = format!("refs/heads/{branch}");
    Ok(listed[&repo_id].get(&name).cloned())
}

fn revisions(
    pull: db::models::PullRequest,
) -> Option<db::pull_requests::records::PullRequestRevisions> {
    (pull.state == "open").then_some(db::pull_requests::records::PullRequestRevisions {
        id: pull.id,
        repo_id: pull.repo_id,
        number: pull.number,
        author_id: pull.author_id,
        head_repo_id: pull.head_repo_id,
        head_branch: pull.head_branch,
        head_sha: pull.head_sha,
        base_branch: pull.base_branch,
        base_sha: pull.base_sha,
    })
}

/// Moves the recorded head of an open pull request from `from` to `to`, unless another
/// refresh moved it first. The head it records gets CI, as every head move does.
async fn move_head(
    pool: &DbPool,
    ci: &CiRuns,
    base: &Repository,
    pull_id: i64,
    from: &str,
    to: &str,
    actor_id: i64,
) -> Result<bool, RefreshError> {
    let moved = db::pull_requests::records::move_open_pull_request_head(pool, pull_id, from, to)
        .await
        .map_err(|_| RefreshError::Database)?;
    if !moved {
        return Ok(false);
    }
    let current = db::pull_requests::records::get_pull_request_by_id(pool, pull_id)
        .await
        .map_err(|_| RefreshError::Database)?;
    if let Some(current) = current {
        let _ = queue_pull_request_ci(pool, ci, base, &current, actor_id).await;
        super::code_owners::request_code_owner_reviews(pool, ci, base, &current, actor_id).await;
    }
    Ok(true)
}

/// The pull ref can name a head that the row does not record, when a refresh published
/// that head and then lost its row update. Publication compares against the recorded
/// head, so record the published head first, or every later attempt conflicts.
async fn record_published_head(
    pool: &DbPool,
    ci: &CiRuns,
    base: &Repository,
    base_store: &RepoStore,
    pull: &db::pull_requests::records::PullRequestRevisions,
    actor_id: i64,
) -> Result<bool, RefreshError> {
    let reference = format!("refs/pull/{}/head", pull.number);
    let published = match base_store.commit_from_primary(&reference).await {
        Ok(commit) => commit.sha,
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(RefreshError::Storage),
    };
    if published == pull.head_sha {
        return Ok(false);
    }
    move_head(
        pool,
        ci,
        base,
        pull.id,
        &pull.head_sha,
        &published,
        actor_id,
    )
    .await
}

async fn refresh_one(
    pool: &DbPool,
    moved_repo: &Repository,
    ci: &CiRuns,
    actor_id: i64,
    listed: &mut ListedRefs,
    mut pull: db::pull_requests::records::PullRequestRevisions,
) -> Result<bool, RefreshError> {
    let context = ci.cluster.as_ref();
    let mut changed = false;
    for attempt in 0..MAX_REFRESH_ATTEMPTS {
        if attempt > 0 {
            let Some(current) = db::pull_requests::records::get_pull_request_by_id(pool, pull.id)
                .await
                .map_err(|_| RefreshError::Database)?
                .and_then(revisions)
            else {
                return Ok(changed);
            };
            pull = current;
        }

        let base = if pull.repo_id == moved_repo.id {
            moved_repo.clone()
        } else {
            db::repositories::records::get_repository_by_id(pool, pull.repo_id)
                .await
                .map_err(|_| RefreshError::Database)?
                .ok_or(RefreshError::MissingBase)?
        };
        let base_store = repo_store(&base, context);
        let new_base = branch_commit(listed, &base_store, base.id, attempt, &pull.base_branch)
            .await?
            .unwrap_or_else(|| pull.base_sha.clone());

        let mut retry = false;
        if new_base != pull.base_sha {
            // The refresh read `pull.base_branch`; a re-base since then must make it stale.
            if db::pull_requests::records::move_open_pull_request_base(
                pool,
                pull.id,
                &pull.base_sha,
                &new_base,
                &pull.base_branch,
            )
            .await
            .map_err(|_| RefreshError::Database)?
            {
                changed = true;
                // The base may have changed CODEOWNERS or the files the pull request differs in.
                let current = db::pull_requests::records::get_pull_request_by_id(pool, pull.id)
                    .await
                    .map_err(|_| RefreshError::Database)?;
                if let Some(current) = current {
                    super::code_owners::request_code_owner_reviews(
                        pool, ci, &base, &current, actor_id,
                    )
                    .await;
                }
            } else {
                retry = true;
            }
        }

        let head = match pull.head_repo_id {
            Some(id) if id == base.id => Some(base.clone()),
            Some(id) if id == moved_repo.id => Some(moved_repo.clone()),
            Some(id) => db::repositories::records::get_repository_by_id(pool, id)
                .await
                .map_err(|_| RefreshError::Database)?,
            None => None,
        };
        let new_head = if let Some(head) = &head {
            let head_store = repo_store(head, context);
            branch_commit(listed, &head_store, head.id, attempt, &pull.head_branch)
                .await?
                .unwrap_or_else(|| pull.head_sha.clone())
        } else {
            pull.head_sha.clone()
        };

        let mut head_ready = true;
        let mut head_failed = false;
        if pull.head_repo_id != Some(base.id) {
            if let Some(head) = &head {
                // The pull request may have been closed while the refs were read. A closed
                // one takes no new commits from its fork.
                let open = db::pull_requests::records::get_pull_request_by_id(pool, pull.id)
                    .await
                    .map_err(|_| RefreshError::Database)?
                    .is_some_and(|row| row.state == "open");
                if !open {
                    return Ok(changed);
                }
                if let Err(error) = base_store
                    .fetch_pull_head(
                        pool,
                        base.id,
                        head.id,
                        pull.author_id,
                        &repo_store(head, context),
                        pull.number,
                        &new_head,
                        Some(&pull.head_sha),
                    )
                    .await
                {
                    head_ready = false;
                    if error.kind == ErrorKind::Conflict {
                        if record_published_head(pool, ci, &base, &base_store, &pull, actor_id)
                            .await?
                        {
                            changed = true;
                        }
                        retry = true;
                    } else {
                        head_failed = true;
                    }
                }
            }
        }

        if head_ready && new_head != pull.head_sha {
            if move_head(
                pool,
                ci,
                &base,
                pull.id,
                &pull.head_sha,
                &new_head,
                actor_id,
            )
            .await?
            {
                changed = true;
            } else {
                retry = true;
            }
        }

        if head_failed {
            return Err(RefreshError::Storage);
        }
        if !retry {
            return Ok(changed);
        }
        if attempt + 1 < MAX_REFRESH_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(10 * (attempt as u64 + 1))).await;
        }
    }
    Err(RefreshError::Conflict)
}

/// Refreshes one pull request against its base repository. Operations on one PR
/// use this path so an unrelated broken source cannot deny them.
pub async fn refresh_open_pull_request(
    pool: &DbPool,
    repository: &Repository,
    ci: &CiRuns,
    actor_id: i64,
    pull_request_id: i64,
) -> Result<bool, RefreshError> {
    let Some(pull) = db::pull_requests::records::get_pull_request_by_id(pool, pull_request_id)
        .await
        .map_err(|_| RefreshError::Database)?
        .and_then(revisions)
    else {
        return Ok(false);
    };
    if pull.repo_id != repository.id {
        return Err(RefreshError::MissingBase);
    }
    let mut listed = ListedRefs::new();
    refresh_one(pool, repository, ci, actor_id, &mut listed, pull).await
}

/// Refreshes the open pull requests that use one of `branches` in this repository, as
/// base or as head, after those branches moved. `None` refreshes every open pull request
/// of the repository, for callers that do not know which branches moved. One pull
/// request's source/storage failure is reported after later pull requests have had their
/// independent head and base updates attempted.
pub async fn refresh_open_pull_requests(
    pool: &DbPool,
    repo: &Repository,
    branches: Option<&[&str]>,
    ci: &CiRuns,
    actor_id: i64,
) -> Result<usize, RefreshError> {
    let open = match branches.filter(|names| names.len() <= MAX_FILTERED_BRANCHES) {
        Some(names) => {
            db::pull_requests::records::list_open_pull_request_revisions_on_branches(
                pool, repo.id, names,
            )
            .await
        }
        None => db::pull_requests::records::list_open_pull_request_revisions(pool, repo.id).await,
    }
    .map_err(|_| RefreshError::Database)?;
    let mut listed = ListedRefs::new();
    let mut changed = 0;
    let mut first_error = None;
    for pull in open {
        let pull_id = pull.id;
        match refresh_one(pool, repo, ci, actor_id, &mut listed, pull).await {
            Ok(true) => changed += 1,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(pull_request_id = pull_id, %error, "could not refresh pull request revisions");
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(changed),
    }
}

/// A branch already moved, so refresh failures are logged and retried by the next
/// branch move, reopen, ready or merge.
pub async fn branches_moved(pool: &DbPool, repo: &Repository, ci: &CiRuns, actor_id: i64) {
    if let Err(error) = refresh_open_pull_requests(pool, repo, None, ci, actor_id).await {
        tracing::warn!(repo_id = repo.id, %error, "could not refresh open pull requests after a branch moved");
    }
}

#[cfg(test)]
mod tests;
