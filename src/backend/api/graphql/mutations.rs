//! Mutation adaptation only: permissions and side effects stay in the REST domain operations.
use super::{
    bindings::boxed, db, issue, missing, node_id, repository, unavailable, user, Arguments,
    RequestContext, Result, Value,
};
use crate::backend::{
    identity::following,
    issues,
    repositories::{engagement, pull_requests, store::MergeMethod},
};
use async_graphql::{Error, ErrorExtensions};
use serde_json::json;

pub(super) fn text<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input[key]
        .as_str()
        .ok_or_else(|| Error::new(format!("Missing {key}")))
}
fn optional_text(input: &Value, key: &str) -> Option<String> {
    input[key].as_str().map(str::to_owned)
}
pub(super) fn target(input: &Value, key: &str, expected: &str) -> Result<i64> {
    let (kind, id) = node_id::decode(text(input, key)?).ok_or_else(missing)?;
    if kind != expected {
        return Err(missing());
    }
    Ok(id)
}
/// The merge method a merge or auto-merge names, refused when the client read an older head.
fn merge_method(input: &Value, current: &db::models::PullRequest) -> Result<MergeMethod> {
    if !input["expectedHeadOid"].is_null()
        && !text(input, "expectedHeadOid")?.eq_ignore_ascii_case(&current.head_sha)
    {
        return Err(Error::new(
            "Head branch was modified. Review and try the merge again.",
        ));
    }
    let method = input["mergeMethod"]
        .as_str()
        .unwrap_or("MERGE")
        .to_ascii_lowercase();
    MergeMethod::parse(&method).ok_or_else(|| Error::new("Invalid merge method"))
}
pub(super) fn pull_error(error: pull_requests::OperationError) -> Error {
    let kind = match &error {
        pull_requests::OperationError::Missing { .. } => "NOT_FOUND",
        pull_requests::OperationError::Denied { .. }
        | pull_requests::OperationError::Unauthenticated { .. } => "FORBIDDEN",
        pull_requests::OperationError::Unavailable { .. } => "INTERNAL",
        _ => "UNPROCESSABLE",
    };
    Error::new(error.to_string()).extend_with(|_, e| e.set("type", kind))
}
pub(super) fn issue_error(error: issues::OperationError) -> Error {
    let kind = match &error {
        issues::OperationError::Missing { .. } => "NOT_FOUND",
        issues::OperationError::Denied { .. } | issues::OperationError::Unauthenticated { .. } => {
            "FORBIDDEN"
        }
        issues::OperationError::Unavailable { .. } => "INTERNAL",
        _ => "UNPROCESSABLE",
    };
    Error::new(error.to_string()).extend_with(|_, e| e.set("type", kind))
}

