use crate::activity::engagement_error;
use crate::empty;
use axum::body::Bytes;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

fn document(owner: &str, name: &str, watch: &codeyard_db_core::models::Watch) -> Value {
    let url = codeyard_model::urls::repo_url(owner, name);
    json!({"subscribed":matches!(watch.level.as_str(),"watching"|"custom"),"ignored":watch.level=="ignoring","reason":null,"created_at":watch.created_at,"url":format!("{url}/subscription"),"repository_url":url})
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
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?
        .user;
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let watch =
        codeyard_db_repos::repositories::engagement::get_repo_watch(&state.pool, actor.id, repo.id)
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
    let actor = viewer.ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    let input: Subscription = crate::optional_json::decode(&body)
        .map_err(|_| Error::invalid("Invalid subscription"))?
        .unwrap_or_default();
    let subscribed = input.subscribed.unwrap_or(false);
    let ignored = input.ignored.unwrap_or(false);
    if subscribed && ignored {
        return Err(Error::invalid(
            "A subscription cannot be both subscribed and ignored",
        ));
    }
    codeyard_repos_pulls::repositories::engagement::set_subscription(
        &state.pool,
        &state.notification_broadcaster,
        actor.database_actor(),
        repo.id,
        subscribed,
        ignored,
    )
    .await
    .map_err(engagement_error)?;
    let watch = codeyard_db_repos::repositories::engagement::get_repo_watch(
        &state.pool,
        actor.user.id,
        repo.id,
    )
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
    let actor = viewer.ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    codeyard_repos_pulls::repositories::engagement::set_level(
        &state.pool,
        actor.database_actor(),
        repo.id,
        "participating",
        &[],
    )
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
    let users = codeyard_db_repos::repositories::engagement::list_repo_watchers(
        &state.pool,
        repo.id,
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = users.len() > limit;
    let values = users
        .iter()
        .take(limit)
        .map(codeyard_webhooks::payloads::user)
        .collect();
    page.response(&uri, values, has_next)
}
async fn collection(
    state: Arc<AppState>,
    headers: HeaderMap,
    username: Option<String>,
    page: Pagination,
    uri: Uri,
) -> Result<Response> {
    let viewer =
        codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, username.is_none())
            .await?;
    let user = match username {
        Some(name) => {
            codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, &name)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?
        }
        None => viewer
            .as_ref()
            .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?
            .user
            .clone(),
    };
    let (offset, limit) = page.window()?;
    let rows = codeyard_db_repos::repositories::engagement::list_watched_repositories(
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
    let readable = codeyard_repos_core::api_shared::github::auth::readable_repositories(
        &state,
        viewer.as_ref(),
        &rows.iter().map(|repo| repo.id).collect::<Vec<_>>(),
        "metadata",
    )
    .await?;
    for repo in rows
        .iter()
        .take(limit)
        .filter(|repo| readable.contains(&repo.id))
    {
        items.push(
            codeyard_app::api_shared::github::repos::document(&state, repo, viewer.as_ref())
                .await?,
        );
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
