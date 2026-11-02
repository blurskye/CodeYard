use super::{auth, engagement_error, repository, Error, Pagination, Result};
use crate::backend::{app::AppState, db, repositories::engagement, webhooks::payloads};
use axum::{
    body::Bytes,
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{delete, get, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

fn document(owner: &str, name: &str, watch: &db::models::Watch) -> Value {
    let url = super::super::repo_url(owner, name);
    json!({"subscribed":matches!(watch.level.as_str(),"watching"|"custom"),"ignored":watch.level=="ignoring","reason":null,"created_at":watch.created_at,"url":format!("{url}/subscription"),"repository_url":url})
}
fn empty(status: StatusCode) -> Response {
    (status, [(header::CONTENT_LENGTH, "0")]).into_response()
}

async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(namespace): Path<(String, String)>,
) -> Result<Response> {
    match read_value(State(state), headers, Path(namespace)).await {
        Ok(value) => Ok(value.into_response()),
        Err(Error(StatusCode::NOT_FOUND, _, _)) => Ok(empty(StatusCode::NOT_FOUND)),
        Err(error) => Err(error),
    }
}

async fn read_value(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?
        .user;
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let watch = db::repositories::engagement::get_repo_watch(&state.pool, actor.id, repo.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(Json(document(&owner, &name, &watch)))
}
#[derive(Default, Deserialize)]
struct Subscription {
    subscribed: Option<bool>,
    ignored: Option<bool>,
}
// Watching's PUT specifies a JSON success document, but no JSON error representation.
async fn set(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(namespace): Path<(String, String)>,
    body: Bytes,
) -> Response {
    match set_value(State(state), headers, Path(namespace), body).await {
        Ok(value) => value.into_response(),
        Err(error) => empty(error.0),
    }
}

async fn set_value(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<Value>> {
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = viewer.ok_or_else(auth::required)?.user;
    let input: Subscription =
        serde_json::from_slice(&body).map_err(|_| Error::invalid("Invalid subscription"))?;
    let subscribed = input.subscribed.unwrap_or(false);
    let ignored = input.ignored.unwrap_or(false);
    if subscribed && ignored {
        return Err(Error::invalid(
            "A subscription cannot be both subscribed and ignored",
        ));
    }
    engagement::set_subscription(
        &state.pool,
        &state.notification_broadcaster,
        actor.id,
        repo.id,
        subscribed,
        ignored,
    )
    .await
    .map_err(engagement_error)?;
    let watch = db::repositories::engagement::get_repo_watch(&state.pool, actor.id, repo.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(|| Error::unavailable(()))?;
    Ok(Json(document(&owner, &name, &watch)))
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = viewer.ok_or_else(auth::required)?.user;
    engagement::set_level(&state.pool, actor.id, repo.id, "participating", &[])
        .await
        .map_err(engagement_error)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn watchers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let (offset, limit) = page.window()?;
    let users = db::repositories::engagement::list_repo_watchers(
        &state.pool,
        repo.id,
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = users.len() > limit;
    let values = users.iter().take(limit).map(payloads::user).collect();
    page.response(&uri, values, has_next)
}
async fn collection(
    state: Arc<AppState>,
    headers: HeaderMap,
    username: Option<String>,
    page: Pagination,
    uri: Uri,
) -> Result<Response> {
    let viewer = auth::authenticate(&state, &headers, username.is_none()).await?;
    let user = match username {
        Some(name) => db::users::accounts::get_user_by_username(&state.pool, &name)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?,
        None => viewer.as_ref().ok_or_else(auth::required)?.user.clone(),
    };
    let (offset, limit) = page.window()?;
    let rows = db::repositories::engagement::list_watched_repositories(
        &state.pool,
        user.id,
        viewer.as_ref().map(|a| a.user.id),
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let mut items = Vec::new();
    let has_next = rows.len() > limit;
    for repo in rows.iter().take(limit) {
        let owner = db::repositories::records::repository_owner_name(&state.pool, repo.id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        items.push(payloads::repository(repo, &owner));
    }
    page.response(&uri, items, has_next)
}
async fn mine(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    collection(state, headers, None, page, uri).await
}
async fn user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    collection(state, headers, Some(username), page, uri).await
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("activity/get-repo-subscription", get(read)),
        ("activity/set-repo-subscription", put(set)),
        ("activity/delete-repo-subscription", delete(remove)),
        ("activity/list-watchers-for-repo", get(watchers)),
        (
            "activity/list-watched-repos-for-authenticated-user",
            get(mine),
        ),
        ("activity/list-repos-watched-by-user", get(user)),
    ]
}
