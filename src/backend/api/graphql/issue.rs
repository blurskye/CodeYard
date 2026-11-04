use crate::backend::api::graphql::bindings::boxed;
use crate::backend::api::graphql::mutations::issue_error as operation;

use crate::backend::api::graphql::connection::Page;
use crate::backend::api::graphql::db;
use crate::backend::api::graphql::missing;
use crate::backend::api::graphql::node_id;

use crate::backend::api::graphql::unavailable;
use crate::backend::api::graphql::url;

use crate::backend::api::graphql::Arguments;
use crate::backend::api::graphql::RequestContext;
use crate::backend::api::graphql::Result;
use crate::backend::api::graphql::Value;

use crate::backend::issues::comments::CreateCommentRequest;
use crate::backend::issues::mutations::CreateIssueRequest;
use crate::backend::issues::mutations::UpdateIssueRequest;
use async_graphql::Error;
use serde_json::json;

mod management;
pub(super) mod metadata;
mod mutations;
pub(super) use metadata::search;

pub(super) async fn linked_branch_node(id: i64, request: &RequestContext) -> Result<Value> {
    let branch = db::graphql::linked_branch(&request.state.pool, id)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    management::linked(branch, request)
        .await?
        .ok_or_else(missing)
}

fn lock_reason(value: Option<&str>) -> Option<&str> {
    value.and_then(|value| match value {
        "off-topic" => Some("OFF_TOPIC"),
        "too heated" => Some("TOO_HEATED"),
        "resolved" => Some("RESOLVED"),
        "spam" => Some("SPAM"),
        _ => None,
    })
}

pub(super) fn object(issue: &codeyard_db_core::models::Issue) -> Value {
    json!({"__typename":"Issue","id":node_id::encode("I",issue.id),"databaseId":issue.id,
        "_repo":issue.repo_id,"_author":issue.author_id,"_milestone":issue.milestone_id,"number":issue.number,"title":issue.title,
        "updatedAt":issue.updated_at,"closed":issue.state=="closed","locked":issue.locked,"activeLockReason":lock_reason(issue.lock_reason.as_deref()),"stateReason":issue.state_reason.as_ref().map(|s|s.to_uppercase()),
        "body":issue.body,"state":issue.state.to_uppercase(),"createdAt":issue.created_at,"closedAt":issue.closed_at})
}

pub(super) fn pull_object(pull: &codeyard_db_core::models::PullRequest) -> Value {
    json!({"__typename":"PullRequest","id":node_id::encode("PR",pull.id),"databaseId":pull.id,
        "_repo":pull.repo_id,"_author":pull.author_id,"_milestone":pull.milestone_id,"number":pull.number,"title":pull.title,
        "updatedAt":pull.updated_at,"maintainerCanModify":pull.maintainer_can_modify,"closed":pull.state!="open","locked":pull.locked,"activeLockReason":lock_reason(pull.lock_reason.as_deref()),
        "body":pull.body,"state":if pull.merged {"MERGED".to_owned()} else {pull.state.to_uppercase()},
        "createdAt":pull.created_at,"closedAt":pull.closed_at,"baseRefName":pull.base_branch,
        "headRefName":pull.head_branch,"isDraft":pull.is_draft,"merged":pull.merged,"mergedAt":pull.merged_at})
}

