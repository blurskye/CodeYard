use super::OperationError;
use crate::backend::ci::CiRuns;
use crate::backend::ci::RefUpdate;
use crate::backend::db;
use crate::backend::notifications::create::create_and_broadcast_notification;
use crate::backend::notifications::create::NotificationRequest;
use crate::backend::notifications::NotificationBroadcaster;
use crate::backend::repositories::protection::{
    check_branch_write, BranchWrite, History, Refusal, WriteVia,
};
use crate::backend::repositories::pull_requests;
use crate::backend::repositories::store::allowed_merge_methods;
use crate::backend::repositories::store::repo_store;
use crate::backend::repositories::store::validate_merge_revision;
use crate::backend::repositories::store::ErrorKind;
use crate::backend::repositories::store::MergeMethod;
use crate::backend::repositories::store::MergeRequest as StoreMerge;
use crate::backend::repositories::store::RepoError;
use crate::webui::pages::collaboration::MERGE_COMMIT_REFUSED;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;
#[derive(Deserialize)]
pub struct MergeRequest {
    pub expected_head: String,
    pub expected_base: String,
    /// The base branch the client read. Clients that predate it omit it and are checked on the
    /// commits alone. When present, a different base branch is refused like a stale commit.
    #[serde(default)]
    pub expected_base_branch: Option<String>,
    /// The branch the client confirmed the merge lands in: the trunk of the stack for a layer,
    /// otherwise the base. Omitted means no check. When present and different from the branch the
    /// merge lands in now, the merge is refused like a stale revision.
    #[serde(default)]
    pub expected_target: Option<String>,
    /// How the commits reach the base branch. Omitted means a merge commit.
    #[serde(default)]
    pub merge_method: MergeMethod,
}

#[derive(Serialize)]
pub struct MergeResponse {
    pub sha: String,
    pub merged: bool,
    pub message: String,
}

fn method_not_allowed(method: MergeMethod) -> OperationError {
    let kind = match method {
        MergeMethod::Merge => "Merge commits",
        MergeMethod::Squash => "Squash merging",
        MergeMethod::Rebase => "Rebase merging",
    };
    OperationError::Conflict {
        code: "merge_method_not_allowed",
        message: format!("{kind} is not allowed in this repository."),
    }
}

/// Maps a failed merge preparation. A conflict names its files, an unsupported history is
/// refused, and any other failure means that storage could not build the commit.
pub(super) fn prepare_error(error: RepoError) -> OperationError {
    if error.is_storage_node_unavailable() {
        return OperationError::Unavailable {
            code: "storage_unavailable",
            message: "storage node unavailable".to_owned(),
        };
    }
    match error.kind {
        ErrorKind::Conflict => OperationError::Conflict {
            code: "merge_conflict",
            message: error.message,
        },
        ErrorKind::Invalid => OperationError::Conflict {
            code: "unsupported_merge",
            message: error.message,
        },
        _ => OperationError::Unavailable {
            code: "git_unavailable",
            message: "Repository storage is unavailable; merge denied.".to_owned(),
        },
    }
}

/// Counts each reviewer's latest decision. An approval counts for the head only when
/// `dismiss_stale_reviews` is set; otherwise it counts for any commit. A change request
/// blocks whatever commit it was written on.
fn count_effective_approvals(
    decisions: &[db::pull_requests::reviews::PullRequestReviewDecision],
    head_sha: &str,
    dismiss_stale_reviews: bool,
) -> (usize, bool) {
    let mut approvals = 0usize;
    let mut has_blocking_change_request = false;

    for decision in decisions {
        match decision.state.as_str() {
            "approved"
                if !dismiss_stale_reviews || decision.commit_sha.as_deref() == Some(head_sha) =>
            {
                approvals += 1
            }
            "changes_requested" => has_blocking_change_request = true,
            _ => {}
        }
    }

    (approvals, has_blocking_change_request)
}

/// Each reviewer's decision that counts toward protection rules: from users other
/// than the PR author who currently have write access.
async fn eligible_reviews(
    pool: &db::DbPool,
    pull_request: &db::models::PullRequest,
    repository: &db::models::Repository,
) -> anyhow::Result<Vec<db::pull_requests::reviews::PullRequestReviewDecision>> {
    let decisions =
        db::pull_requests::reviews::list_pull_request_review_decisions(pool, pull_request.id)
            .await?;
    let mut eligible = Vec::new();
    for decision in decisions {
        if decision.author_id == pull_request.author_id {
            continue;
        }
        let allowed = match db::users::accounts::get_user_by_id(pool, decision.author_id).await? {
            Some(reviewer) => {
                crate::backend::repositories::access::can_write(pool, &reviewer, repository).await?
            }
            None => false,
        };
        if allowed {
            eligible.push(decision);
        }
    }
    Ok(eligible)
}

