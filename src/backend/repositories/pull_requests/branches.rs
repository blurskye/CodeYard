//! Open pull requests follow their branches. Fork heads are retained in the base
//! repository so deleting a fork does not remove the commits being reviewed.

use crate::backend::ci::CiRuns;
use crate::backend::db;
use crate::backend::db::models::Repository;
use crate::backend::db::DbPool;
use crate::backend::repositories::store::{repo_store, ErrorKind, RepoError, RepoStore};
use crate::backend::webhooks::{self, events, payloads::Scope};
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

/// A namespaced branch can live in a renamed fork. An unrelated same-named repository
/// must not shadow that fork.
pub async fn owner_head_repository(pool: &DbPool, base: &Repository, owner: &str) -> anyhow::Result<Option<Repository>> {
    if let Some(head) = db::repositories::records::get_repository(pool, owner, &base.name).await? {
        if head.id == base.id || head.forked_from_id == Some(base.id) {
            return Ok(Some(head));
        }
    }
    use crate::backend::repositories::lifecycle;
    match lifecycle::transfer_target(pool, owner).await {
        Ok((owner, _)) => db::repositories::discovery::get_owner_fork(pool, owner, base.id).await,
        Err(error) if error.kind == lifecycle::LifecycleFailure::Invalid => Ok(None),
        Err(error) => Err(error.into()),
    }
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
        head_publisher_id: pull.head_publisher_id,
        head_repo_id: pull.head_repo_id,
        head_branch: pull.head_branch,
        head_sha: pull.head_sha,
        base_branch: pull.base_branch,
        base_sha: pull.base_sha,
        stack_id: pull.stack_id,
    })
}

/// Moves the recorded head of an open pull request from `from` to `to`, unless another
/// refresh moved it first. The head it records gets CI, as every head move does.
#[expect(
    clippy::too_many_arguments,
    reason = "Head refresh carries an optional CI access cap."
)]
async fn move_head(
    pool: &DbPool,
    ci: &CiRuns,
    base: &Repository,
    pull_id: i64,
    from: &str,
    to: &str,
    head_repo: Option<i64>,
    head_branch: &str,
    actor_id: i64,
    ci_access: Option<db::models::AccessLevel>,
) -> Result<bool, RefreshError> {
    // GitHub keeps auto-merge only when the recorded pusher of this head may write the base.
    let pusher = match head_repo {
        Some(id) => match db::repositories::records::get_repository_by_id(pool, id)
            .await
            .map_err(|_| RefreshError::Database)?
        {
            Some(head) => repo_store(&head, ci.cluster.as_ref())
                .branch_publication(head.id, &format!("refs/heads/{head_branch}"))
                .await
                .map_err(|_| RefreshError::Storage)?
                .filter(|record| record.publication.new == to)
                .and_then(|record| record.publication.actor.user_id()),
            None => None,
        },
        None => None,
    };
    let moved = db::pull_requests::records::move_open_pull_request_head(
        pool,
        pull_id,
        from,
        to,
        head_repo,
        head_branch,
        pusher,
    )
        .await
        .map_err(|_| RefreshError::Database)?;
    if !moved {
        return Ok(false);
    }
    let current = db::pull_requests::records::get_pull_request_by_id(pool, pull_id)
        .await
        .map_err(|_| RefreshError::Database)?;
    if let Some(current) = current {
        super::code_owners::request_code_owner_reviews(pool, ci, base, &current, actor_id).await;
        // Describe this successful CAS even if another refresh already moved the row again.
        let mut current = current;
        current.head_sha = to.to_owned();
        let pull_for_ci = current.clone();
        let (author_id, repository) = (current.author_id, base.clone());
        let before = from.to_owned();
        let store = repo_store(base, ci.cluster.as_ref());
        let payload = webhooks::emit(
            pool,
            base,
            events::PULL_REQUEST,
            vec![author_id, actor_id],
            move |owner, people| {
                let (author, sender) = (people.get(author_id), people.get(actor_id));
                let mut payload = super::webhook_payload(
                    "synchronize",
                    &current,
                    &author,
                    Scope {
                        repo: &repository,
                        owner,
                        sender: &sender,
                    },
                    &store,
                );
                payload["before"] = serde_json::json!(before);
                payload["after"] = serde_json::json!(current.head_sha);
                payload
            },
        )
        .await;
        if let Some(access) = ci_access {
            super::workflows::queue_pull_request_event_ci(
                pool,
                ci,
                base,
                &pull_for_ci,
                actor_id,
                access,
                &payload,
            )
            .await;
        }
    }
    Ok(true)
}