pub(super) async fn resolve(
    ty: &str,
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
    parent: &Value,
) -> Result<Value> {
    if let Some(value) = parent.get(field) {
        return Ok(value.clone());
    }
    let repo = crate::backend::api::graphql::repository::by_id(
        parent["_repo"].as_i64().ok_or_else(missing)?,
        request,
    )
    .await?;
    let id = parent["databaseId"].as_i64().ok_or_else(missing)?;
    let pool = &request.state.pool;
    match field {
        "projectItems" => {
            boxed(|| crate::backend::api::graphql::projects::content_items(ty, id, ctx, request))
                .await
        }
        "isPinned" | "linkedBranches" | "closedByPullRequestsReferences" if ty == "Issue" => {
            boxed(|| management::resolve(field, ctx, request, &repo, id)).await
        }
        "issueType" | "parent" | "subIssues" | "subIssuesSummary" | "blockedBy" | "blocking"
            if ty == "Issue" =>
        {
            boxed(|| metadata::resolve(ty, field, ctx, request, parent, &repo, id)).await
        }
        "author" => {
            crate::backend::api::graphql::user::retained_author(
                parent["_author"].as_i64().ok_or_else(missing)?,
                request,
            )
            .await
        }
        "url" => Ok(json!(url(&format!(
            "/{}/{}/{}/{}",
            crate::backend::api::graphql::repository::owner(&repo, request).await?,
            repo.name,
            if ty == "Issue" { "issues" } else { "pull" },
            parent["number"]
        )))),
        "reviewRequests" if ty == "PullRequest" => {
            crate::backend::api::graphql::review_requests::list(ctx, request, id).await
        }
        "labels" => {
            crate::backend::api::graphql::connection::validate(ctx)?;
            let labels = if ty == "Issue" {
                db::labels::get_issue_labels(pool, repo.id, id).await
            } else {
                db::labels::get_pull_request_labels(pool, repo.id, id).await
            }
            .map_err(unavailable)?;
            crate::backend::api::graphql::repository::label_connection(
                ctx,
                labels,
                if ty == "PullRequest" {
                    "pull-requests"
                } else {
                    "issues"
                },
            )
        }
        "comments" => {
            crate::backend::api::graphql::connection::validate(ctx)?;
            let (updated, descending) =
                if let Some(order) = ctx.args.get("orderBy").filter(|value| !value.is_null()) {
                    let order = order.object()?;
                    let updated = match order.try_get("field")?.enum_name()? {
                        "UPDATED_AT" => true,
                        "CREATED_AT" => false,
                        _ => return Err(async_graphql::Error::new("Unsupported comment order")),
                    };
                    (updated, order.try_get("direction")?.enum_name()? == "DESC")
                } else {
                    (false, false)
                };
            let count = if ty == "Issue" {
                db::issues::count_issue_comments(pool, &[id])
                    .await
                    .map_err(unavailable)?
                    .first()
                    .map(|(_, n)| *n)
                    .unwrap_or(0)
            } else {
                db::pull_requests::reviews::count_pull_request_conversation_comments(pool, id)
                    .await
                    .map_err(unavailable)?
            };
            let page = Page::new(ctx, count)?;
            let mut nodes = Vec::new();
            if ty == "Issue" {
                for entry in db::issues::list_issue_comments_ordered(
                    pool,
                    id,
                    page.limit,
                    page.offset,
                    updated,
                    descending,
                )
                .await
                .map_err(unavailable)?
                {
                    nodes.push(comment(&entry.comment, request).await?);
                }
            } else {
                for entry in
                    db::pull_requests::reviews::list_pull_request_conversation_comments_ordered(
                        pool,
                        id,
                        page.limit,
                        page.offset,
                        updated,
                        descending,
                    )
                    .await
                    .map_err(unavailable)?
                {
                    nodes.push(pull_comment(&entry.comment, request).await?);
                }
            }
            Ok(page.finish(nodes))
        }
        _ if ty == "PullRequest" => {
            boxed(|| {
                crate::backend::api::graphql::pull_request::resolve(
                    field, ctx, request, parent, &repo,
                )
            })
            .await
        }
        _ => {
            boxed(|| {
                crate::backend::api::graphql::collaboration::resolve(
                    ty, field, ctx, request, parent, &repo,
                )
            })
            .await
        }
    }
}

pub(super) async fn comment(
    comment: &codeyard_db_core::models::IssueComment,
    request: &RequestContext,
) -> Result<Value> {
    let issue = db::issues::get_issue_by_id(&request.state.pool, comment.issue_id)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    crate::backend::api::graphql::repository::by_id(issue.repo_id, request).await?;
    request.charge(comment.body.len().saturating_mul(4))?;
    Ok(
        json!({"__typename":"IssueComment","id":node_id::encode("IC",codeyard_model::webhook_ids::conversation_comment_id(comment.id,false)),"databaseId":codeyard_model::webhook_ids::conversation_comment_id(comment.id,false),"_comment":comment.id,
        "_repo":issue.repo_id,"_number":issue.number,"_pull":false,"_author":comment.author_id,
        "viewerDidAuthor":comment.author_id==request.auth.user.id,"includesCreatedEdit":comment.updated_at>comment.created_at,"isMinimized":false,"minimizedReason":null,
        "body":comment.body,"createdAt":comment.created_at,"updatedAt":comment.updated_at,"author":user::retained_author(comment.author_id,request).await?}),
    )
}