fn required_status_check_failures(
    head_sha: &str,
    required_checks: &[String],
    statuses: &[db::models::CommitStatus],
    ci_runs: &[db::models::CiRun],
) -> Vec<String> {
    let by_context: HashMap<&str, &str> = statuses
        .iter()
        .filter(|status| status.sha == head_sha)
        .map(|status| (status.context.as_str(), status.state.as_str()))
        .collect();

    let mut ci_by_context: HashMap<String, Vec<&db::models::CiRun>> = HashMap::new();
    for run in ci_runs
        .iter()
        .filter(|run| run.head_sha.as_deref() == Some(head_sha))
    {
        for context in [
            run.commit_status_context(),
            run.legacy_commit_status_context(),
        ] {
            ci_by_context.entry(context).or_default().push(run);
        }
    }
    let mut failures = Vec::new();
    for required in required_checks {
        // Old stored successes may have hidden another producer's failure.
        // CI requirements use the latest actual runs, including released names.
        if db::repositories::protection::server_owned_status_context(required) {
            match ci_by_context.get(required) {
                Some(runs) => {
                    if runs.iter().any(|run| run.status != "success") {
                        let state = if runs.iter().any(|run| {
                            !matches!(run.status.as_str(), "success" | "queued" | "running")
                        }) {
                            "failure"
                        } else {
                            "pending"
                        };
                        failures.push(format!("{required} ({state})"));
                    }
                }
                None => failures.push(format!("{required} (missing)")),
            }
            continue;
        }
        match by_context.get(required.as_str()) {
            Some(&"success") => {}
            Some(state) => failures.push(format!("{} ({})", required, state)),
            None => failures.push(format!("{} (missing)", required)),
        }
    }

    failures
}

/// Every refusal the protection rules give to a layer that a merge brings in, in the order the
/// merge checks them. The merge refuses with the first one, and the pull request page lists them
/// all. A storage failure is not a refusal: it ends the check with an error.
async fn merge_blockers(
    pool: &db::DbPool,
    ci: &CiRuns,
    repository: &db::models::Repository,
    user: &db::models::User,
    pull_request: &db::models::PullRequest,
    target: (&str, &str),
    method: MergeMethod,
) -> Result<Vec<OperationError>, OperationError> {
    // The pull request itself is the merge's way in. The shared policy decides who may move
    // the branch and returns the rules this merge is still held to.
    let write = BranchWrite {
        branch: target.0,
        old: Some(target.1),
        new: Some(pull_request.head_sha.as_str()),
        via: WriteVia::Merge,
    };
    let rules =
        match check_branch_write(pool, repository, user, &write, &History::SingleCommit).await {
            Ok(rules) => rules,
            // Without the rules nothing else can be judged, so this refusal is the only blocker.
            Err(Refusal::Blocked(reason)) => {
                return Ok(vec![OperationError::Denied {
                    code: "branch_protection",
                    message: reason.to_owned(),
                }])
            }
            Err(Refusal::Unavailable) => {
                return Err(OperationError::Unavailable {
                    code: "database_error",
                    message: "Collaboration storage is temporarily unavailable".to_owned(),
                })
            }
        };
    let mut blockers = Vec::new();
    // Every rule judges the same reviews, so they are read once, when the first rule needs them.
    let mut reviews = None;
    for rule in &rules {
        if rule.require_signed_commits || rule.require_code_owner_reviews {
            blockers.push(OperationError::Conflict {
                code: "unsupported_protection",
                message:
                    "Signed-commit/code-owner enforcement is unavailable, so this merge is denied. Delete the branch protection rule and create it again to clear these flags."
                        .to_owned(),
            });
        }

        if reviews.is_none() {
            reviews = Some(
                eligible_reviews(pool, pull_request, repository)
                    .await
                    .map_err(|_| OperationError::Unavailable {
                        code: "database_error",
                        message: "Collaboration storage is temporarily unavailable".to_owned(),
                    })?,
            );
        }
        let decisions = reviews.as_deref().unwrap_or_default();
        let (approval_count, has_changes_requested) = count_effective_approvals(
            decisions,
            &pull_request.head_sha,
            rule.dismiss_stale_reviews,
        );
        if has_changes_requested {
            blockers.push(OperationError::Conflict {
                code: "branch_protection",
                message: "Cannot merge while review state is changes_requested".to_owned(),
            });
        }
        if rule.required_approving_reviews > 0
            && approval_count < rule.required_approving_reviews as usize
        {
            blockers.push(OperationError::Conflict {
                code: "branch_protection",
                message: format!(
                    "{} approving review{} required, found {}",
                    rule.required_approving_reviews,
                    if rule.required_approving_reviews == 1 {
                        ""
                    } else {
                        "s"
                    },
                    approval_count
                ),
            });
        }

        if rule.require_conversation_resolution {
            let unresolved =
                db::pull_requests::reviews::count_unresolved_conversations(pool, pull_request.id)
                    .await
                    .map_err(|_| OperationError::Unavailable {
                        code: "database_error",
                        message: "Collaboration storage is temporarily unavailable".to_owned(),
                    })?;
            if unresolved > 0 {
                blockers.push(OperationError::Conflict {
                    code: "unresolved_conversations",
                    message: format!(
                        "{unresolved} conversation(s) must be resolved before merging"
                    ),
                });
            }
        }

        if rule.require_status_checks {
            let mut required_status_checks = Vec::new();
            let mut seen = HashSet::new();
            for check in &rule.required_status_checks {
                let trimmed = check.trim();
                if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                    required_status_checks.push(trimmed.to_string());
                }
            }
            if required_status_checks.is_empty() {
                blockers.push(OperationError::Conflict {
                    code: "branch_protection",
                    message:
                        "Status checks are required but no contexts are configured; merge denied."
                            .to_owned(),
                });
            } else {
                let statuses = db::repositories::protection::list_commit_statuses(
                    pool,
                    repository.id,
                    &pull_request.head_sha,
                )
                .await
                .map_err(|_| OperationError::Unavailable {
                    code: "database_error",
                    message: "Collaboration storage is temporarily unavailable".to_owned(),
                })?;
                let ci_runs = db::ci::latest_ci_runs_for_commit(
                    pool,
                    repository.id,
                    &pull_request.head_sha,
                    pull_request.id,
                )
                .await
                .map_err(|_| OperationError::Unavailable {
                    code: "database_error",
                    message: "Collaboration storage is temporarily unavailable".to_owned(),
                })?;
                let failures = required_status_check_failures(
                    &pull_request.head_sha,
                    &required_status_checks,
                    &statuses,
                    &ci_runs,
                );
                if !failures.is_empty() {
                    blockers.push(OperationError::Conflict {
                        code: "branch_protection",
                        message: format!(
                            "Required status checks not passing: {}",
                            failures.join(", ")
                        ),
                    });
                }
            }
        }

        // The merge method always makes a merge commit, even when the head is up to date. Squash
        // and rebase leave a linear history, so only the merge method is refused here.
        if rule.require_linear_history && method == MergeMethod::Merge {
            blockers.push(OperationError::Conflict {
                code: "branch_protection",
                message: MERGE_COMMIT_REFUSED.to_owned(),
            });
        }
        if rule.require_up_to_date_branch {
            let up_to_date = repo_store(repository, ci.cluster.as_ref())
                .is_ancestor(target.1, &pull_request.head_sha)
                .await
                .map_err(|_| OperationError::Unavailable {
                    code: "git_unavailable",
                    message: "Git ancestry is unavailable; merge denied.".to_owned(),
                })?;
            if !up_to_date {
                blockers.push(OperationError::Conflict {
                    code: "branch_protection",
                    message: "Head branch is out of date with base branch".to_owned(),
                });
            }
        }
    }
    Ok(blockers)
}