/// `head_writer` is a writer of a fork head who asked for this refresh, as by updating the branch.
/// That writer publishes the head with their own authority instead of the recorded publisher's.
#[expect(
    clippy::too_many_arguments,
    reason = "Refresh carries the shared ref listings, an optional CI access cap and an optional head writer."
)]
async fn refresh_one(
    pool: &DbPool,
    moved_repo: &Repository,
    ci: &CiRuns,
    actor_id: i64,
    listed: &mut ListedRefs,
    mut pull: db::pull_requests::records::PullRequestRevisions,
    ci_access: Option<db::models::AccessLevel>,
    head_writer: Option<i64>,
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
            // Keep the base this head contains, including before it joins a stack.
            let fork = match pull.head_repo_id {
                Some(head_repo_id) if head_repo_id == base.id => {
                    let built_on_base = base_store
                        .is_ancestor(&pull.base_sha, &pull.head_sha)
                        .await
                        .map_err(|_| RefreshError::Storage)?;
                    built_on_base.then(|| pull.base_sha.clone())
                }
                _ => None,
            };
            // The refresh read `pull.base_branch`; a re-base since then must make it stale.
            if db::pull_requests::records::move_open_pull_request_base(
                pool,
                pull.id,
                &pull.base_sha,
                &new_base,
                &pull.base_branch,
                fork.as_deref(),
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
        if head.is_none() {
            // With the fork deleted, the pull ref the server last published from it is the pull
            // request's head, as on GitHub; the row takes it when the update after a push was lost.
            match base_store.commit_from_primary(&format!("refs/pull/{}/head", pull.number)).await {
                Ok(commit) if commit.sha != pull.head_sha => {
                    if move_head(pool, ci, &base, pull.id, &pull.head_sha, &commit.sha, pull.head_repo_id, &pull.head_branch, actor_id, ci_access).await? {
                        changed = true;
                        retry = true;
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind == ErrorKind::NotFound => {}
                Err(_) => return Err(RefreshError::Storage),
            }
        }
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
            // A retained ref has no branch provenance: it may come from a publication
            // whose row CAS lost to a rename. Use it only as the storage CAS input,
            // never as evidence for advancing the row. Only the live branch observation
            // below may advance SQL, under the complete recorded-identity CAS.
            let expected_head = match base_store
                .commit_from_primary(&format!("refs/pull/{}/head", pull.number))
                .await
            {
                Ok(commit) => Some(commit.sha),
                Err(error) if error.kind == ErrorKind::NotFound => None,
                Err(_) => return Err(RefreshError::Storage),
            };
            // Storage rechecks this real contributor's live source/base read authority
            // under the publication lock. An imported issue's author is not a credential.
            let publisher = if head.id == base.id {
                Some(actor_id)
            } else {
                head_writer.or(pull.head_publisher_id)
            };
            let publication = match publisher {
                Some(publisher) => {
                    base_store
                        .fetch_pull_head(
                            pool,
                            base.id,
                            head.id,
                            publisher,
                            &repo_store(head, context),
                            pull.number,
                            &new_head,
                            expected_head.as_deref(),
                        )
                        .await
                }
                // Deleted contributors leave previously authorized snapshots usable,
                // but cannot lend another principal authority to publish new commits.
                None => match base_store
                    .commit_from_primary(&format!("refs/pull/{}/head", pull.number))
                    .await
                {
                    Ok(commit) if commit.sha == new_head => Ok(()),
                    Ok(_) => Err(RepoError::write_denied()),
                    Err(error) => Err(error),
                },
            };
            if let Err(error) = publication {
                head_ready = false;
                if error.kind == ErrorKind::Conflict {
                    retry = true;
                } else {
                    head_failed = true;
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
                pull.head_repo_id,
                &pull.head_branch,
                actor_id,
                ci_access,
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
            if changed {
                drop_obsolete_fork(pool, &base_store, pull.id).await;
            }
            return Ok(changed);
        }
        if attempt + 1 < MAX_REFRESH_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(10 * (attempt as u64 + 1))).await;
        }
    }
    Err(RefreshError::Conflict)
}

/// A fork point keeps the base a stacked layer was built on while its head lacks the current base.
/// Once the head contains that base again, as after a restack, the old fork point would show the
/// rewritten layers below as this layer's own changes, so it is dropped. A failure only leaves the
/// fork point for the next refresh.
async fn drop_obsolete_fork(pool: &DbPool, store: &RepoStore, pull_id: i64) {
    let pull = match db::pull_requests::records::get_pull_request_by_id(pool, pull_id).await {
        Ok(Some(pull)) if pull.state == "open" => pull,
        Ok(_) => return,
        Err(error) => {
            tracing::warn!(pull_request_id = pull_id, %error, "could not read the fork point");
            return;
        }
    };
    let Some(fork) = pull.fork_sha.as_deref() else {
        return;
    };
    match store.is_ancestor(&pull.base_sha, &pull.head_sha).await {
        Ok(true) => {
            if let Err(error) = db::pull_requests::records::clear_fork_point(
                pool,
                pull.id,
                fork,
                &pull.base_sha,
                &pull.head_sha,
            )
            .await
            {
                tracing::warn!(pull_request_id = pull_id, %error, "could not drop the fork point");
            }
        }
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(pull_request_id = pull_id, %error, "could not check the fork point")
        }
    }
}

