mod writes;
use super::super::{auth, pagination::Pagination, repository, Error, Result};
use super::{association, target};
use crate::backend::{app::AppState, db, repositories::pull_requests, webhooks::payloads};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

async fn document(
    state: &AppState,
    owner: &str,
    repo: &db::models::Repository,
    pull: &db::models::PullRequest,
    review: &db::models::PullRequestReview,
) -> Result<Value> {
    let user = db::users::accounts::get_user_by_id(&state.pool, review.author_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let mut value =
        payloads::pull_request_review_json(review, &payloads::Person::of(&user), pull, repo, owner);
    value["user"] = payloads::user(&user);
    value["author_association"] = json!(association(&state.pool, repo, user.id).await?);
    Ok(value)
}

async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number, review_id)): Path<(String, String, i32, i64)>,
) -> Result<Json<Value>> {
    let (repo, pull) = target(&state, &headers, &owner, &name, number).await?;
    let viewer = auth::authenticate(&state, &headers, false).await?;
    let review = db::pull_requests::reviews::get_pull_request_review_by_id(&state.pool, review_id)
        .await
        .map_err(Error::unavailable)?
        .filter(|review| {
            review.pull_request_id == pull.id
                && (review.state != "pending"
                    || viewer
                        .as_ref()
                        .is_some_and(|viewer| viewer.user.id == review.author_id))
        })
        .ok_or_else(Error::missing)?;
    Ok(Json(document(&state, &owner, &repo, &pull, &review).await?))
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, pull) = target(&state, &headers, &owner, &name, number).await?;
    let viewer = auth::authenticate(&state, &headers, false).await?;
    let (offset, limit) = page.window()?;
    let rows = db::pull_requests::reviews::list_visible_pull_request_reviews(
        &state.pool,
        pull.id,
        viewer.as_ref().map(|auth| auth.user.id),
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = rows.len() > limit;
    let mut items = Vec::new();
    for review in rows.iter().take(limit) {
        items.push(document(&state, &owner, &repo, &pull, review).await?);
    }
    page.response(&uri, items, has_next)
}

#[derive(Deserialize)]
struct Dismiss {
    message: String,
    event: Option<String>,
}

async fn dismiss(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number, review_id)): Path<(String, String, i32, i64)>,
    input: std::result::Result<Json<Dismiss>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>> {
    let (_, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    // GitHub documents this 422 with plain string errors.
    let Json(input) = input.map_err(|error| super::super::json_error(error).simple())?;
    if input.message.trim().is_empty()
        || input
            .event
            .as_deref()
            .is_some_and(|event| event != "DISMISS")
    {
        return Err(Error::invalid_simple(
            "A dismissal message and DISMISS event are required",
        ));
    }
    let review = pull_requests::reviews::dismiss_pull_request_review(
        &state.pool,
        actor.id,
        owner.clone(),
        name.clone(),
        number,
        review_id,
        pull_requests::reviews::DismissPullRequestReviewRequest {
            reason: Some(input.message),
        },
    )
    .await
    .map_err(|error| match error {
        pull_requests::OperationError::Invalid { message, .. } => Error::invalid_simple(message),
        pull_requests::OperationError::Missing { .. } => Error::missing(),
        pull_requests::OperationError::Denied { message, .. } => {
            Error::new(StatusCode::FORBIDDEN, message)
        }
        _ => Error::unavailable(()),
    })?;
    let (repo, pull) = target(&state, &headers, &owner, &name, number).await?;
    Ok(Json(document(&state, &owner, &repo, &pull, &review).await?))
}

async fn requested(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, pull) = target(&state, &headers, &owner, &name, number).await?;
    let viewer = auth::authenticate(&state, &headers, false).await?;
    let teams = super::team_reviewers::documents(
        &state,
        repo.id,
        pull.id,
        viewer.as_ref().map(|viewer| viewer.user.id),
    )
    .await?;
    let rows = db::pull_requests::reviews::list_pull_request_reviewers(&state.pool, pull.id)
        .await
        .map_err(Error::unavailable)?;
    let (offset, limit) = page.window()?;
    let more =
        rows.len() > offset.saturating_add(limit) || teams.len() > offset.saturating_add(limit);
    let users: Vec<_> = rows
        .iter()
        .skip(offset)
        .take(limit)
        .map(payloads::user)
        .collect();
    let teams: Vec<_> = teams.into_iter().skip(offset).take(limit).collect();
    Ok((
        page.headers(&uri, more)?,
        Json(json!({"users": users, "teams": teams})),
    )
        .into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    let mut handlers = vec![
        ("pulls/get-review", get(read)),
        ("pulls/list-reviews", get(list)),
        ("pulls/dismiss-review", put(dismiss)),
        ("pulls/list-requested-reviewers", get(requested)),
    ];
    handlers.extend(writes::handlers());
    handlers
}