/// Applies the protection rules of `target` (the branch and its tip) to one layer that a merge
/// brings in. The layer's own approvals, checks and conversations are judged at its head.
async fn check_merge_policy(
    pool: &db::DbPool,
    ci: &CiRuns,
    repository: &db::models::Repository,
    user: &db::models::User,
    pull_request: &db::models::PullRequest,
    target: (&str, &str),
    method: MergeMethod,
) -> Result<(), OperationError> {
    let blockers = merge_blockers(pool, ci, repository, user, pull_request, target, method).await?;
    blockers.into_iter().next().map_or(Ok(()), Err)
}

/// What a merge of `pull_request` would refuse right now, for the pull request page to list above
/// the merge button. `method` is the one the merge form preselects. The list is advisory: the merge
/// repeats every check, and a failed read is logged and lists nothing instead of failing the page.
pub async fn page_blockers(
    pool: &db::DbPool,
    ci: &CiRuns,
    repository: &db::models::Repository,
    viewer: &db::models::User,
    pull_request: &db::models::PullRequest,
    method: MergeMethod,
) -> Vec<String> {
    match page_blocker_messages(pool, ci, repository, viewer, pull_request, method).await {
        Ok(messages) => messages,
        Err(error) => {
            tracing::debug!(pull_request_id = pull_request.id, %error, "merge blockers unavailable; the page lists none");
            Vec::new()
        }
    }
}

async fn page_blocker_messages(
    pool: &db::DbPool,
    ci: &CiRuns,
    repository: &db::models::Repository,
    viewer: &db::models::User,
    pull_request: &db::models::PullRequest,
    method: MergeMethod,
) -> Result<Vec<String>, OperationError> {
    if pull_request.state != "open" {
        return Ok(Vec::new());
    }
    if repository.is_archived {
        return Ok(vec![archived_refusal().to_string()]);
    }
    let layers = match open_chain(pool, pull_request).await {
        Ok(layers) => layers,
        // A closed or cyclic layer below the pull request refuses the merge with its own message.
        Err(OperationError::Conflict { message, .. }) => return Ok(vec![message]),
        Err(error) => return Err(error),
    };
    let Some(bottom) = layers.first() else {
        return Ok(Vec::new());
    };
    let drafts: Vec<String> = layers
        .iter()
        .filter(|layer| layer.is_draft)
        .map(|layer| draft_refusal(layer.number).to_string())
        .collect();
    if !drafts.is_empty() {
        return Ok(drafts);
    }
    let refs = repo_store(repository, ci.cluster.as_ref())
        .refs()
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "git_unavailable",
            message: "Repository storage is unavailable; merge denied.".to_owned(),
        })?;
    // The merge lands on the tip of the bottom layer's base branch, as it is now.
    let Some(base_sha) = refs.get(&format!("refs/heads/{}", bottom.base_branch)) else {
        return Ok(vec![base_branch_missing().to_string()]);
    };
    let mut messages = Vec::new();
    for layer in &layers {
        let blockers = merge_blockers(
            pool,
            ci,
            repository,
            viewer,
            layer,
            (&bottom.base_branch, base_sha),
            method,
        )
        .await?;
        // Each refusal is named the way check_chain_policy names it, so the list matches the merge.
        messages.extend(
            blockers
                .into_iter()
                .map(|blocker| named_layer(layer.number, blocker).to_string()),
        );
    }
    Ok(messages)
}

