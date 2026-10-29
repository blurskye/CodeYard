use super::{auth, labels, pagination::Pagination, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db,
    db::reactions::{Reaction, ReactionRefusal, Subject, Target},
    webhooks::payloads,
};
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
    pool: &db::DbPool,
    repo_id: i64,
    p: &Params,
    route: &str,
    viewer: Option<i64>,
) -> Result<Target> {
    if let Some(id) = p.get("release_id") {
        let detail = crate::backend::repositories::releases::detail_by_id(
            pool,
            viewer,
            &p["owner"],
            &p["repo"],
            id.parse().map_err(|_| Error::missing())?,
        )
        .await
        .map_err(super::releases::release_error)?;
        return Ok(Target {
            repo_id,
            item_id: detail.release.release.id,
            subject: Subject::Release,
            subject_id: detail.release.release.id,
        });
    }
    if let Some(number) = p.get("issue_number") {
        let (subject, id) = match labels::item(pool, repo_id, number).await? {
            db::assignees::AssignedItem::Issue(id) => (Subject::Issue, id),
            db::assignees::AssignedItem::PullRequest(id) => (Subject::PullRequest, id),
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
        let comment = db::repositories::commit_comments::get_comment(pool, repo_id, id)
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
        let comment = db::pull_requests::reviews::get_pull_request_comment_by_id(pool, id)
            .await
            .map_err(Error::unavailable)?
            .filter(|comment| comment.path.is_some())
            .ok_or_else(Error::missing)?;
        let pull =
            db::pull_requests::records::get_pull_request_by_id(pool, comment.pull_request_id)
                .await
                .map_err(Error::unavailable)?
                .filter(|pull| pull.repo_id == repo_id)
                .ok_or_else(Error::missing)?;
        if let Some(review_id) = comment.review_id {
            let review = db::pull_requests::reviews::get_pull_request_review_by_id(pool, review_id)
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
    let comment = super::comments::find(pool, repo_id, id).await?;
    let (item_id, subject) = match comment.item {
        db::assignees::AssignedItem::Issue(id) => (id, Subject::IssueComment),
        db::assignees::AssignedItem::PullRequest(id) => (id, Subject::PullRequestComment),
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
        .downcast_ref::<db::discussions::DiscussionWriteRefusal>()
        .is_some()
    {
        return Error::new(StatusCode::FORBIDDEN, "Reaction mutation denied");
    }
    Error::unavailable(error)
}

async fn document(pool: &db::DbPool, reaction: &Reaction) -> Result<Value> {
    let user = db::users::accounts::get_user_by_id(pool, reaction.user_id)
        .await
        .map_err(Error::unavailable)?;
    Ok(payloads::reaction(reaction, user.as_ref()))
}

#[derive(Deserialize)]
struct Content {
    content: String,
}

fn validate(subject: Subject, content: &str) -> Result<()> {
    if db::reactions::valid_content(subject, content) {
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
    let actor = auth.ok_or_else(auth::required)?;
    let target = target(&state.pool, repo.id, &p, route.as_str(), Some(actor.user.id)).await?;
    validate(target.subject, &input.content)?;
    let (reaction, created) =
        db::reactions::create_reaction(&state.pool, actor.user.id, &actor.credential(), target, &input.content)
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
    let rows = db::reactions::list_reactions(
        &state.pool,
        target,
        query.content.as_deref(),
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let next = rows.len() > limit;
    let mut items = Vec::new();
    for row in rows {
        items.push(document(&state.pool, &row).await?);
    }
    page.response(&uri, items, next)
}

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    route: MatchedPath,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(auth::required)?;
    let target = target(&state.pool, repo.id, &p, route.as_str(), Some(actor.user.id)).await?;
    let id = p["reaction_id"].parse().map_err(|_| Error::missing())?;
    if !db::reactions::delete_reaction(&state.pool, actor.user.id, &actor.credential(), target, id)
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
