//! Code owners on pull requests: the review requests a pull request gets when it opens or its head
//! moves, and the approvals that a branch rule with `require_code_owner_review` waits for.

use super::reviews::notify_review_request;
use crate::backend::ci::CiRuns;
use crate::backend::db;
use crate::backend::db::models::{PullRequest, Repository};
use crate::backend::repositories::code_owners::{load_rules, resolve_owners};
use crate::backend::repositories::store::repo_store;

/// A changed file with owners who can review it. `owners` are the written names that have such a
/// user, and `writers` their users, including the author. Self-review is excluded separately.
struct OwnedFile {
    owners: Vec<String>,
    writers: Vec<i64>,
}

async fn owned_files(
    pool: &db::DbPool,
    ci: &CiRuns,
    repository: &Repository,
    base_sha: &str,
    head_sha: &str,
) -> anyhow::Result<Vec<OwnedFile>> {
    let cluster = ci.cluster.as_ref();
    let rules = load_rules(repository, cluster, base_sha).await?;
    if rules.is_empty() {
        return Ok(Vec::new());
    }
    let changed = repo_store(repository, cluster)
        .changed_paths(base_sha, head_sha)
        .await?;
    let paths = rules.owned_paths(&changed)?;
    let names: Vec<&str> = paths
        .iter()
        .flat_map(|(_, owners)| owners.iter().map(String::as_str))
        .collect();
    let resolved = resolve_owners(pool, repository, names).await?;
    Ok(paths
        .into_iter()
        .filter_map(|(_, owners)| {
            let mut named = Vec::new();
            let mut writers = Vec::new();
            for owner in owners {
                let ids: Vec<i64> = resolved
                    .get(&owner)
                    .into_iter()
                    .flatten()
                    .copied()
                    .collect();
                if !ids.is_empty() {
                    named.push(owner);
                    writers.extend(ids);
                }
            }
            (!writers.is_empty()).then_some(OwnedFile {
                owners: named,
                writers,
            })
        })
        .collect())
}

/// Asks the owners of the files that `pull` changes to review it, when it opens, becomes ready or
/// gets a new head. An owner who already reviewed is not asked again, and an open request stays
/// as it is. Failures are logged; the next head move asks again.
pub async fn request_code_owner_reviews(
    pool: &db::DbPool,
    ci: &CiRuns,
    repository: &Repository,
    pull: &PullRequest,
    actor_id: i64,
) {
    if pull.state != "open" || pull.is_draft {
        return;
    }
    let files = match owned_files(pool, ci, repository, &pull.base_sha, &pull.head_sha).await {
        Ok(files) => files,
        Err(error) => {
            tracing::warn!(repo_id = repository.id, pull_number = pull.number, %error, "code owners unavailable for pull request");
            return;
        }
    };
    let mut wanted: Vec<i64> = files
        .iter()
        .flat_map(|file| file.writers.iter().copied())
        .filter(|id| *id != pull.author_id)
        .collect();
    wanted.sort_unstable();
    wanted.dedup();
    if wanted.is_empty() {
        return;
    }
    let decided = match db::pull_requests::reviews::list_pull_request_review_decisions(
        pool, pull.id,
    )
    .await
    {
        Ok(decided) => decided,
        Err(error) => {
            tracing::warn!(repo_id = repository.id, pull_number = pull.number, %error, "reviews unavailable for code owner requests");
            return;
        }
    };
    let requested = match db::pull_requests::reviews::list_pull_request_reviewers(pool, pull.id)
        .await
    {
        Ok(requested) => requested,
        Err(error) => {
            tracing::warn!(pull_request_id = pull.id, %error, "review requests unavailable for code owners");
            return;
        }
    };
    wanted.retain(|id| {
        !decided.iter().any(|review| review.author_id == *id)
            && !requested.iter().any(|reviewer| reviewer.id == *id)
    });
    if wanted.is_empty() {
        return;
    }
    let owner = match db::ci::ci_repository_owner(pool, repository.id).await {
        Ok(Some(owner)) => owner,
        _ => return,
    };
    let requester = match db::users::accounts::get_user_by_id(pool, actor_id).await {
        Ok(Some(requester)) => requester,
        _ => return,
    };
    let targets = match db::pull_requests::reviews::request_code_owner_reviews(
        pool, pull, actor_id, &wanted,
    )
    .await
    {
        Ok(targets) => targets,
        Err(error) => {
            tracing::warn!(pull_request_id = pull.id, %error, "could not request code owner reviews");
            return;
        }
    };
    for target in targets {
        notify_review_request(
            pool,
            ci.notifications(),
            &requester,
            &owner,
            repository,
            pull,
            &target,
        )
        .await;
    }
}

/// The owners whose approval a merge still lacks: every changed file with owners needs an approval
/// from one of them. `approvers` are the users whose approvals count for this head. Returns the
/// owners as written, each once, in file order.
pub(super) async fn missing_owner_approvals(
    pool: &db::DbPool,
    ci: &CiRuns,
    repository: &Repository,
    base_sha: &str,
    pull: &PullRequest,
    approvers: &[i64],
) -> anyhow::Result<Vec<String>> {
    let files = owned_files(pool, ci, repository, base_sha, &pull.head_sha).await?;
    let mut missing: Vec<String> = Vec::new();
    for file in files {
        if file
            .writers
            .iter()
            .any(|id| *id != pull.author_id && approvers.contains(id))
        {
            continue;
        }
        for owner in file.owners {
            if !missing.contains(&owner) {
                missing.push(owner);
            }
        }
    }
    Ok(missing)
}