/// The open layers from the bottom of the stack of `pull_request` up to it, bottom first. A closed
/// or cyclic layer below it refuses the chain.
pub(super) async fn open_chain(
    pool: &db::DbPool,
    pull_request: &db::models::PullRequest,
) -> Result<Vec<db::models::PullRequest>, OperationError> {
    let chain = pull_requests::stacks::chain_below(pool, pull_request)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?;
    match chain {
        pull_requests::stacks::Chain::Open(layers) => Ok(layers),
        pull_requests::stacks::Chain::BelowClosed(number) => Err(OperationError::Conflict {
            code: "stack_layer_closed",
            message: format!(
                "Pull request #{number} below it is closed. Reopen it or unstack this pull request."
            ),
        }),
        pull_requests::stacks::Chain::Broken => Err(OperationError::Conflict {
            code: "stack_broken",
            message:
                "These pull requests depend on each other, so they cannot be merged as a stack."
                    .to_owned(),
        }),
    }
}

/// The open layers that a merge of `pull_request` brings in, bottom first, with `pull_request`
/// last. Squash and rebase carry only one pull request, so a pull request with open layers below
/// it can only be merged with a merge commit.
async fn stack_chain(
    pool: &db::DbPool,
    pull_request: &db::models::PullRequest,
    method: MergeMethod,
) -> Result<Vec<db::models::PullRequest>, OperationError> {
    let layers = open_chain(pool, pull_request).await?;
    if layers.len() > 1 && method != MergeMethod::Merge {
        return Err(OperationError::Conflict {
            code: "stack_needs_merge_commit",
            message: "Squash and rebase are unavailable for a pull request with open pull requests below it. Merge it with a merge commit.".to_owned(),
        });
    }
    Ok(layers)
}

/// The refusal for a merge whose submitted revisions or destination are no longer the current ones.
fn stale_revision() -> OperationError {
    OperationError::Conflict {
        code: "revision_conflict",
        message: "Expected head/base must match the admitted pull request revisions. Reload and re-review changed revisions.".to_owned(),
    }
}

/// Brings the layers of a chain up to their branches, as a merge of one pull request does, and
/// returns them as they now stand. The chosen layer was refreshed before the chain was read.
async fn refresh_chain(
    pool: &db::DbPool,
    ci: &CiRuns,
    repository: &db::models::Repository,
    user_id: i64,
    chosen: i64,
    chain: Vec<db::models::PullRequest>,
) -> Result<Vec<db::models::PullRequest>, OperationError> {
    let mut layers = Vec::with_capacity(chain.len());
    for layer in chain {
        if layer.id == chosen {
            layers.push(layer);
            continue;
        }
        pull_requests::refresh_open_pull_request(pool, repository, ci, user_id, layer.id)
            .await
            .map_err(|_| OperationError::Unavailable {
                code: "git_unavailable",
                message: "Pull request revisions could not be refreshed; merge denied.".into(),
            })?;
        let refreshed = db::pull_requests::records::get_pull_request_by_id(pool, layer.id)
            .await
            .map_err(|_| OperationError::Unavailable {
                code: "database_error",
                message: "Collaboration storage is temporarily unavailable".to_owned(),
            })?
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Pull request not found".to_owned(),
            })?;
        layers.push(refreshed);
    }
    Ok(layers)
}

/// The ref that a merge verifies for a layer: its head branch, or the pull ref when its head is in
/// a fork.
fn head_ref(repository: &db::models::Repository, pull_request: &db::models::PullRequest) -> String {
    if pull_request.head_repo_id == Some(repository.id) {
        pull_request.head_branch.clone()
    } else {
        format!("refs/pull/{}/head", pull_request.number)
    }
}

