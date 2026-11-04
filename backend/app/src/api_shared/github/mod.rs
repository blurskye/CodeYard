use crate::app::AppState;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::Value;
use std::sync::Arc;
pub async fn repository(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    write: bool,
) -> Result<(
    codeyard_db_core::models::Repository,
    Option<codeyard_identity::github_auth::Authentication>,
)> {
    let auth = codeyard_identity::github_auth::authenticate(state, headers, write).await?;
    if write {
        auth.as_ref()
            .ok_or_else(codeyard_identity::github_auth::required)?
            .require_write()?;
    }
    let repo = codeyard_db_repos::repositories::records::get_repository(&state.pool, owner, name)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    if !codeyard_identity::repository_access::can_read(
        &state.pool,
        auth.as_ref().map(|auth| &auth.user),
        &repo,
    )
    .await
    .map_err(Error::unavailable)?
    {
        return Err(Error::missing());
    }
    Ok((repo, auth))
}

pub async fn get_repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    Ok(Json(
        crate::api_shared::github::repos::document(&state, &repo, auth.as_ref()).await?,
    ))
}

pub async fn missing() -> Error {
    Error::missing()
}

pub mod issues;

pub mod labels;

pub mod repos;
