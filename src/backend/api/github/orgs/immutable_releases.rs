//! Immutable release enforcement settings are owned by organization owners.
use super::super::{api_url, pagination::Pagination, repos};
use super::{auth, outcome, target, AppState, Error, Result};
use crate::backend::db::organizations::immutable_releases::{self as data, Change};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
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
        data::get(&state.pool, actor.user.id, org.id, 0, 0)
            .await
            .map_err(Error::unavailable)?,
    )?;
    Ok(Json(
        json!({"enforced_repositories": settings.enforced_repositories,
        "selected_repositories_url": api_url(&format!("/orgs/{}/settings/immutable-releases/repositories", crate::webui::encode_component(&name)))}),
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
    let settings = outcome(
        data::get(&state.pool, actor.user.id, org.id, offset as i64, limit as i64)
            .await
            .map_err(Error::unavailable)?,
    )?;
    let mut repositories = Vec::new();
    for repo in &settings.repositories {
        repositories.push(repos::document(&state, repo, Some(&actor)).await?);
    }
    Ok((
        page.headers(&uri, settings.total > (offset + limit) as i64)?,
        Json(json!({"total_count":settings.total,"repositories":repositories})),
    )
        .into_response())
}