/// Whether the head branch of a layer still exists, in this repository or in its fork.
async fn head_branch_exists(
    pool: &db::DbPool,
    ci: &CiRuns,
    repository: &db::models::Repository,
    branches: &HashMap<String, String>,
    pull_request: &db::models::PullRequest,
) -> Result<bool, OperationError> {
    let reference = format!("refs/heads/{}", pull_request.head_branch);
    if pull_request.head_repo_id == Some(repository.id) {
        return Ok(branches.contains_key(&reference));
    }
    let Some(id) = pull_request.head_repo_id else {
        return Ok(true);
    };
    match db::repositories::records::get_repository_by_id(pool, id).await {
        Ok(Some(head)) => Ok(repo_store(&head, ci.cluster.as_ref())
            .refs()
            .await
            .map_err(|_| OperationError::Unavailable {
                code: "git_unavailable",
                message: "Head repository storage is unavailable".to_owned(),
            })?
            .contains_key(&reference)),
        Ok(None) => Ok(true),
        Err(_) => Err(OperationError::Unavailable {
            code: "database_error",
            message: "Head repository is unavailable".to_owned(),
        }),
    }
}

/// Each layer must contain the head of the layer below it. A layer pushed to after it was stacked
/// leaves the layers above it stale. The bottom layer must also not sit on a squashed or rebased
/// layer below it. Either way, the stack must be rebased before any of it merges.
async fn check_restacked(
    pool: &db::DbPool,
    ci: &CiRuns,
    repository: &db::models::Repository,
    layers: &[db::models::PullRequest],
    branches: &HashMap<String, String>,
) -> Result<(), OperationError> {
    let store = repo_store(repository, ci.cluster.as_ref());
    for (below, above) in layers.iter().zip(layers.iter().skip(1)) {
        let contains = store
            .is_ancestor(&below.head_sha, &above.head_sha)
            .await
            .map_err(|_| OperationError::Unavailable {
                code: "git_unavailable",
                message: "Git ancestry is unavailable; merge denied.".to_owned(),
            })?;
        if !contains {
            return Err(needs_restack());
        }
    }
    if let Some(bottom) = layers.first() {
        if pull_requests::restack::squashed_below(pool, &store, branches, bottom).await? {
            return Err(needs_restack());
        }
    }
    Ok(())
}

fn needs_restack() -> OperationError {
    OperationError::Conflict {
        code: "stack_needs_restack",
        message: "Rebase the stack first: press Rebase stack in the stack panel.".to_owned(),
    }
}

/// Applies the protection rules of the trunk, which the bottom layer is based on, to every layer.
/// A refusal names the layer it is about.
async fn check_chain_policy(
    pool: &db::DbPool,
    ci: &CiRuns,
    repository: &db::models::Repository,
    user: &db::models::User,
    layers: &[db::models::PullRequest],
    method: MergeMethod,
) -> Result<(), OperationError> {
    let Some(bottom) = layers.first() else {
        return Ok(());
    };
    let target = (bottom.base_branch.as_str(), bottom.base_sha.as_str());
    for layer in layers {
        check_merge_policy(pool, ci, repository, user, layer, target, method)
            .await
            .map_err(|error| named_layer(layer.number, error))?;
    }
    Ok(())
}

/// The refusal for an archived repository. The merge and the pull request page give this text.
fn archived_refusal() -> OperationError {
    OperationError::Denied {
        code: "archived",
        message: "Archived repositories cannot be merged.".to_owned(),
    }
}

/// The refusal for a draft pull request. The merge and the pull request page give this text.
fn draft_refusal(number: i32) -> OperationError {
    OperationError::Conflict {
        code: "unsupported_merge",
        message: format!(
            "Pull request #{number} is a draft. Draft pull requests cannot be merged through this operation."
        ),
    }
}

/// The refusal for a base branch that no longer exists. The merge and the pull request page give
/// this text.
fn base_branch_missing() -> OperationError {
    OperationError::Conflict {
        code: "base_branch_missing",
        message: "The base branch no longer exists. Restore it to merge this pull request."
            .to_owned(),
    }
}

/// Puts the number of the pull request a refusal is about in front of its message.
fn named_layer(number: i32, error: OperationError) -> OperationError {
    match error {
        OperationError::Conflict { code, message } => OperationError::Conflict {
            code,
            message: format!("Pull request #{number}: {message}"),
        },
        OperationError::Denied { code, message } => OperationError::Denied {
            code,
            message: format!("Pull request #{number}: {message}"),
        },
        other => other,
    }
}

/// Whether `now` still has the branches, commits and state that a prepared merge was checked
/// against.
fn same_revision(now: &db::models::PullRequest, prepared: &db::models::PullRequest) -> bool {
    now.id == prepared.id
        && now.repo_id == prepared.repo_id
        && now.number == prepared.number
        && now.head_repo_id == prepared.head_repo_id
        && now.head_branch == prepared.head_branch
        && now.base_branch == prepared.base_branch
        && now.head_sha == prepared.head_sha
        && now.base_sha == prepared.base_sha
        && now.state == prepared.state
        && now.is_draft == prepared.is_draft
}

pub struct PreparedMerge {
    actor_id: i64,
    owner: String,
    repo: String,
    repository: db::models::Repository,
    /// The pull request that was asked to merge. It is the last of `layers`.
    pull_request: db::models::PullRequest,
    /// The open layers the merge brings in, bottom first.
    layers: Vec<db::models::PullRequest>,
    merge: StoreMerge,
    merge_sha: String,
}

