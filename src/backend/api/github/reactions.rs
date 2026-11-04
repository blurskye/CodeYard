use crate::backend::api::github::pagination::Pagination;
use crate::backend::api::github::repository;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use crate::backend::db::reactions::Reaction;
use crate::backend::db::reactions::ReactionRefusal;
use crate::backend::db::reactions::Subject;
use crate::backend::db::reactions::Target;

use axum::{
    extract::{MatchedPath, OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{delete, get, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

type Params = HashMap<String, String>;

async fn target(
    pool: &codeyard_db_core::DbPool,
    repo_id: i64,
    p: &Params,
    route: &str,
    viewer: Option<i64>,
) -> Result<Target> {
    if let Some(id) = p.get("release_id") {
        let detail = crate::backend::repositories::releases::detail_by_id(
            pool,
            viewer,
            repo_id,
            id.parse().map_err(|_| Error::missing())?,
        )
        .await
        .map_err(crate::backend::api::github::releases::release_error)?;
        return Ok(Target {
            repo_id,
            item_id: detail.release.release.id,
            subject: Subject::Release,
            subject_id: detail.release.release.id,
        });
    }
    if let Some(number) = p.get("issue_number") {
        let (subject, id) =
            match crate::backend::api::github::labels::item(pool, repo_id, number).await? {
                crate::backend::db::assignees::AssignedItem::Issue(id) => (Subject::Issue, id),
                crate::backend::db::assignees::AssignedItem::PullRequest(id) => {
                    (Subject::PullRequest, id)
                }
            };
        return Ok(Target {
            repo_id,
            item_id: id,
            subject,
            subject_id: id,
        });
    }
    let id = p["comment_id"].parse().map_err(|_| Error::missing())?;
    if !route.contains("/issues/comments/") && !route.contains("/pulls/comments/") {
        let comment =
            crate::backend::db::repositories::commit_comments::get_comment(pool, repo_id, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
        return Ok(Target {
            repo_id,
            item_id: comment.id,
            subject: Subject::CommitComment,
            subject_id: comment.id,
        });
    }
    if route.contains("/pulls/comments/") {
        let comment =
            crate::backend::db::pull_requests::reviews::get_pull_request_comment_by_id(pool, id)
                .await
                .map_err(Error::unavailable)?
                .filter(|comment| comment.path.is_some())
                .ok_or_else(Error::missing)?;
        let pull = crate::backend::db::pull_requests::records::get_pull_request_by_id(
            pool,
            comment.pull_request_id,
        )
        .await
        .map_err(Error::unavailable)?
        .filter(|pull| pull.repo_id == repo_id)
        .ok_or_else(Error::missing)?;
        if let Some(review_id) = comment.review_id {
            let review = crate::backend::db::pull_requests::reviews::get_pull_request_review_by_id(
                pool, review_id,
            )
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
            if review.state == "pending" {
                return Err(Error::missing());
            }
        }
        return Ok(Target {
            repo_id,
            item_id: pull.id,
            subject: Subject::PullRequestComment,
            subject_id: id,
        });
    }
    let comment = crate::backend::api::github::comments::find(pool, repo_id, id).await?;
    let (item_id, subject) = match comment.item {
        crate::backend::db::assignees::AssignedItem::Issue(id) => (id, Subject::IssueComment),
        crate::backend::db::assignees::AssignedItem::PullRequest(id) => {
            (id, Subject::PullRequestComment)
        }
    };
    Ok(Target {
        repo_id,
        item_id,
        subject,
        subject_id: comment.id,
    })
}

pub(in crate::backend::api) fn write_error(error: anyhow::Error) -> Error {
    if error.downcast_ref::<ReactionRefusal>().is_some() {
        return Error::missing();
    }
    if error
        .downcast_ref::<crate::backend::db::discussions::DiscussionWriteRefusal>()
        .is_some()
    {
        return Error::new(StatusCode::FORBIDDEN, "Reaction mutation denied");
    }
    Error::unavailable(error)
}

async fn document(pool: &codeyard_db_core::DbPool, reaction: &Reaction) -> Result<Value> {
    let user = crate::backend::db::users::accounts::get_user_by_id(pool, reaction.user_id)
        .await
        .map_err(Error::unavailable)?;
    Ok(crate::backend::webhooks::payloads::reaction(
        reaction,
        user.as_ref(),
    ))
}

#[derive(Deserialize)]
struct Content {
    content: String,
}

fn validate(subject: Subject, content: &str) -> Result<()> {
    if crate::backend::db::reactions::valid_content(subject, content) {
        Ok(())
    } else {
        Err(Error::invalid("Unknown reaction"))
    }
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    route: MatchedPath,
    Json(input): Json<Content>,
) -> Result<(StatusCode, Json<Value>)> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(crate::backend::api::github::auth::required)?;
    let target = target(
        &state.pool,
        repo.id,
        &p,
        route.as_str(),
        Some(actor.user.id),
    )
    .await?;
    validate(target.subject, &input.content)?;
    let (reaction, created) = crate::backend::db::reactions::create_reaction(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        target,
        &input.content,
    )
    .await
    .map_err(write_error)?;
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(document(&state.pool, &reaction).await?),
    ))
}

#[derive(Deserialize)]
struct List {
    content: Option<String>,
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    OriginalUri(uri): OriginalUri,
    route: MatchedPath,
    Query(query): Query<List>,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let target = target(
        &state.pool,
        repo.id,
        &p,
        route.as_str(),
        auth.as_ref().map(|a| a.user.id),
    )
    .await?;
    if let Some(content) = &query.content {
        validate(target.subject, content)?;
    }
    let (offset, limit) = page.window()?;
    let mut rows = crate::backend::db::reactions::list_reactions(
        &state.pool,
        target,
        query.content.as_deref(),
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let next = rows.len() > limit;
    rows.truncate(limit);
    let mut ids: Vec<_> = rows.iter().map(|row| row.user_id).collect();
    ids.sort_unstable();
    ids.dedup();
    let users: std::collections::HashMap<_, _> =
        crate::backend::db::users::accounts::get_users_by_ids(&state.pool, &ids)
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .map(|user| (user.id, user))
            .collect();
    let items = rows
        .iter()
        .map(|row| crate::backend::webhooks::payloads::reaction(row, users.get(&row.user_id)))
        .collect();
    page.response(&uri, items, next)
}

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    route: MatchedPath,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(crate::backend::api::github::auth::required)?;
    let target = target(
        &state.pool,
        repo.id,
        &p,
        route.as_str(),
        Some(actor.user.id),
    )
    .await?;
    let id = p["reaction_id"].parse().map_err(|_| Error::missing())?;
    if !crate::backend::db::reactions::delete_reaction(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        target,
        id,
    )
    .await
    .map_err(write_error)?
    {
        return Err(Error::missing());
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("reactions/create-for-issue", post(create)),
        ("reactions/list-for-issue", get(list)),
        ("reactions/delete-for-issue", delete(remove)),
        ("reactions/create-for-issue-comment", post(create)),
        ("reactions/list-for-issue-comment", get(list)),
        ("reactions/delete-for-issue-comment", delete(remove)),
        (
            "reactions/create-for-pull-request-review-comment",
            post(create),
        ),
        ("reactions/list-for-pull-request-review-comment", get(list)),
        ("reactions/delete-for-pull-request-comment", delete(remove)),
        ("reactions/create-for-release", post(create)),
        ("reactions/list-for-release", get(list)),
        ("reactions/delete-for-release", delete(remove)),
        ("reactions/create-for-commit-comment", post(create)),
        ("reactions/list-for-commit-comment", get(list)),
        ("reactions/delete-for-commit-comment", delete(remove)),
    ]
}
