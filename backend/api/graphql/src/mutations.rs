//! Mutation adaptation only: permissions and side effects stay in the REST domain operations.
use crate::arguments::Arguments;
use crate::bindings::boxed;
use crate::missing;
use crate::unavailable;
use crate::RequestContext;
use crate::Result;
use async_graphql::Error;
use async_graphql::ErrorExtensions;
use codeyard_model::node_id;
use codeyard_repos_core::store_types::MergeMethod;
use serde_json::json;
use serde_json::Value;

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
fn merge_method(
    input: &Value,
    current: &codeyard_db_core::models::PullRequest,
) -> Result<MergeMethod> {
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
pub(super) fn pull_error(
    error: codeyard_repos_core::repositories::pull_requests::OperationError,
) -> Error {
    let kind = match &error {
        codeyard_repos_core::repositories::pull_requests::OperationError::Missing { .. } => {
            "NOT_FOUND"
        }
        codeyard_repos_core::repositories::pull_requests::OperationError::Denied { .. }
        | codeyard_repos_core::repositories::pull_requests::OperationError::Unauthenticated {
            ..
        } => "FORBIDDEN",
        codeyard_repos_core::repositories::pull_requests::OperationError::Unavailable {
            ..
        } => "INTERNAL",
        _ => "UNPROCESSABLE",
    };
    Error::new(error.to_string()).extend_with(|_, e| e.set("type", kind))
}
pub(super) fn issue_error(error: codeyard_repos_pulls::issues::OperationError) -> Error {
    let kind = match &error {
        codeyard_repos_pulls::issues::OperationError::Missing { .. } => "NOT_FOUND",
        codeyard_repos_pulls::issues::OperationError::Denied { .. }
        | codeyard_repos_pulls::issues::OperationError::Unauthenticated { .. } => "FORBIDDEN",
        codeyard_repos_pulls::issues::OperationError::Unavailable { .. } => "INTERNAL",
        _ => "UNPROCESSABLE",
    };
    Error::new(error.to_string()).extend_with(|_, e| e.set("type", kind))
}
/// The viewer can read the repository, so a refused change to someone else's issue is
/// GitHub's FORBIDDEN, not a missing repository.
pub(super) fn issue_change_error(
    error: codeyard_repos_pulls::issues::OperationError,
    author_id: i64,
    field: &str,
    request: &RequestContext,
) -> Error {
    match error {
        codeyard_repos_pulls::issues::OperationError::Missing { .. }
            if author_id != request.auth.user.id =>
        {
            crate::forbidden(field, request)
        }
        error => issue_error(error),
    }
}

/// Every mutation validates explicit root hints before any domain operation runs.
/// ID fields retain their schema-coerced values; free text is never interpreted as an ID.
async fn validate_input_roots(args: &Arguments, request: &RequestContext) -> Result<()> {
    let Some(input) = args.args.get("input") else {
        return Ok(());
    };
    let input: Value = input.deserialize()?;
    let mut pending = vec![("input", &input, 0usize)];
    let mut visited = 0usize;
    let mut roots = 0usize;
    while let Some((key, value, depth)) = pending.pop() {
        visited += 1;
        if visited > 10000 || depth > 64 {
            return Err(Error::new("Mutation input exceeds the request limit"));
        }
        match value {
            Value::Object(fields) => {
                if fields.len() > 10000usize.saturating_sub(visited + pending.len()) {
                    return Err(Error::new("Mutation input exceeds the request limit"));
                }
                for (key, value) in fields {
                    pending.push((key.as_str(), value, depth + 1));
                }
            }
            Value::Array(values) => {
                if values.len() > 10000usize.saturating_sub(visited + pending.len()) {
                    return Err(Error::new("Mutation input exceeds the request limit"));
                }
                for value in values {
                    pending.push((key, value, depth + 1));
                }
            }
            Value::String(id)
                if key != "clientMutationId"
                    && (key == "id" || key.ends_with("Id") || key.ends_with("Ids"))
                    && node_id::scoped_root(id).is_some() =>
            {
                roots += 1;
                if roots > 1000 {
                    return Err(Error::new("Mutation input exceeds the request limit"));
                }
                crate::validate_node_root(id, request).await?;
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) async fn resolve(
    field: &str,
    args: &Arguments,
    request: &RequestContext,
) -> Result<Value> {
    validate_input_roots(args, request).await?;
    if field.contains("Discussion") {
        return boxed(|| crate::discussions::mutation(field, args, request)).await;
    }
    if matches!(field, "addLabelsToLabelable" | "removeLabelsFromLabelable") {
        let input = args.args.try_get("input")?.object()?;
        if matches!(
            node_id::decode(input.try_get("labelableId")?.string()?),
            Some(("D", _))
        ) {
            return boxed(|| crate::discussions::mutation(field, args, request)).await;
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
        return boxed(|| crate::issue::mutation(field, args, request)).await;
    }
    request
        .auth
        .require_write()
        .map_err(|_| Error::new("Resource not accessible by token"))?;
    let input: Value = args.args.try_get("input")?.deserialize()?;
    if crate::repository_mutations::FIELDS.contains(&field) {
        return boxed(|| crate::repository_mutations::resolve(field, &input, request)).await;
    }
    if matches!(field, "addReaction" | "removeReaction") {
        return boxed(|| crate::reaction_mutations::resolve(field, &input, request)).await;
    }
    if matches!(field, "updateIssueComment" | "deleteIssueComment") {
        return boxed(|| crate::comment_mutations::resolve(field, &input, request)).await;
    }
    if crate::review_mutations::FIELDS.contains(&field) {
        return boxed(|| crate::review_mutations::resolve(field, &input, request)).await;
    }
    if crate::collaboration_mutations::FIELDS.contains(&field) {
        return boxed(|| crate::collaboration_mutations::resolve(field, &input, request)).await;
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
            "projectIds",
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
            "projectIds",
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
    // projectIds lists classic projects, which Codeyard does not have. gh sends an empty
    // list beside its Projects edits; any ID in it names nothing.
    let projects = &input["projectIds"];
    if !(projects.is_null() || projects.as_array().is_some_and(Vec::is_empty)) {
        return Err(missing());
    }
    if request.bounded()
        && matches!(
            field,
            "addStar" | "removeStar" | "updateSubscription" | "followUser" | "unfollowUser"
        )
    {
        return Err(crate::token_denied());
    }
    let actor = request.auth.user.id;
    let pool = &request.state.pool;
    let broadcaster = &request.state.notification_broadcaster;
    let mut payload = json!({"clientMutationId":input["clientMutationId"],"actor":crate::user::actor_object(&request.auth.user,actor)});
    match field {
        "archiveRepository" | "unarchiveRepository" => {
            boxed(|| async move {
                let repo =
                    crate::repository::by_id(target(&input, "repositoryId", "R")?, request).await?;
                request.authorize(repo.id, "administration", true).await?;
                codeyard_db_repos::repositories::records::set_repository_archived(
                    pool,
                    actor,
                    repo.id,
                    field == "archiveRepository",
                )
                .await
                .map_err(|error| codeyard_api_github::repos::settings_error(error).graphql())?;
                payload["repository"] =
                    crate::repository::object(&crate::repository::by_id(repo.id, request).await?);
                Ok(payload)
            })
            .await
        }
        "updateIssue" => {
            boxed(|| async move {
                let id = target(&input, "id", "I")?;
                let current = codeyard_db_repos::issues::get_issue_by_id(pool, id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?;
                let repo = crate::repository::by_id(current.repo_id, request).await?;
                request.authorize(repo.id, "issues", true).await?;
                let metadata = crate::metadata_mutations::prepare(&input, &repo, request).await?;
                // A metadata-only edit needs triage, not the write access a content edit needs.
                let metadata_only = ["title", "body", "state", "stateReason"]
                    .iter()
                    .all(|key| input.get(*key).is_none_or(Value::is_null));
                let saved = if metadata_only {
                    current
                } else {
                    codeyard_repos_pulls::issues::mutations::update_issue_latest(
                        pool,
                        broadcaster,
                        actor,
                        repo.id,
                        current.number,
                        codeyard_repos_pulls::issues::mutations::UpdateIssueRequest {
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
                    .map_err(|error| issue_change_error(error, current.author_id, field, request))?
                };
                crate::metadata_mutations::apply(
                    &repo,
                    codeyard_db_repos::assignees::AssignedItem::Issue(saved.id),
                    saved.number,
                    &saved.title,
                    metadata,
                    request,
                )
                .await?;
                let saved = codeyard_db_repos::issues::get_issue_by_id(pool, saved.id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?;
                payload["issue"] = crate::issue::object(&saved);
                Ok(payload)
            })
            .await
        }
        "createPullRequest" => {
            boxed(|| async move {
                let repo = crate::repository::by_id(target(&input, "repositoryId", "R")?, request).await?;
                request.authorize(repo.id, "pull-requests", true).await?;
                codeyard_repos_pulls::repositories::identity_interactions::prepare(
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
                    let head = codeyard_ci_engine::pull_requests::branches::owner_head_repository(pool, &repo, owner)
                        .await
                        .map_err(unavailable)?
                        .ok_or_else(missing)?;
                    if !codeyard_identity::repository_access::can_read(
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
                let head = crate::repository::by_id(head_repo_id.unwrap_or(repo.id), request).await?;
                request.authorize(head.id, "contents", false).await?;
                let saved = codeyard_repos_pulls::repositories::pull_requests::mutations::create_pull_request(
                    pool,
                    &request.state.ci_runs,
                    broadcaster,
                    codeyard_repos_core::store_types::PullHeadActor {
                        user: actor,
                        token: Some(request.auth.token.id),
                    },
                    crate::repository::owner(&repo, request).await?,
                    repo.name,
                    codeyard_repos_pulls::repositories::pull_requests::mutations::CreatePullRequestRequest {
                        suppress_ci: false,
                        head_repo_id,
                        source: codeyard_repos_pulls::repositories::pull_requests::mutations::PullRequestSource::New {
                            title: text(&input, "title")?.into(),
                            body: optional_text(&input, "body"),
                        },
                        head_branch: head_branch.into(),
                        base_branch: text(&input, "baseRefName")?.into(),
                        is_draft: input["draft"].as_bool().unwrap_or(false),
                        maintainer_can_modify: Some(
                            input["maintainerCanModify"].as_bool().unwrap_or_else(|| {
                                codeyard_db_repos::pull_requests::records::can_grant_maintainer_edit(
                                    actor, repo.id, &head,
                                )
                            }),
                        ),
                    },
                )
                .await
                .map_err(pull_error)?;
                payload["pullRequest"] = crate::issue::pull_object(&saved);
                Ok(payload)
            })
            .await
        }
        "revertPullRequest" => {
            boxed(|| async move {
                let current = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
                    pool,
                    target(&input, "pullRequestId", "PR")?,
                )
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
                let repo = crate::repository::by_id(current.repo_id, request).await?;
                request.authorize(repo.id, "pull-requests", true).await?;
                request.authorize(repo.id, "contents", true).await?;
                request.require_oauth_scope("workflow")?;
                let saved =
                    codeyard_repos_pulls::repositories::pull_requests::revert::revert_pull_request(
                        pool,
                        &request.state.ci_runs,
                        broadcaster,
                        actor,
                        (
                            crate::repository::owner(&repo, request).await?,
                            repo.name,
                            current.number,
                        ),
                        codeyard_repos_pulls::repositories::pull_requests::revert::RevertRequest {
                            title: optional_text(&input, "title"),
                            body: optional_text(&input, "body"),
                            is_draft: input["draft"].as_bool().unwrap_or(false),
                        },
                    )
                    .await
                    .map_err(pull_error)?;
                payload["pullRequest"] = crate::issue::pull_object(&current);
                payload["revertPullRequest"] = crate::issue::pull_object(&saved);
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
                let current = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
                    pool,
                    target(&input, "pullRequestId", "PR")?,
                )
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
                let repo = crate::repository::by_id(current.repo_id, request).await?;
                request.authorize(repo.id, "pull-requests", true).await?;
                let owner = crate::repository::owner(&repo, request).await?;
                let metadata = crate::metadata_mutations::prepare(&input, &repo, request).await?;
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
                    let rebase = match input["updateMethod"].as_str() {
                        None | Some("MERGE") => false,
                        Some("REBASE") => true,
                        Some(_) => return Err(Error::new("updateMethod must be MERGE or REBASE")),
                    };
                    // The head may be a fork the actor cannot read but may update through
                    // the maintainer grant, which update_branch checks. The credential needs
                    // contents write there, as on REST.
                    request
                        .auth
                        .repository_scope(
                            pool,
                            current.head_repo_id.ok_or_else(missing)?,
                            "contents",
                            true,
                        )
                        .await
                        .map_err(|error| codeyard_api_github::auth::scope_error(error).graphql())?;
                    let expected = input
                        .get("expectedHeadOid")
                        .filter(|v| !v.is_null())
                        .map(|v| {
                            v.as_str().ok_or_else(|| {
                                Error::new("expectedHeadOid must be a Git object ID")
                            })
                        })
                        .transpose()?;
                    codeyard_repos_pulls::repositories::pull_requests::update_branch::update_branch(
                        pool,
                        &request.state.ci_runs,
                        actor,
                        (&owner, &repo.name, current.number),
                        expected,
                        rebase,
                        Some(request.auth.token.id),
                    )
                    .await
                    .map_err(pull_error)?;
                    codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, current.id)
                        .await
                        .map_err(unavailable)?
                        .ok_or_else(missing)?
                } else if matches!(
                    field,
                    "markPullRequestReadyForReview" | "convertPullRequestToDraft"
                ) {
                    codeyard_repos_pulls::repositories::pull_requests::mutations::set_pull_request_draft(
                        pool,
                        &request.state.ci_runs,
                        actor,
                        (owner, repo.name, current.number),
                        Some(&current.updated_at.to_rfc3339()),
                        field == "convertPullRequestToDraft",
                    )
                    .await
                    .map_err(|error| match error {
                        codeyard_repos_core::repositories::pull_requests::OperationError::Denied { .. } => {
                            crate::forbidden(field, request)
                        }
                        error => pull_error(error),
                    })?
                } else if field == "mergePullRequest" {
                    request.authorize(repo.id, "contents", true).await?;
                    let method = merge_method(&input, &current)?;
                    codeyard_repos_pulls::repositories::pull_requests::merge::merge_pull_request(
                        &request.state,
                        request.auth.database_actor(),
                        (owner, repo.name, current.number),
                        codeyard_repos_pulls::repositories::pull_requests::merge::MergeRequest {
                            expected_head: current.head_sha,
                            expected_base: current.base_sha,
                            expected_base_branch: Some(current.base_branch),
                            expected_target: None,
                            merge_method: method,
                            commit_title: optional_text(&input, "commitHeadline"),
                            commit_message: optional_text(&input, "commitBody"),
                            author_email: optional_text(&input, "authorEmail"),
                        },
                    )
                    .await
                    .map_err(pull_error)?;
                    codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, current.id)
                        .await
                        .map_err(unavailable)?
                        .ok_or_else(missing)?
                } else {
                    let state = match field {
                        "closePullRequest" => Some("closed".into()),
                        "reopenPullRequest" => Some("open".into()),
                        _ => optional_text(&input, "state").map(|s| s.to_ascii_lowercase()),
                    };
                    codeyard_repos_pulls::repositories::pull_requests::mutations::update_pull_request_latest(
                        pool,
                        &request.state.ci_runs,
                        broadcaster,
                        actor,
                        (repo.id, current.number),
                        codeyard_repos_pulls::repositories::pull_requests::mutations::UpdatePullRequestRequest {
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
                let repo = crate::repository::by_id(saved.repo_id, request).await?;
                crate::metadata_mutations::apply(
                    &repo,
                    codeyard_db_repos::assignees::AssignedItem::PullRequest(saved.id),
                    saved.number,
                    &saved.title,
                    metadata,
                    request,
                )
                .await?;
                let saved = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, saved.id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?;
                payload["pullRequest"] = crate::issue::pull_object(&saved);
                Ok(payload)
            })
            .await
        }
        "enablePullRequestAutoMerge" | "disablePullRequestAutoMerge" => {
            boxed(|| async move {
                let current = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
                    pool,
                    target(&input, "pullRequestId", "PR")?,
                )
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
                let repo = crate::repository::by_id(current.repo_id, request).await?;
                request.authorize(repo.id, "pull-requests", true).await?;
                if field == "enablePullRequestAutoMerge" {
                    request.authorize(repo.id, "contents", true).await?;
                    let options =
                    codeyard_repos_pulls::repositories::pull_requests::auto_merge::EnableRequest {
                        expected_head: optional_text(&input, "expectedHeadOid"),
                        token: Some(request.auth.token.id),
                        merge_method: merge_method(&input, &current)?,
                        commit_title: optional_text(&input, "commitHeadline"),
                        commit_message: optional_text(&input, "commitBody"),
                        author_email: optional_text(&input, "authorEmail"),
                    };
                    codeyard_repos_pulls::repositories::pull_requests::auto_merge::enable(
                        pool, actor, &current, options,
                    )
                    .await
                    .map_err(pull_error)?;
                } else {
                    codeyard_repos_pulls::repositories::pull_requests::auto_merge::disable(
                        pool, actor, &current,
                    )
                    .await
                    .map_err(pull_error)?;
                }
                payload["pullRequest"] = crate::issue::pull_object(&current);
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
                crate::repository::by_id(id, request).await?;
                if field == "updateSubscription" {
                    let level = match text(&input, "state")? {
                        "SUBSCRIBED" => "watching",
                        "IGNORED" => "ignoring",
                        "UNSUBSCRIBED" => "participating",
                        _ => return Err(Error::new("Invalid subscription state")),
                    };
                    codeyard_repos_pulls::repositories::engagement::set_level(
                        pool,
                        actor,
                        id,
                        level,
                        &[],
                    )
                    .await
                    .map_err(|error| match error {
                        codeyard_repos_pulls::repositories::engagement::EngagementError::Access(
                            _,
                        ) => missing(),
                        _ => unavailable(()),
                    })?;
                } else {
                    codeyard_repos_pulls::repositories::engagement::change(
                        pool,
                        broadcaster,
                        actor,
                        id,
                        codeyard_repos_pulls::repositories::engagement::Engagement::Star,
                        field == "addStar",
                    )
                    .await
                    .map_err(|error| match error {
                        codeyard_repos_pulls::repositories::engagement::EngagementError::Access(
                            _,
                        ) => missing(),
                        _ => unavailable(()),
                    })?;
                }
                payload[if field == "updateSubscription" {
                    "subscribable"
                } else {
                    "starrable"
                }] = crate::repository::object(&crate::repository::by_id(id, request).await?);
                Ok(payload)
            })
            .await
        }
        "followUser" | "unfollowUser" => {
            boxed(|| async move {
                // REST has no OAuth route for account follow writes, so OAuth tokens may not make them.
                if request.is_oauth() {
                    return Err(crate::token_denied());
                }
                let id = target(&input, "userId", "U")?;
                let current = codeyard_db_identity::users::accounts::get_user_by_id(pool, id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?;
                codeyard_notifications::following::change(
                    pool,
                    broadcaster,
                    actor,
                    &request.auth.credential(),
                    &current.username,
                    field == "followUser",
                )
                .await
                .map_err(|error| match error {
                    codeyard_notifications::following::FollowError::Missing => missing(),
                    codeyard_notifications::following::FollowError::Unavailable
                    | codeyard_notifications::following::FollowError::CountsUnavailableAfterSave => unavailable(()),
                    error => Error::new(error.to_string()),
                })?;
                payload["user"] = crate::user::object(&current, actor);
                Ok(payload)
            })
            .await
        }
        _ => unreachable!("mutation admission enumerates the supported operations"),
    }
}