/// Refreshes one pull request against its base repository. Operations on one PR
/// use this path so an unrelated broken source cannot deny them.
pub async fn refresh_open_pull_request(
    pool: &DbPool,
    repository: &Repository,
    ci: &CiRuns,
    actor_id: i64,
    pull_request_id: i64,
    ci_access: Option<db::models::AccessLevel>,
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
    refresh_one(pool, repository, ci, actor_id, &mut listed, pull, ci_access, None).await
}

/// Refreshes a pull request after `actor_id` updated its head branch. When the actor can write the
/// head repository, the new head is published with the actor's own authority, so a contributor's
/// lapsed publication grant does not hold back the head writer's explicit update.
pub async fn refresh_updated_head(
    pool: &DbPool,
    repository: &Repository,
    head: &Repository,
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
    if pull.repo_id != repository.id || pull.head_repo_id != Some(head.id) {
        return Err(RefreshError::MissingBase);
    }
    let actor = db::users::accounts::get_user_by_id(pool, actor_id)
        .await
        .map_err(|_| RefreshError::Database)?;
    let writer = match actor {
        Some(actor) => crate::backend::repositories::access::can_write(pool, &actor, head)
            .await
            .map_err(|_| RefreshError::Database)?,
        None => false,
    };
    let head_writer = (writer && head.id != repository.id).then_some(actor_id);
    let mut listed = ListedRefs::new();
    refresh_one(
        pool,
        repository,
        ci,
        actor_id,
        &mut listed,
        pull,
        Some(db::models::AccessLevel::Owner),
        head_writer,
    )
    .await
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
    ci_access: db::models::AccessLevel,
) -> Result<usize, RefreshError> {
    if let Err(error) = db::vulnerabilities::enqueue(pool, repo.id).await {
        tracing::warn!(repo_id=repo.id,%error,"could not queue dependency vulnerability scan");
    }
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
        match refresh_one(pool, repo, ci, actor_id, &mut listed, pull, (ci_access != db::models::AccessLevel::None).then_some(ci_access), None).await {
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
    if let Err(error) = refresh_open_pull_requests(
        pool,
        repo,
        None,
        ci,
        actor_id,
        crate::backend::db::models::AccessLevel::Owner,
    )
    .await
    {
        tracing::warn!(repo_id = repo.id, %error, "could not refresh open pull requests after a branch moved");
    }
}

#[cfg(test)]
mod tests;