pub async fn prepare_pull_request_merge(
    pool: &db::DbPool,
    ci: &CiRuns,
    actor_id: i64,
    (owner, repo, number): (String, String, i32),
    request: MergeRequest,
) -> Result<PreparedMerge, OperationError> {
    let repository = db::repositories::records::get_repository(pool, &owner, &repo)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;

    let user = super::actor(pool, actor_id).await?;
    super::require_write(pool, &user, &repository).await?;

    let pull_request = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;
    // A merge uses this pull request's branches as they are now. An unrelated
    // broken fork must not deny a healthy pull request.
    pull_requests::refresh_open_pull_request(pool, &repository, ci, user.id, pull_request.id)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "git_unavailable",
            message: "Pull request revisions could not be refreshed; merge denied.".into(),
        })?;
    let pull_request = db::pull_requests::records::get_pull_request_by_id(pool, pull_request.id)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;

    if repository.is_archived {
        return Err(archived_refusal());
    }
    let fork_head = pull_request.head_repo_id != Some(repository.id);
    if validate_merge_revision(&pull_request.head_branch, &request.expected_head).is_err()
        || validate_merge_revision(&pull_request.base_branch, &request.expected_base).is_err()
        || request.expected_head != pull_request.head_sha
        || request.expected_base != pull_request.base_sha
        || request
            .expected_base_branch
            .as_deref()
            .is_some_and(|branch| branch != pull_request.base_branch)
        || (!fork_head && pull_request.head_branch == pull_request.base_branch)
    {
        return Err(stale_revision());
    }
    if pull_request.state != "open" {
        return Err(OperationError::Invalid {
            code: "invalid_state",
            message: "Pull request is not open".to_owned(),
        });
    }
    if !allowed_merge_methods(&repository).contains(&request.merge_method) {
        return Err(method_not_allowed(request.merge_method));
    }
    let chain = stack_chain(pool, &pull_request, request.merge_method).await?;
    let layers = refresh_chain(pool, ci, &repository, user.id, pull_request.id, chain).await?;
    // The stack may have changed since the client read the destination, for example by unstacking
    // in another session. Then the merge would land somewhere else, so it is refused. The check
    // runs on the refreshed layers because refreshing adopts the base branch of each one.
    if request.expected_target.as_deref().is_some_and(|target| {
        layers
            .first()
            .is_some_and(|bottom| bottom.base_branch != target)
    }) {
        return Err(stale_revision());
    }
    // A branch that was deleted keeps the commit recorded for it. That commit alone
    // is not something to merge, so both branches have to exist.
    let branches = repo_store(&repository, ci.cluster.as_ref())
        .refs()
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "git_unavailable",
            message: "Repository storage is unavailable; merge denied.".to_owned(),
        })?;
    for layer in &layers {
        if layer.is_draft {
            return Err(draft_refusal(layer.number));
        }
        if !head_branch_exists(pool, ci, &repository, &branches, layer).await? {
            return Err(OperationError::Conflict {
                code: "head_branch_missing",
                message: format!(
                    "The head branch of #{} no longer exists. Restore it to merge this pull request.",
                    layer.number
                ),
            });
        }
    }
    let trunk = &layers[0];
    if !branches.contains_key(&format!("refs/heads/{}", trunk.base_branch)) {
        return Err(base_branch_missing());
    }
    check_restacked(pool, ci, &repository, &layers, &branches).await?;
    check_chain_policy(pool, ci, &repository, &user, &layers, request.merge_method).await?;

    // A squash commit is authored by the pull request's author under the no-reply address, so the
    // account's own email is not published. When that account is gone, the merger is the author.
    let author = if request.merge_method == MergeMethod::Squash {
        db::users::accounts::get_user_by_id(pool, pull_request.author_id)
            .await
            .map_err(|_| OperationError::Unavailable {
                code: "database_error",
                message: "Collaboration storage is temporarily unavailable".to_owned(),
            })?
    } else {
        None
    };
    let message = match request.merge_method {
        MergeMethod::Merge => format!(
            "Merge pull request #{} from {}\n\n{}",
            pull_request.number, pull_request.head_branch, pull_request.title
        ),
        MergeMethod::Squash => format!("{} (#{})", pull_request.title, pull_request.number),
        // A rebase keeps the message of each commit it replays.
        MergeMethod::Rebase => String::new(),
    };
    let merge = StoreMerge {
        head_branch: head_ref(&repository, &pull_request),
        base_branch: trunk.base_branch.clone(),
        head: pull_request.head_sha.clone(),
        base: trunk.base_sha.clone(),
        message,
        name: user.username.clone(),
        email: user.commit_email(),
        method: request.merge_method,
        author_name: author.as_ref().map(|author| author.username.clone()),
        author_email: author.as_ref().map(|author| author.noreply_email()),
    };
    let merge_sha = repo_store(&repository, ci.cluster.as_ref())
        .prepare_merge(&merge)
        .await
        .map_err(prepare_error)?;

    Ok(PreparedMerge {
        actor_id,
        owner,
        repo,
        repository,
        pull_request,
        layers,
        merge,
        merge_sha,
    })
}

