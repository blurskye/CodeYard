//! Immutable release enforcement settings are owned by organization owners.
use super::immutable_releases as data;
use crate::auth;
use crate::orgs::outcome;
use crate::orgs::target;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_orgs::organizations::immutable_releases::Change;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::api_url;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("orgs/get-immutable-releases-settings", get(get_settings)),
        ("orgs/set-immutable-releases-settings", put(set_settings)),
        (
            "orgs/get-immutable-releases-settings-repositories",
            get(repositories),
        ),
        (
            "orgs/set-immutable-releases-settings-repositories",
            put(set_repositories),
        ),
        (
            "orgs/enable-selected-repository-immutable-releases-organization",
            put(enable),
        ),
        (
            "orgs/disable-selected-repository-immutable-releases-organization",
            delete(disable),
        ),
    ]
}

async fn get_settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    let settings = outcome(
        data::get(&state.pool, actor.user.id, org.id)
            .await
            .map_err(Error::unavailable)?,
    )?;
    Ok(Json(
        json!({"enforced_repositories": settings.enforced_repositories,
        "selected_repositories_url": api_url(&format!("/orgs/{}/settings/immutable-releases/repositories", codeyard_webui::encode_component(&name)))}),
    ))
}

#[derive(Deserialize)]
struct SettingsInput {
    enforced_repositories: String,
    selected_repository_ids: Option<Vec<i64>>,
}
#[derive(Deserialize)]
struct RepositoriesInput {
    selected_repository_ids: Vec<i64>,
}

async fn change(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    change: Change,
) -> Result<StatusCode> {
    let actor = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let org = target(state, name).await?;
    outcome(
        data::change(&state.pool, actor.user.id, org.id, &change)
            .await
            .map_err(Error::unavailable)?,
    )?;
    Ok(StatusCode::NO_CONTENT)
}
async fn set_settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(input): Json<SettingsInput>,
) -> Result<StatusCode> {
    change(
        &state,
        &headers,
        &name,
        Change::Enforcement(input.enforced_repositories, input.selected_repository_ids),
    )
    .await
}
async fn set_repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(input): Json<RepositoriesInput>,
) -> Result<StatusCode> {
    change(
        &state,
        &headers,
        &name,
        Change::Repositories(input.selected_repository_ids),
    )
    .await
}
async fn enable(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, i64)>,
) -> Result<StatusCode> {
    change(&state, &headers, &name, Change::Repository(id, true)).await
}
async fn disable(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, i64)>,
) -> Result<StatusCode> {
    change(&state, &headers, &name, Change::Repository(id, false)).await
}
async fn repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    let (offset, limit) = page.window()?;
    let (count, rows) = outcome(
        data::repositories(
            &state.pool,
            actor.user.id,
            org.id,
            offset as i64,
            limit as i64,
        )
        .await
        .map_err(Error::unavailable)?,
    )?;
    let repositories: Vec<_> = rows
        .iter()
        .map(codeyard_app::webhooks::payloads::repository_snapshot)
        .collect();
    Ok((
        page.headers(&uri, count > (offset + limit) as i64)?,
        Json(json!({"total_count":count,"repositories":repositories})),
    )
        .into_response())
}
