//! Deploy-key writes retain live credentials and cross-account fingerprint uniqueness.
use super::{auth, pagination::Pagination, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db,
    repositories::deploy_keys::{self as keys, DeployKeyError},
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

fn key_error(error: DeployKeyError) -> Error {
    match error {
        DeployKeyError::Denied => {
            Error::new(StatusCode::FORBIDDEN, "Repository administration required")
        }
        DeployKeyError::NotFound => Error::missing(),
        DeployKeyError::Unavailable(_) => Error::unavailable(()),
        _ => Error::invalid(error.to_string()),
    }
}

async fn document(
    state: &AppState,
    owner: &str,
    name: &str,
    key: &db::models::DeployKey,
) -> Result<Value> {
    let added_by = match key.created_by_id {
        Some(id) => db::users::accounts::get_user_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .map(|user| user.username),
        None => None,
    };
    Ok(payloads::deploy_key(key, owner, name, added_by.as_deref()))
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let auth = auth.ok_or_else(auth::required)?;
    let keys = keys::list_keys(
        &state.pool,
        &auth.user,
        &repo,
        db::deploy_keys::Credential::Token(auth.token.id, auth.token.token_hash.clone()),
    )
    .await
    .map_err(key_error)?;
    let (offset, limit) = page.window()?;
    let next = keys.len() > offset + limit;
    let mut values = Vec::new();
    for key in keys.iter().skip(offset).take(limit) {
        values.push(document(&state, &owner, &name, key).await?);
    }
    page.response(&uri, values, next)
}

async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let auth = auth.ok_or_else(auth::required)?;
    let keys = keys::list_keys(
        &state.pool,
        &auth.user,
        &repo,
        db::deploy_keys::Credential::Token(auth.token.id, auth.token.token_hash.clone()),
    )
    .await
    .map_err(key_error)?;
    let key = keys
        .iter()
        .find(|key| key.id == id)
        .ok_or_else(Error::missing)?;
    Ok(Json(document(&state, &owner, &name, key).await?))
}

#[derive(Deserialize)]
struct Create {
    title: Option<String>,
    key: String,
    read_only: Option<bool>,
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Create>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(auth::required)?;
    // gh sends an empty title when none was given; GitHub treats that as no title.
    let title = input
        .title
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| {
            let comment = input
                .key
                .split_whitespace()
                .skip(2)
                .collect::<Vec<_>>()
                .join(" ");
            if comment.is_empty() {
                "Deploy key".to_owned()
            } else {
                comment
            }
        });
    let key = keys::add_key(
        &state.pool,
        &auth.user,
        db::deploy_keys::Credential::Token(auth.token.id, auth.token.token_hash.clone()),
        &repo,
        &title,
        &input.key,
        !input.read_only.unwrap_or(true),
    )
    .await
    .map_err(key_error)?;
    Ok((
        StatusCode::CREATED,
        Json(document(&state, &owner, &name, &key).await?),
    )
        .into_response())
}

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(auth::required)?;
    keys::remove_key(
        &state.pool,
        &auth.user,
        db::deploy_keys::Credential::Token(auth.token.id, auth.token.token_hash.clone()),
        &repo,
        id,
    )
    .await
    .map_err(key_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/list-deploy-keys", get(list)),
        ("repos/get-deploy-key", get(read)),
        ("repos/create-deploy-key", post(create)),
        ("repos/delete-deploy-key", delete(remove)),
    ]
}
