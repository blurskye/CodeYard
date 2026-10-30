//! Code owners on pull requests: the review requests a pull request gets when it opens or its head
//! moves, and the approvals that a branch rule with `require_code_owner_review` waits for.

use super::reviews::{emit_reviewer_event, notify_review_request};
use crate::backend::ci::CiRuns;
use crate::backend::db;
use crate::backend::db::models::{PullRequest, Repository};
use crate::backend::repositories::code_owners::{load_rules, resolve_owners};
use crate::backend::repositories::store::repo_store;

/// A changed file with review requirements. Unresolved emails still require approval;
/// changing an account's email must not remove that requirement. Self-review is excluded separately.
struct OwnedFile {
    owners: Vec<String>,
    writers: Vec<i64>,
    individual_writers: Vec<i64>,
    teams: Vec<String>,
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
    let eligible_team_owners: std::collections::HashSet<String> = if names
        .iter()
        .any(|owner| owner.starts_with('@') && owner.contains('/'))
    {
        db::pull_requests::team_reviewers::list(pool, repository.id, None, None)
            .await?
            .into_iter()
            .map(|row| format!("@{}/{}", row.org_slug, row.team.slug))
            .collect()
    } else {
        std::collections::HashSet::new()
    };
    let resolved = resolve_owners(pool, repository, names).await?;
    Ok(paths
        .into_iter()
        .filter_map(|(_, owners)| {
            let mut named = Vec::new();
            let mut writers = Vec::new();
            let mut individual_writers = Vec::new();
            let mut teams = Vec::new();
            for owner in owners {
                let ids: Vec<i64> = resolved.get(owner).into_iter().flatten().copied().collect();
                // A real eligible team still owns its files when it temporarily has no members.
                if !ids.is_empty()
                    || !owner.starts_with('@')
                    || eligible_team_owners.contains(owner)
                {
                    named.push(owner.clone());
                    if let Some((_, slug)) = owner
                        .strip_prefix('@')
                        .and_then(|name| name.split_once('/'))
                    {
                        teams.push(slug.to_owned());
                    } else {
                        individual_writers.extend(ids.iter().copied());
                    }
                    writers.extend(ids);
                }
            }
            (!named.is_empty()).then_some(OwnedFile {
                owners: named,
                writers,
                individual_writers,
                teams,
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
    let base_sha = match super::stacks::merge_target(pool, pull).await {
        Ok((_, base_sha, _)) => base_sha,
        Err(error) => {
            tracing::warn!(pull_request_id = pull.id, %error, "code owner target unavailable");
            return;
        }
    };
    let files = match owned_files(pool, ci, repository, &base_sha, &pull.head_sha).await {
        Ok(files) => files,
        Err(error) => {
            tracing::warn!(repo_id = repository.id, pull_number = pull.number, %error, "code owners unavailable for pull request");
            return;
        }
    };
    let requester = match db::users::accounts::get_user_by_id(pool, actor_id).await {
        Ok(Some(requester)) => requester,
        _ => return,
    };
    let mut teams: Vec<_> = files
        .iter()
        .flat_map(|file| file.teams.iter().cloned())
        .collect();
    teams.sort();
    teams.dedup();
    let mut requested = Vec::new();
    for slugs in teams.chunks(100) {
        match db::pull_requests::team_reviewers::change(
            pool,
            actor_id,
            pull.id,
            slugs,
            db::pull_requests::team_reviewers::ReviewTeamAction::Request,
            Some(pull),
        )
        .await
        {
            Ok(added) => requested.extend(added),
            Err(error) => {
                tracing::warn!(pull_request_id=pull.id, %error, "Could not request code owner teams")
            }
        }
    }
    super::team_reviewers::notify(pool, ci.notifications(), &requester, repository, pull, &requested).await;
    super::team_reviewers::emit_events(
        pool,
        ci.cluster.as_ref(),
        repository,
        pull,
        "review_requested",
        actor_id,
        &requested,
    )
    .await;
    let mut wanted: Vec<i64> = files
        .iter()
        .flat_map(|file| file.individual_writers.iter().copied())
        .filter(|id| *id != pull.author_id)
        .collect();
    wanted.sort_unstable();
    wanted.dedup();
    if wanted.is_empty() {
        return;
    }
    let owner = match db::ci::ci_repository_owner(pool, repository.id).await {
        Ok(Some(owner)) => owner,
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
        emit_reviewer_event(
            pool,
            ci.cluster.as_ref(),
            repository,
            pull,
            "review_requested",
            actor_id,
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
