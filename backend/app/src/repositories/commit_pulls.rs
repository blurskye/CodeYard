//! Associations are derived from published Git ancestry, not commit-message references.

use crate::cluster::context::NodeContext;
use crate::repositories::store::repo_store;
use crate::repositories::store::RepoStore;
use codeyard_db_core::models::PullRequest;
use codeyard_db_core::models::Repository;
use codeyard_git as git;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_git::CommitInfo;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug)]
pub enum AssociationError {
    Access(codeyard_identity::repository_access::AccessError),
    Git(RepoError),
    Limit,
    Unavailable,
}

pub struct CommitPullQuery<'a> {
    pub repository_id: i64,
    pub actor: Option<i64>,
    pub reference: &'a str,
    pub limit: usize,
    pub offset: usize,
}

pub struct CommitPullPage {
    pub repository: Repository,
    pub commit: CommitInfo,
    pub pulls: Vec<PullRequest>,
    pub has_more: bool,
}

async fn contains(
    store: &RepoStore,
    commit: &str,
    tip: &str,
    cache: &mut HashMap<String, bool>,
) -> Result<bool, AssociationError> {
    if commit == tip {
        return Ok(true);
    }
    if let Some(result) = cache.get(tip) {
        return Ok(*result);
    }
    let result = store
        .is_ancestor(commit, tip)
        .await
        .map_err(AssociationError::Git)?;
    cache.insert(tip.to_owned(), result);
    Ok(result)
}

async fn introduced(
    store: &RepoStore,
    commit: &str,
    pull: &PullRequest,
    on_default: bool,
    cache: &mut HashMap<String, bool>,
) -> Result<bool, AssociationError> {
    let boundary = pull.diff_base();
    if contains(store, commit, boundary, cache).await? {
        return Ok(false);
    }
    // A merged request records the base it merged into, which may be newer than its fork point.
    if pull.merge_commit_sha.is_some()
        && pull.base_sha != boundary
        && contains(store, commit, &pull.base_sha, cache).await?
    {
        return Ok(false);
    }
    // Stack layers share a final merge tip; its ancestry is not every layer's content.
    if pull.stack_id.is_some() {
        return Ok(pull.merge_commit_sha.as_deref() == Some(commit)
            || contains(store, commit, &pull.head_sha, cache).await?);
    }
    let tip = if on_default {
        pull.merge_commit_sha.as_deref().unwrap_or(&pull.head_sha)
    } else {
        &pull.head_sha
    };
    if contains(store, commit, tip, cache).await? {
        return Ok(true);
    }
    if !on_default {
        if let Some(merge) = &pull.merge_commit_sha {
            return contains(store, commit, merge, cache).await;
        }
    }
    Ok(false)
}

/// Commit IDs on the default branch expose only merged introductions. Named branch selection
/// and commits elsewhere include open and merged requests. Already-present base commits do not count.
/// Pagination is over matches, never over the database candidates that were inspected.
pub async fn associated_pulls(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    query: CommitPullQuery<'_>,
) -> Result<CommitPullPage, AssociationError> {
    if !(1..=100).contains(&query.limit) || query.offset > 100_000 {
        return Err(AssociationError::Limit);
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        let repository = codeyard_identity::repository_access::check_read(
            pool,
            query.repository_id,
            query.actor,
        )
        .await
        .map_err(AssociationError::Access)?;
        let store = repo_store(&repository, cluster);
        let refs = store
            .refs_from_primary()
            .await
            .map_err(AssociationError::Git)?;
        let immutable = git::is_full_commit_id(query.reference);
        let branch = if immutable
            || (query.reference.starts_with("refs/") && !query.reference.starts_with("refs/heads/"))
        {
            None
        } else {
            git::branch_ref(
                query
                    .reference
                    .strip_prefix("refs/heads/")
                    .unwrap_or(query.reference),
            )
            .and_then(|branch| refs.get(&branch))
        };
        let commit = store
            .commit_from_primary(branch.map_or(query.reference, String::as_str))
            .await
            .map_err(AssociationError::Git)?;
        let mut cache = HashMap::new();
        let on_default = if branch.is_some() {
            false
        } else {
            match store.commit_from_primary("HEAD").await {
                Ok(default) => contains(&store, &commit.sha, &default.sha, &mut cache).await?,
                Err(error) if error.kind == ErrorKind::NotFound => false,
                Err(error) => return Err(AssociationError::Git(error)),
            }
        };
        let mut before = i64::MAX;
        let mut inspected = 0;
        let mut skipped = 0;
        let mut pulls = Vec::new();
        loop {
            let candidates =
                codeyard_db_repos::pull_requests::records::commit_association_candidates(
                    pool,
                    repository.id,
                    before,
                    256,
                    on_default,
                )
                .await
                .map_err(|_| AssociationError::Unavailable)?;
            if candidates.is_empty() {
                break;
            }
            for pull in candidates {
                before = pull.id;
                inspected += 1;
                if inspected > 8192 {
                    return Err(AssociationError::Limit);
                }
                if on_default && !pull.merged {
                    continue;
                }
                if !introduced(&store, &commit.sha, &pull, on_default, &mut cache).await? {
                    continue;
                }
                if skipped < query.offset {
                    skipped += 1;
                    continue;
                }
                if pulls.len() == query.limit {
                    let repository = codeyard_identity::repository_access::check_read(
                        pool,
                        repository.id,
                        query.actor,
                    )
                    .await
                    .map_err(AssociationError::Access)?;
                    return Ok(CommitPullPage {
                        repository,
                        commit,
                        pulls,
                        has_more: true,
                    });
                }
                pulls.push(pull);
            }
        }
        let repository =
            codeyard_identity::repository_access::check_read(pool, repository.id, query.actor)
                .await
                .map_err(AssociationError::Access)?;
        Ok(CommitPullPage {
            repository,
            commit,
            pulls,
            has_more: false,
        })
    })
    .await
    .map_err(|_| AssociationError::Limit)?
}
