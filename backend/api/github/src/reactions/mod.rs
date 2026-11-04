use axum::extract::MatchedPath;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_db_repos::reactions::Reaction;
use codeyard_db_repos::reactions::ReactionRefusal;
use codeyard_db_repos::reactions::Subject;
use codeyard_db_repos::reactions::Target;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

type Params = HashMap<String, String>;

async fn target(
    pool: &codeyard_db_core::DbPool,
    repo_id: i64,
    p: &Params,
    route: &str,
    viewer: Option<i64>,
) -> Result<Target> {
    if let Some(id) = p.get("release_id") {
        let detail = codeyard_app::repositories::releases::detail_by_id(
            pool,
            viewer,
            repo_id,
            id.parse().map_err(|_| Error::missing())?,
        )
        .await
        .map_err(crate::repos::releases::release_error)?;
        return Ok(Target {
            repo_id,
            item_id: detail.release.release.id,
            subject: Subject::Release,
            subject_id: detail.release.release.id,
        });
    }
    if let Some(number) = p.get("issue_number") {
        let (subject, id) =
            match codeyard_app::api_shared::github::labels::item(pool, repo_id, number).await? {
                codeyard_db_repos::assignees::AssignedItem::Issue(id) => (Subject::Issue, id),
                codeyard_db_repos::assignees::AssignedItem::PullRequest(id) => {
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
            codeyard_db_repos::repositories::commit_comments::get_comment(pool, repo_id, id)
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
            codeyard_db_repos::pull_requests::reviews::get_pull_request_comment_by_id(pool, id)
                .await
                .map_err(Error::unavailable)?
                .filter(|comment| comment.path.is_some())
                .ok_or_else(Error::missing)?;
        let pull = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
            pool,
            comment.pull_request_id,
        )
        .await
        .map_err(Error::unavailable)?
        .filter(|pull| pull.repo_id == repo_id)
        .ok_or_else(Error::missing)?;
        if let Some(review_id) = comment.review_id {
            let review = codeyard_db_repos::pull_requests::reviews::get_pull_request_review_by_id(
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
    let comment = codeyard_app::api_shared::github::comments::find(pool, repo_id, id).await?;
    let (item_id, subject) = match comment.item {
        codeyard_db_repos::assignees::AssignedItem::Issue(id) => (id, Subject::IssueComment),
        codeyard_db_repos::assignees::AssignedItem::PullRequest(id) => {
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

pub fn write_error(error: anyhow::Error) -> Error {
    if error.downcast_ref::<ReactionRefusal>().is_some() {
        return Error::missing();
    }
    if error
        .downcast_ref::<codeyard_db_core::queries::discussions::DiscussionWriteRefusal>()
        .is_some()
    {
        return Error::new(StatusCode::FORBIDDEN, "Reaction mutation denied");
    }
    Error::unavailable(error)
}

async fn document(pool: &codeyard_db_core::DbPool, reaction: &Reaction) -> Result<Value> {
    let user = codeyard_db_identity::users::accounts::get_user_by_id(pool, reaction.user_id)
        .await
        .map_err(Error::unavailable)?;
    Ok(codeyard_app::webhooks::payloads::reaction(
        reaction,
        user.as_ref(),
    ))
}

#[derive(Deserialize)]
struct Content {
    content: String,
}

fn validate(subject: Subject, content: &str) -> Result<()> {
    if codeyard_db_repos::reactions::valid_content(subject, content) {
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
    let actor = auth.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let target = target(
        &state.pool,
        repo.id,
        &p,
        route.as_str(),
        Some(actor.user.id),
    )
    .await?;
    validate(target.subject, &input.content)?;
    let (reaction, created) = codeyard_db_repos::reactions::create_reaction(
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
    let mut rows = codeyard_db_repos::reactions::list_reactions(
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
        codeyard_db_identity::users::accounts::get_users_by_ids(&state.pool, &ids)
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .map(|user| (user.id, user))
            .collect();
    let items = rows
        .iter()
        .map(|row| codeyard_app::webhooks::payloads::reaction(row, users.get(&row.user_id)))
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
    let actor = auth.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let target = target(
        &state.pool,
        repo.id,
        &p,
        route.as_str(),
        Some(actor.user.id),
    )
    .await?;
    let id = p["reaction_id"].parse().map_err(|_| Error::missing())?;
    if !codeyard_db_repos::reactions::delete_reaction(
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