pub(super) async fn resolve(
    field: &str,
    args: &Arguments,
    request: &RequestContext,
) -> Result<Value> {
    if field.contains("Discussion") {
        return boxed(|| super::discussions::mutation(field, args, request)).await;
    }
    if matches!(field, "addLabelsToLabelable" | "removeLabelsFromLabelable") {
        let input = args.args.try_get("input")?.object()?;
        if matches!(
            node_id::decode(input.try_get("labelableId")?.string()?),
            Some(("D", _))
        ) {
            return boxed(|| super::discussions::mutation(field, args, request)).await;
        }
    }
    if matches!(
        field,
        "createIssue"
            | "addComment"
            | "closeIssue"
            | "reopenIssue"
            | "pinIssue"
            | "unpinIssue"
            | "deleteIssue"
            | "transferIssue"
            | "createLinkedBranch"
            | "updateIssueIssueType"
            | "addSubIssue"
            | "removeSubIssue"
            | "addBlockedBy"
            | "removeBlockedBy"
    ) {
        return boxed(|| issue::mutation(field, args, request)).await;
    }
    request
        .auth
        .require_write()
        .map_err(|_| Error::new("Resource not accessible by token"))?;
    let input: Value = args.args.try_get("input")?.deserialize()?;
    if super::repository_mutations::FIELDS.contains(&field) {
        return boxed(|| super::repository_mutations::resolve(field, &input, request)).await;
    }
    if matches!(field, "addReaction" | "removeReaction") {
        return boxed(|| super::reaction_mutations::resolve(field, &input, request)).await;
    }
    if matches!(field, "updateIssueComment" | "deleteIssueComment") {
        return boxed(|| super::comment_mutations::resolve(field, &input, request)).await;
    }
    if super::review_mutations::FIELDS.contains(&field) {
        return boxed(|| super::review_mutations::resolve(field, &input, request)).await;
    }
    if super::collaboration_mutations::FIELDS.contains(&field) {
        return boxed(|| super::collaboration_mutations::resolve(field, &input, request)).await;
    }
    let allowed: &[&str] = match field {
        "updateIssue" => &[
            "id",
            "title",
            "body",
            "state",
            "stateReason",
            "labelIds",
            "assigneeIds",
            "milestoneId",
            "clientMutationId",
        ],
        "createPullRequest" => &[
            "repositoryId",
            "headRepositoryId",
            "title",
            "body",
            "headRefName",
            "baseRefName",
            "draft",
            "maintainerCanModify",
            "clientMutationId",
        ],
        "updatePullRequest" => &[
            "pullRequestId",
            "title",
            "body",
            "state",
            "baseRefName",
            "maintainerCanModify",
            "labelIds",
            "assigneeIds",
            "milestoneId",
            "clientMutationId",
        ],
        "archiveRepository" | "unarchiveRepository" => &["repositoryId", "clientMutationId"],
        "updatePullRequestBranch" => &[
            "pullRequestId",
            "expectedHeadOid",
            "updateMethod",
            "clientMutationId",
        ],
        "closePullRequest"
        | "reopenPullRequest"
        | "markPullRequestReadyForReview"
        | "convertPullRequestToDraft" => &["pullRequestId", "clientMutationId"],
        "revertPullRequest" => &[
            "pullRequestId",
            "title",
            "body",
            "draft",
            "clientMutationId",
        ],
        "mergePullRequest" => &[
            "pullRequestId",
            "expectedHeadOid",
            "mergeMethod",
            "commitHeadline",
            "commitBody",
            "authorEmail",
            "clientMutationId",
        ],
        "enablePullRequestAutoMerge" => &[
            "pullRequestId",
            "expectedHeadOid",
            "mergeMethod",
            "commitHeadline",
            "commitBody",
            "authorEmail",
            "clientMutationId",
        ],
        "disablePullRequestAutoMerge" => &["pullRequestId", "clientMutationId"],
        "addStar" | "removeStar" => &["starrableId", "clientMutationId"],
        "updateSubscription" => &["subscribableId", "state", "clientMutationId"],
        "followUser" | "unfollowUser" => &["userId", "clientMutationId"],
        _ => {
            return Err(Error::new(format!(
                "Codeyard does not support Mutation.{field} yet"
            )))
        }
    };
    if let Some(name) = input
        .as_object()
        .into_iter()
        .flat_map(|object| object.keys())
        .find(|name| !allowed.contains(&name.as_str()) && !input[*name].is_null())
    {
        return Err(Error::new(format!(
            "Codeyard does not support {field}.{name} yet"
        )));
    }
    if request.bounded()
        && matches!(
            field,
            "addStar" | "removeStar" | "updateSubscription" | "followUser" | "unfollowUser"
        )
    {
        return Err(super::token_denied());
    }
    let actor = request.auth.user.id;
    let pool = &request.state.pool;
    let broadcaster = &request.state.notification_broadcaster;
    let mut payload = json!({"clientMutationId":input["clientMutationId"],"actor":user::actor_object(&request.auth.user,actor)});
    match field {
        "archiveRepository" | "unarchiveRepository" => {
            boxed(|| async move {
                let repo = repository::by_id(target(&input, "repositoryId", "R")?, request).await?;
                request.authorize(repo.id, "administration", true).await?;
                db::repositories::records::set_repository_archived(
                    pool,
                    actor,
                    repo.id,
                    field == "archiveRepository",
                )
                .await
                .map_err(|error| super::super::github::repos::settings_error(error).graphql())?;
                payload["repository"] =
                    repository::object(&repository::by_id(repo.id, request).await?);
                Ok(payload)
            })
            .await
        }
        "updateIssue" => {
            boxed(|| async move {
                let id = target(&input, "id", "I")?;
                let current = db::issues::get_issue_by_id(pool, id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?;
                let repo = repository::by_id(current.repo_id, request).await?;
                request.authorize(repo.id, "issues", true).await?;
                let metadata = super::metadata_mutations::prepare(&input, &repo, request).await?;
                // A metadata-only edit needs triage, not the write access a content edit needs.
                let metadata_only = ["title", "body", "state", "stateReason"]
                    .iter()
                    .all(|key| input.get(*key).is_none_or(Value::is_null));
                let saved = if metadata_only {
                    current
                } else {
                    issues::mutations::update_issue_latest(
                        pool,
                        broadcaster,
                        actor,
                        repo.id,
                        current.number,
                        issues::mutations::UpdateIssueRequest {
                            expected_state: current.state,
                            expected_updated: current.updated_at.to_rfc3339(),
                            title: optional_text(&input, "title"),
                            body: optional_text(&input, "body"),
                            state: optional_text(&input, "state").map(|s| s.to_ascii_lowercase()),
                            state_reason: optional_text(&input, "stateReason")
                                .map(|s| s.to_ascii_lowercase()),
                        },
                    )
                    .await
                    .map_err(issue_error)?
                };
                super::metadata_mutations::apply(
                    &repo,
                    db::assignees::AssignedItem::Issue(saved.id),
                    saved.number,
                    &saved.title,
                    metadata,
                    request,
                )
                .await?;
                let saved = db::issues::get_issue_by_id(pool, saved.id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?;
                payload["issue"] = issue::object(&saved);
                Ok(payload)
            })
            .await
        }
        "createPullRequest" => {
            boxed(|| async move {
                let repo = repository::by_id(target(&input, "repositoryId", "R")?, request).await?;
                request.authorize(repo.id, "pull-requests", true).await?;
                crate::backend::identity::interactions::prepare(
                    &request.state,
                    &request.auth.user,
                    &repo,
                )
                .await
                .map_err(unavailable)?;
                // "owner:branch" names the branch of that owner's fork of this repository, as REST does.
                let head_ref = text(&input, "headRefName")?;
                let (head_owner, head_branch) = match head_ref.split_once(':') {
                    Some((owner, branch)) => (Some(owner), branch),
                    None => (None, head_ref),
                };
                let head_repo_id = if !input["headRepositoryId"].is_null() {
                    let id = target(&input, "headRepositoryId", "R")?;
                    request.authorize(id, "metadata", false).await?;
                    Some(id)
                } else if let Some(owner) = head_owner {
                    let head = db::repositories::records::get_repository(pool, owner, &repo.name)
                        .await
                        .map_err(unavailable)?
                        .ok_or_else(missing)?;
                    if !crate::backend::repositories::access::can_read(
                        pool,
                        Some(&request.auth.user),
                        &head,
                    )
                    .await
                    .map_err(unavailable)?
                    {
                        return Err(missing());
                    }
                    Some(head.id)
                } else {
                    None
                };
                let head = repository::by_id(head_repo_id.unwrap_or(repo.id), request).await?;
                let saved = pull_requests::mutations::create_pull_request(
                    pool,
                    &request.state.ci_runs,
                    broadcaster,
                    actor,
                    repository::owner(&repo, request).await?,
                    repo.name,
                    pull_requests::mutations::CreatePullRequestRequest {
                        head_repo_id,
                        source: pull_requests::mutations::PullRequestSource::New {
                            title: text(&input, "title")?.into(),
                            body: optional_text(&input, "body"),
                        },
                        head_branch: head_branch.into(),
                        base_branch: text(&input, "baseRefName")?.into(),
                        is_draft: input["draft"].as_bool().unwrap_or(false),
                        maintainer_can_modify: Some(
                            input["maintainerCanModify"].as_bool().unwrap_or_else(|| {
                                db::pull_requests::records::can_grant_maintainer_edit(
                                    actor, repo.id, &head,
                                )
                            }),
                        ),
                    },
                )
                .await
                .map_err(pull_error)?;
                payload["pullRequest"] = issue::pull_object(&saved);
                Ok(payload)
            })
            .await
        }
        "revertPullRequest" => {
            boxed(|| async move {
                let current = db::pull_requests::records::get_pull_request_by_id(
                    pool,
                    target(&input, "pullRequestId", "PR")?,
                )
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
                let repo = repository::by_id(current.repo_id, request).await?;
                request.authorize(repo.id, "pull-requests", true).await?;
                request.authorize(repo.id, "contents", true).await?;
                request.require_oauth_scope("workflow")?;
                let saved = pull_requests::revert::revert_pull_request(
                    pool,
                    &request.state.ci_runs,
                    broadcaster,
                    actor,
                    (
                        repository::owner(&repo, request).await?,
                        repo.name,
                        current.number,
                    ),
                    pull_requests::revert::RevertRequest {
                        title: optional_text(&input, "title"),
                        body: optional_text(&input, "body"),
                        is_draft: input["draft"].as_bool().unwrap_or(false),
                    },
                )
                .await
                .map_err(pull_error)?;
                payload["pullRequest"] = issue::pull_object(&current);
                payload["revertPullRequest"] = issue::pull_object(&saved);
                Ok(payload)
            })
            .await
        }
        "updatePullRequest"
        | "closePullRequest"
        | "reopenPullRequest"
        | "markPullRequestReadyForReview"
        | "convertPullRequestToDraft"
        | "updatePullRequestBranch"
        | "mergePullRequest" => {
            boxed(|| async move {
                let current = db::pull_requests::records::get_pull_request_by_id(
                    pool,
                    target(&input, "pullRequestId", "PR")?,
                )
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
                let repo = repository::by_id(current.repo_id, request).await?;
                request.authorize(repo.id, "pull-requests", true).await?;
                let owner = repository::owner(&repo, request).await?;
                let metadata = super::metadata_mutations::prepare(&input, &repo, request).await?;
                // Metadata has its own live native authority; do not run a structural PR edit
                // for an empty edit (notably a fork author's grant revocation).
                let metadata_only = field == "updatePullRequest"
                    && [
                        "title",
                        "body",
                        "state",
                        "baseRefName",
                        "maintainerCanModify",
                    ]
                    .iter()
                    .all(|key| input.get(*key).is_none_or(Value::is_null));
                let saved = if metadata_only {
                    current
                } else if field == "updatePullRequestBranch" {
                    if input["updateMethod"]
                        .as_str()
                        .is_some_and(|method| method != "MERGE")
                    {
                        return Err(Error::new(
                            "Codeyard can update a pull request branch by merge, not rebase",
                        ));
                    }
                    request
                        .authorize(current.head_repo_id.ok_or_else(missing)?, "contents", true)
                        .await?;
                    let expected = input
                        .get("expectedHeadOid")
                        .filter(|v| !v.is_null())
                        .map(|v| {
                            v.as_str().ok_or_else(|| {
                                Error::new("expectedHeadOid must be a Git object ID")
                            })
                        })
                        .transpose()?;
                    pull_requests::update_branch::update_branch(
                        pool,
                        &request.state.ci_runs,
                        actor,
                        (&owner, &repo.name, current.number),
                        expected,
                        Some(request.auth.token.id),
                    )
                    .await
                    .map_err(pull_error)?;
                    db::pull_requests::records::get_pull_request_by_id(pool, current.id)
                        .await
                        .map_err(unavailable)?
                        .ok_or_else(missing)?
                } else if matches!(
                    field,
                    "markPullRequestReadyForReview" | "convertPullRequestToDraft"
                ) {
                    pull_requests::mutations::set_pull_request_draft(
                        pool,
                        &request.state.ci_runs,
                        actor,
                        (owner, repo.name, current.number),
                        Some(&current.updated_at.to_rfc3339()),
                        field == "convertPullRequestToDraft",
                    )
                    .await
                    .map_err(pull_error)?
                } else if field == "mergePullRequest" {
                    request.authorize(repo.id, "contents", true).await?;
                    let method = merge_method(&input, &current)?;
                    pull_requests::merge::merge_pull_request(
                        pool,
                        &request.state.ci_runs,
                        broadcaster,
                        actor,
                        (owner, repo.name, current.number),
                        pull_requests::merge::MergeRequest {
                            expected_head: current.head_sha,
                            expected_base: current.base_sha,
                            expected_base_branch: Some(current.base_branch),
                            expected_target: None,
                            merge_method: method,
                            commit_title: optional_text(&input, "commitHeadline"),
                            commit_message: optional_text(&input, "commitBody"),
                            author_email: optional_text(&input, "authorEmail"),
                            token: Some(request.auth.token.id),
                        },
                    )
                    .await
                    .map_err(pull_error)?;
                    db::pull_requests::records::get_pull_request_by_id(pool, current.id)
                        .await
                        .map_err(unavailable)?
                        .ok_or_else(missing)?
                } else {
                    let state = match field {
                        "closePullRequest" => Some("closed".into()),
                        "reopenPullRequest" => Some("open".into()),
                        _ => optional_text(&input, "state").map(|s| s.to_ascii_lowercase()),
                    };
                    pull_requests::mutations::update_pull_request_latest(
                        pool,
                        &request.state.ci_runs,
                        broadcaster,
                        actor,
                        (repo.id, current.number),
                        pull_requests::mutations::UpdatePullRequestRequest {
                            expected_state: current.state,
                            expected_updated: current.updated_at.to_rfc3339(),
                            title: optional_text(&input, "title"),
                            body: optional_text(&input, "body"),
                            state,
                            base: optional_text(&input, "baseRefName"),
                            maintainer_can_modify: input["maintainerCanModify"].as_bool(),
                        },
                    )
                    .await
                    .map_err(pull_error)?
                };
                let repo = repository::by_id(saved.repo_id, request).await?;
                super::metadata_mutations::apply(
                    &repo,
                    db::assignees::AssignedItem::PullRequest(saved.id),
                    saved.number,
                    &saved.title,
                    metadata,
                    request,
                )
                .await?;
                let saved = db::pull_requests::records::get_pull_request_by_id(pool, saved.id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?;
                payload["pullRequest"] = issue::pull_object(&saved);
                Ok(payload)
            })
            .await
        }
        "enablePullRequestAutoMerge" | "disablePullRequestAutoMerge" => {
            boxed(|| async move {
                let current = db::pull_requests::records::get_pull_request_by_id(
                    pool,
                    target(&input, "pullRequestId", "PR")?,
                )
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
                let repo = repository::by_id(current.repo_id, request).await?;
                request.authorize(repo.id, "pull-requests", true).await?;
                if field == "enablePullRequestAutoMerge" {
                    request.require_oauth_scope("workflow")?;
                    request.authorize(repo.id, "contents", true).await?;
                    let options = pull_requests::auto_merge::EnableRequest {
                        merge_method: merge_method(&input, &current)?,
                        commit_title: optional_text(&input, "commitHeadline"),
                        commit_message: optional_text(&input, "commitBody"),
                        author_email: optional_text(&input, "authorEmail"),
                    };
                    pull_requests::auto_merge::enable(pool, actor, &current, options)
                        .await
                        .map_err(pull_error)?;
                } else {
                    pull_requests::auto_merge::disable(pool, actor, &current)
                        .await
                        .map_err(pull_error)?;
                }
                payload["pullRequest"] = issue::pull_object(&current);
                Ok(payload)
            })
            .await
        }
        "addStar" | "removeStar" | "updateSubscription" => {
            boxed(|| async move {
                let key = if field == "updateSubscription" {
                    "subscribableId"
                } else {
                    "starrableId"
                };
                let id = target(&input, key, "R")?;
                repository::by_id(id, request).await?;
                if field == "updateSubscription" {
                    let level = match text(&input, "state")? {
                        "SUBSCRIBED" => "watching",
                        "IGNORED" => "ignoring",
                        "UNSUBSCRIBED" => "participating",
                        _ => return Err(Error::new("Invalid subscription state")),
                    };
                    engagement::set_level(pool, actor, id, level, &[])
                        .await
                        .map_err(|error| match error {
                            engagement::EngagementError::Access(_) => missing(),
                            _ => unavailable(()),
                        })?;
                } else {
                    engagement::change(
                        pool,
                        broadcaster,
                        actor,
                        id,
                        engagement::Engagement::Star,
                        field == "addStar",
                    )
                    .await
                    .map_err(|error| match error {
                        engagement::EngagementError::Access(_) => missing(),
                        _ => unavailable(()),
                    })?;
                }
                payload[if field == "updateSubscription" {
                    "subscribable"
                } else {
                    "starrable"
                }] = repository::object(&repository::by_id(id, request).await?);
                Ok(payload)
            })
            .await
        }
        "followUser" | "unfollowUser" => {
            boxed(|| async move {
                // REST has no OAuth route for account follow writes, so OAuth tokens may not make them.
                if request.is_oauth() {
                    return Err(super::token_denied());
                }
                let id = target(&input, "userId", "U")?;
                let current = db::users::accounts::get_user_by_id(pool, id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?;
                following::change(
                    pool,
                    broadcaster,
                    actor,
                    &current.username,
                    field == "followUser",
                )
                .await
                .map_err(|error| match error {
                    following::FollowError::Missing => missing(),
                    following::FollowError::Unavailable
                    | following::FollowError::CountsUnavailableAfterSave => unavailable(()),
                    error => Error::new(error.to_string()),
                })?;
                payload["user"] = user::object(&current, actor);
                Ok(payload)
            })
            .await
        }
        _ => unreachable!("mutation admission enumerates the supported operations"),
    }
}