/// Maps a failed publication of a merge onto the refusal to show.
fn publish_failure(error: RepoError, method: MergeMethod) -> OperationError {
    if error.is_write_denied() {
        OperationError::Denied {
            code: "write_denied",
            message: "Repository write authority changed; merge denied.".to_owned(),
        }
    } else if error.is_merge_commit_blocked() {
        OperationError::Denied {
            code: "branch_protection",
            message: error.message,
        }
    } else if error.is_merge_method_not_allowed() {
        method_not_allowed(method)
    } else if error.is_storage_node_unavailable() {
        OperationError::Unavailable {
            code: "storage_unavailable",
            message: "storage node unavailable".to_owned(),
        }
    } else {
        OperationError::Conflict {
            code: "revision_conflict",
            message: "A branch changed while merging. Reload the pull request and try again."
                .to_owned(),
        }
    }
}

pub async fn publish_prepared_merge(
    pool: &db::DbPool,
    ci: &CiRuns,
    broadcaster: &NotificationBroadcaster,
    prepared: PreparedMerge,
) -> Result<MergeResponse, OperationError> {
    let PreparedMerge {
        actor_id,
        owner,
        repo,
        repository,
        pull_request,
        layers,
        merge,
        merge_sha,
    } = prepared;
    let repository = db::repositories::records::get_repository_by_id(pool, repository.id)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;
    // The settings may have changed since the merge was prepared, and the newer ones decide.
    if !allowed_merge_methods(&repository).contains(&merge.method) {
        return Err(method_not_allowed(merge.method));
    }
    let user = super::actor(pool, actor_id).await?;
    super::require_write(pool, &user, &repository).await?;
    let current = db::pull_requests::records::get_pull_request_by_id(pool, pull_request.id)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Conflict {
            code: "revision_conflict",
            message: "The pull request changed while merging. Reload and try again.".to_owned(),
        })?;
    // The stack is read again, so a layer that joined or left it since the merge was prepared
    // refuses the publish, as does any layer whose revision moved.
    let chain = stack_chain(pool, &current, merge.method).await?;
    let unchanged = chain.len() == layers.len()
        && chain
            .iter()
            .zip(&layers)
            .all(|(now, prepared)| same_revision(now, prepared));
    if !unchanged {
        return Err(OperationError::Conflict {
            code: "revision_conflict",
            message: "The pull request changed while merging. Reload and try again.".to_owned(),
        });
    }
    check_chain_policy(pool, ci, &repository, &user, &chain, merge.method).await?;
    // A layer that joins between this check and the ref update is retargeted after the merge, as
    // with any other check made here.
    let published = if let [_] = chain.as_slice() {
        repo_store(&repository, ci.cluster.as_ref())
            .publish_merge_as(
                pool,
                repository.id,
                user.id,
                &merge.head_branch,
                &merge.base_branch,
                &merge.head,
                &merge.base,
                &merge_sha,
                merge.method,
            )
            .await
    } else {
        let heads: Vec<(String, String)> = chain
            .iter()
            .map(|layer| (head_ref(&repository, layer), layer.head_sha.clone()))
            .collect();
        repo_store(&repository, ci.cluster.as_ref())
            .publish_stacked_merge_as(
                pool,
                repository.id,
                user.id,
                &heads,
                &merge.base_branch,
                &merge.base,
                &merge_sha,
            )
            .await
    };
    published.map_err(|error| publish_failure(error, merge.method))?;
    let marked =
        db::pull_requests::records::mark_pull_requests_merged(pool, &chain, user.id, &merge_sha)
            .await;
    if !matches!(marked, Ok(true)) {
        tracing::error!(pr = current.id, %merge_sha, "branch merged but the pull requests were not marked");
        return Err(OperationError::BranchMerged { sha: merge_sha });
    }

    // Open pull requests based on the head of any merged layer now follow the trunk, not only those
    // based on the chosen layer. The rows are read again, since a join may have changed a stack
    // after `chain` was loaded. The other open pull requests are refreshed below, which also runs
    // push workflows for the merge commit.
    let mut moved = Vec::new();
    for layer in &chain {
        match db::pull_requests::records::get_pull_request_by_id(pool, layer.id).await {
            Ok(Some(merged_row)) => moved.extend(
                pull_requests::stacks::retarget_after_merge(
                    pool,
                    user.id,
                    &merged_row,
                    &merge.base_branch,
                    &merge_sha,
                )
                .await,
            ),
            _ => tracing::warn!(
                pr = layer.id,
                "could not reload the merged pull request to retarget its stack"
            ),
        }
    }
    // A moved layer may have workflows that run on its new base. They are queued after the
    // retarget's own transaction, as a head move is, and the wait for them is bounded.
    pull_requests::workflows::queue_moved_pull_requests_ci(pool, ci, &repository, &moved, user.id)
        .await;
    pull_requests::branches_moved(pool, &repository, ci, user.id).await;
    ci.queue_push_runs(
        &repository,
        &owner,
        user.id,
        RefUpdate {
            reference: format!("refs/heads/{}", merge.base_branch),
            old: Some(merge.base.clone()),
            new: merge_sha.clone(),
            changed_paths: None,
        },
    )
    .await;

    for layer in &chain {
        if layer.author_id == user.id {
            continue;
        }
        let title = format!("{} merged pull request #{}", user.username, layer.number);
        let link = format!("/{}/{}/pulls/{}", owner, repo, layer.number);
        let _ = create_and_broadcast_notification(
            pool,
            broadcaster,
            NotificationRequest {
                user_id: layer.author_id,
                actor_id: Some(user.id),
                repo_id: Some(repository.id),
                kind: "pull_request_merged",
                title: &title,
                body: Some(&layer.title),
                link: Some(&link),
                metadata: json!({
                    "repo_owner": &owner,
                    "repo_name": &repo,
                    "pull_number": layer.number,
                    "merge_sha": merge_sha.as_str(),
                }),
            },
        )
        .await;
    }

    Ok(MergeResponse {
        sha: merge_sha,
        merged: true,
        message: format!("Pull request #{} merged successfully", current.number),
    })
}