pub(super) async fn pull_comment(
    comment: &codeyard_db_core::models::PullRequestComment,
    request: &RequestContext,
) -> Result<Value> {
    let pull = db::pull_requests::records::get_pull_request_by_id(
        &request.state.pool,
        comment.pull_request_id,
    )
    .await
    .map_err(unavailable)?
    .ok_or_else(missing)?;
    crate::backend::api::graphql::repository::by_id(pull.repo_id, request).await?;
    request.charge(comment.body.len().saturating_mul(4))?;
    Ok(
        json!({"__typename":"IssueComment","id":node_id::encode("IC",codeyard_model::webhook_ids::conversation_comment_id(comment.id,true)),"databaseId":codeyard_model::webhook_ids::conversation_comment_id(comment.id,true),"_comment":comment.id,
        "_repo":pull.repo_id,"_number":pull.number,"_pull":true,"_author":comment.author_id,
        "viewerDidAuthor":comment.author_id==request.auth.user.id,"includesCreatedEdit":comment.updated_at>comment.created_at,"isMinimized":false,"minimizedReason":null,
        "body":comment.body,"createdAt":comment.created_at,"updatedAt":comment.updated_at,"author":user::retained_author(comment.author_id,request).await?}),
    )
}

pub(super) async fn list(
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
    repo: i64,
) -> Result<Value> {
    crate::backend::api::graphql::connection::validate(ctx)?;
    if ["filterBy", "baseRefName", "headRefName"]
        .iter()
        .any(|name| ctx.args.get(name).is_some_and(|value| !value.is_null()))
    {
        return metadata::list(field, ctx, request, repo).await;
    }
    let mut states = ctx
        .args
        .get("states")
        .filter(|v| !v.is_null())
        .map(|v| v.deserialize::<Vec<String>>())
        .transpose()?
        .unwrap_or_default();
    states
        .iter_mut()
        .for_each(|state| state.make_ascii_lowercase());
    states.sort();
    states.dedup();
    if states.len() > 1 {
        return metadata::list(field, ctx, request, repo).await;
    }
    let names: Vec<_> = states.iter().map(String::as_str).collect();
    let (state, include_merged) = match names.as_slice() {
        [] => (None, false),
        [state] => (Some(*state), false),
        ["closed", "open"] if field == "issues" => (None, false),
        ["closed", "merged", "open"] => (None, false),
        ["closed", "merged"] => (Some("closed"), true),
        _ => {
            return Err(Error::new(
                "Codeyard does not support this state combination yet",
            ))
        }
    };
    let labels = ctx
        .args
        .get("labels")
        .filter(|v| !v.is_null())
        .map(|v| v.deserialize::<Vec<String>>())
        .transpose()?
        .unwrap_or_default();
    let mut filter = db::discussions::ListFilter {
        state,
        include_merged,
        labels: &labels,
        ..Default::default()
    };
    if let Some(order) = ctx.args.get("orderBy").filter(|v| !v.is_null()) {
        let order = order.object()?;
        let ascending = order.try_get("direction")?.enum_name()? == "ASC";
        filter.sort = match (order.try_get("field")?.enum_name()?, ascending) {
            ("CREATED_AT", true) => db::discussions::ListSort::Oldest,
            ("CREATED_AT", false) => db::discussions::ListSort::Newest,
            ("UPDATED_AT", true) => db::discussions::ListSort::LeastRecentlyUpdated,
            ("UPDATED_AT", false) => db::discussions::ListSort::RecentlyUpdated,
            ("COMMENTS", true) => db::discussions::ListSort::LeastCommented,
            ("COMMENTS", false) => db::discussions::ListSort::MostCommented,
            _ => return Err(Error::new("Codeyard does not support this order yet")),
        };
    }
    let pool = &request.state.pool;
    let count = if field == "issues" {
        let counts = db::issues::count_repo_issues(pool, repo, &filter)
            .await
            .map_err(unavailable)?;
        match state {
            Some("open") => counts.open,
            Some("closed") => counts.closed,
            _ => counts.all,
        }
    } else {
        db::pull_requests::records::count_repo_pull_requests_by_state(pool, repo, &filter)
            .await
            .map_err(unavailable)?
            .into_iter()
            .filter(|(state, _)| states.is_empty() || states.contains(state))
            .map(|(_, count)| count)
            .sum()
    };
    let page = Page::new(ctx, count)?;
    filter.offset = page.offset;
    let nodes = if page.limit == 0 {
        Vec::new()
    } else if field == "issues" {
        db::issues::list_repo_issues(pool, repo, &filter, page.limit)
            .await
            .map_err(unavailable)?
            .iter()
            .map(|entry| {
                request.charge(
                    entry
                        .issue
                        .title
                        .len()
                        .saturating_add(entry.issue.body.len())
                        .saturating_mul(4),
                )?;
                Ok(object(&entry.issue))
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        db::pull_requests::records::list_repo_pull_requests(pool, repo, &filter, page.limit)
            .await
            .map_err(unavailable)?
            .iter()
            .map(|entry| {
                request.charge(
                    entry
                        .pull_request
                        .title
                        .len()
                        .saturating_add(entry.pull_request.body.len())
                        .saturating_mul(4),
                )?;
                Ok(pull_object(&entry.pull_request))
            })
            .collect::<Result<Vec<_>>>()?
    };
    Ok(page.finish(nodes))
}

pub(super) async fn mutation(
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
) -> Result<Value> {
    request
        .auth
        .require_write()
        .map_err(|_| Error::new("Resource not accessible by token"))?;
    if !matches!(
        field,
        "createIssue" | "addComment" | "closeIssue" | "reopenIssue"
    ) {
        return mutations::resolve(field, ctx, request).await;
    }
    let input: Value = ctx.args.try_get("input")?.deserialize()?;
    let string = |name| {
        input[name]
            .as_str()
            .ok_or_else(|| Error::new(format!("Missing {name}")))
    };
    let allowed: &[&str] = match field {
        "createIssue" => &[
            "repositoryId",
            "title",
            "body",
            "labelIds",
            "assigneeIds",
            "milestoneId",
            "clientMutationId",
        ],
        "addComment" => &["subjectId", "body", "clientMutationId"],
        "closeIssue" => &[
            "issueId",
            "stateReason",
            "duplicateIssueId",
            "clientMutationId",
        ],
        _ => &["issueId", "clientMutationId"],
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
    let target = string(match field {
        "createIssue" => "repositoryId",
        "addComment" => "subjectId",
        _ => "issueId",
    })?;
    let (kind, id) = node_id::decode(target).ok_or_else(missing)?;
    let pool = &request.state.pool;
    let broadcaster = &request.state.notification_broadcaster;
    let actor = request.auth.user.id;
    let mut payload = json!({"clientMutationId":input["clientMutationId"]});
    if field == "createIssue" {
        if kind != "R" {
            return Err(missing());
        }
        let repo = crate::backend::api::graphql::repository::by_id(id, request).await?;
        request.authorize(repo.id, "issues", true).await?;
        crate::backend::identity::interactions::prepare(&request.state, &request.auth.user, &repo)
            .await
            .map_err(unavailable)?;
        let metadata =
            crate::backend::api::graphql::metadata_mutations::prepare(&input, &repo, request)
                .await?;
        let issue = crate::backend::issues::mutations::create_issue_with_metadata(
            pool,
            broadcaster,
            request.auth.database_actor(),
            crate::backend::api::graphql::repository::owner(&repo, request).await?,
            repo.name.clone(),
            CreateIssueRequest {
                title: string("title")?.to_owned(),
                body: input["body"].as_str().map(str::to_owned),
                repository_id: Some(repo.id),
                parent_issue_id: None,
                parent_repository_id: None,
            },
            metadata.creation(),
        )
        .await
        .map_err(operation)?;
        payload["issue"] = object(&issue);
    } else if field == "addComment" && kind == "PR" {
        let pull = db::pull_requests::records::get_pull_request_by_id(pool, id)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        let repo = crate::backend::api::graphql::repository::by_id(pull.repo_id, request).await?;
        request.authorize(repo.id, "pull-requests", true).await?;
        crate::backend::identity::interactions::prepare(&request.state, &request.auth.user, &repo)
            .await
            .map_err(unavailable)?;
        let body = crate::backend::repositories::pull_requests::comments::CreatePullRequestCommentRequest {
            body:string("body")?.to_owned(),repository_id:Some(repo.id),path:None,line:None,side:None,start_line:None,start_side:None,commit_sha:None,base_sha:None,in_reply_to_comment_id:None,review_id:None,pending:false,
        };
        let saved =
            crate::backend::repositories::pull_requests::comments::create_pull_request_comment(
                pool,
                request.state.cluster.as_ref(),
                broadcaster,
                actor,
                crate::backend::api::graphql::repository::owner(&repo, request).await?,
                repo.name.clone(),
                pull.number,
                body,
            )
            .await
            .map_err(|error| {
                use crate::backend::repositories::pull_requests::OperationError;
                match error {
                    OperationError::Missing { .. } => missing(),
                    OperationError::Unavailable { .. } => unavailable(()),
                    error => Error::new(error.to_string()),
                }
            })?;
        let position = db::pull_requests::reviews::conversation_comment_position(pool, saved.id)
            .await
            .map_err(unavailable)?
            - 1;
        payload["commentEdge"] = json!({"cursor":connection::cursor(position),"node":pull_comment(&saved,request).await?});
        payload["subject"] = pull_object(&pull);
    } else {
        if kind != "I" {
            return Err(missing());
        }
        let issue = db::issues::get_issue_by_id(pool, id)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        let repo = crate::backend::api::graphql::repository::by_id(issue.repo_id, request).await?;
        request.authorize(repo.id, "issues", true).await?;
        crate::backend::identity::interactions::prepare(&request.state, &request.auth.user, &repo)
            .await
            .map_err(unavailable)?;
        let owner = crate::backend::api::graphql::repository::owner(&repo, request).await?;
        let state_error = |error| {
            crate::backend::api::graphql::mutations::issue_change_error(
                error,
                issue.author_id,
                field,
                request,
            )
        };
        if field == "addComment" {
            let saved = crate::backend::issues::comments::create_issue_comment(
                pool,
                broadcaster,
                actor,
                &request.auth.credential(),
                owner,
                repo.name.clone(),
                issue.number,
                CreateCommentRequest {
                    body: string("body")?.to_owned(),
                    repository_id: Some(repo.id),
                    issue_id: Some(issue.id),
                },
            )
            .await
            .map_err(operation)?;
            let position = db::issues::issue_comment_position(pool, saved.id)
                .await
                .map_err(unavailable)?
                - 1;
            payload["commentEdge"] = json!({"cursor":connection::cursor(position),"node":comment(&saved,request).await?});
            payload["subject"] = object(&issue);
        } else if field == "closeIssue" && !input["duplicateIssueId"].is_null() {
            let duplicate = mutations::id(&input, "duplicateIssueId", "I")?;
            let target = db::issues::get_issue_by_id(pool, duplicate)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            crate::backend::api::graphql::repository::by_id(target.repo_id, request).await?;
            request.authorize(target.repo_id, "issues", false).await?;
            payload["issue"] = object(
                &crate::backend::issues::mutations::close_duplicate(
                    pool,
                    broadcaster,
                    request.actor.clone(),
                    repo.id,
                    issue.id,
                    duplicate,
                )
                .await
                .map_err(state_error)?,
            );
        } else {
            let saved = crate::backend::issues::mutations::update_issue_latest(
                pool,
                broadcaster,
                actor,
                repo.id,
                issue.number,
                UpdateIssueRequest {
                    expected_state: issue.state,
                    expected_updated: issue.updated_at.to_rfc3339(),
                    title: None,
                    body: None,
                    state: Some(
                        if field == "closeIssue" {
                            "closed"
                        } else {
                            "open"
                        }
                        .into(),
                    ),
                    state_reason: Some(if field == "reopenIssue" {
                        "reopened".into()
                    } else {
                        input["stateReason"]
                            .as_str()
                            .unwrap_or("COMPLETED")
                            .to_ascii_lowercase()
                    }),
                },
            )
            .await
            .map_err(state_error)?;
            payload["issue"] = object(&saved);
        }
    }
    Ok(payload)
}