pub async fn merge_pull_request(
    pool: &db::DbPool,
    ci: &CiRuns,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    repository: (String, String, i32),
    request: MergeRequest,
) -> Result<MergeResponse, OperationError> {
    let prepared = prepare_pull_request_merge(pool, ci, actor_id, repository, request).await?;
    publish_prepared_merge(pool, ci, broadcaster, prepared).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use db::pull_requests::reviews::PullRequestReviewDecision;

    // The database returns each reviewer's latest decision, so these inputs hold at
    // most one entry per reviewer.
    fn decision(author_id: i64, sha: Option<&str>, state: &str) -> PullRequestReviewDecision {
        PullRequestReviewDecision {
            author_id,
            state: state.into(),
            commit_sha: sha.map(str::to_owned),
            username: None,
        }
    }
    #[test]
    fn approvals_follow_the_stale_review_setting_and_change_requests_always_block() {
        // (state, commit of the decision, dismiss_stale_reviews) -> (approvals, blocked)
        let cases = [
            ("approved", "H1", true, (1, false)),
            ("approved", "H1", false, (1, false)),
            ("approved", "H0", true, (0, false)),
            ("approved", "H0", false, (1, false)),
            ("changes_requested", "H1", true, (0, true)),
            ("changes_requested", "H1", false, (0, true)),
            ("changes_requested", "H0", true, (0, true)),
            ("changes_requested", "H0", false, (0, true)),
            // The database hands over a dismissal as the latest decision; it counts for nothing.
            ("dismissed", "H1", true, (0, false)),
            ("dismissed", "H1", false, (0, false)),
            ("dismissed", "H0", true, (0, false)),
            ("dismissed", "H0", false, (0, false)),
        ];
        for (state, sha, dismiss, expected) in cases {
            assert_eq!(
                count_effective_approvals(&[decision(1, Some(sha), state)], "H1", dismiss),
                expected,
                "{state} on {sha}, dismiss_stale_reviews={dismiss}"
            );
        }
    }

    #[test]
    fn a_change_request_blocks_next_to_approvals_and_unknown_commits_follow_the_setting() {
        for dismiss in [false, true] {
            assert_eq!(
                count_effective_approvals(
                    &[
                        decision(1, Some("H1"), "approved"),
                        decision(2, Some("H1"), "changes_requested")
                    ],
                    "H1",
                    dismiss
                ),
                (1, true)
            );
        }
        assert_eq!(
            count_effective_approvals(
                &[
                    decision(1, Some("H1"), "approved"),
                    decision(2, Some("H2"), "approved")
                ],
                "H1",
                true
            ),
            (1, false)
        );
        assert_eq!(
            count_effective_approvals(
                &[
                    decision(1, Some("H1"), "approved"),
                    decision(2, Some("H2"), "approved")
                ],
                "H1",
                false
            ),
            (2, false)
        );
        // A decision recorded without a commit never matches a head when staleness counts.
        assert_eq!(
            count_effective_approvals(&[decision(1, None, "approved")], "H1", true),
            (0, false)
        );
        assert_eq!(
            count_effective_approvals(&[decision(1, None, "approved")], "H1", false),
            (1, false)
        );
    }

    #[test]
    fn statuses_for_another_revision_never_satisfy_the_gate() {
        let status = db::models::CommitStatus {
            id: 1,
            repo_id: 1,
            sha: "H1".into(),
            context: "ci".into(),
            state: "success".into(),
            description: None,
            target_url: None,
            creator_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let required = vec!["ci".to_string()];
        assert!(required_status_check_failures(
            "H1",
            &required,
            std::slice::from_ref(&status),
            &[]
        )
        .is_empty());
        assert_eq!(
            required_status_check_failures("H2", &required, &[status], &[]),
            vec!["ci (missing)"]
        );
        assert!(serde_json::from_value::<MergeRequest>(json!({})).is_err());
    }
}
