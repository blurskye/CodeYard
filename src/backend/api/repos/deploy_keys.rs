//! Deploy keys over JSON, and the settings section's forms. Both call the same checks in
//! `repositories::deploy_keys`, and both need repository admin access.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::chrome::Loaded;
use crate::backend::api::repos::repository::{checkbox, field, settings_model, settings_response};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::db::models::{DeployKey, Repository, User};
use crate::backend::db::users::ssh_keys::SshKeyWriteRefusal;
use crate::backend::repositories::deploy_keys as keys;
use crate::backend::repositories::deploy_keys::DeployKeyError;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Response,
    routing::{delete, get},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::error;

#[derive(Serialize)]
pub struct DeployKeyResponse {
    pub id: i64,
    pub title: String,
    pub fingerprint: String,
    pub key_type: String,
    pub can_write: bool,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

impl From<DeployKey> for DeployKeyResponse {
    fn from(key: DeployKey) -> Self {
        Self {
            id: key.id,
            title: key.title,
            fingerprint: key.fingerprint,
            key_type: crate::backend::repositories::deploy_keys::key_type(&key.public_key),
            can_write: key.can_write,
            created_at: key.created_at,
            last_used_at: key.last_used_at,
        }
    }
}

#[derive(Deserialize)]
pub struct CreateDeployKeyRequest {
    pub title: String,
    pub key: String,
    #[serde(default)]
    pub can_write: bool,
}

/// The HTTP status and stable code for a refused key operation. The JSON and form paths share it.
fn refusal(error: &DeployKeyError) -> (StatusCode, &'static str) {
    match error {
        DeployKeyError::Denied => (StatusCode::FORBIDDEN, "repository_admin_required"),
        DeployKeyError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        DeployKeyError::Key(SshKeyWriteRefusal::AlreadyUsed) => {
            (StatusCode::CONFLICT, "key_in_use")
        }
        DeployKeyError::InvalidTitle | DeployKeyError::Key(_) => {
            (StatusCode::BAD_REQUEST, "validation_error")
        }
        DeployKeyError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "database_unavailable"),
    }
}

fn json_error(error: DeployKeyError) -> (StatusCode, Json<ApiError>) {
    let (status, code) = refusal(&error);
    if let DeployKeyError::Unavailable(cause) = &error {
        error!(error = %cause, "deploy key storage failed");
    }
    (status, Json(ApiError::new(code, &error.to_string())))
}

async fn repository_for(
    state: &AppState,
    owner: &str,
    repo: &str,
) -> Result<Repository, (StatusCode, Json<ApiError>)> {
    db::repositories::records::get_repository(&state.pool, owner, repo)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError::new(
                    "database_unavailable",
                    "Repository storage is temporarily unavailable",
                )),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })
}

pub async fn list_keys(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<Vec<DeployKeyResponse>>, (StatusCode, Json<ApiError>)> {
    let actor = get_current_user(&state.pool, &headers).await?;
    let repository = repository_for(&state, &owner, &repo).await?;
    let keys = keys::list_keys(&state.pool, &actor, &repository)
        .await
        .map_err(json_error)?;
    Ok(Json(
        keys.into_iter().map(DeployKeyResponse::from).collect(),
    ))
}

pub async fn create_key(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<CreateDeployKeyRequest>,
) -> Result<(StatusCode, Json<DeployKeyResponse>), (StatusCode, Json<ApiError>)> {
    let actor = get_current_user(&state.pool, &headers).await?;
    let repository = repository_for(&state, &owner, &repo).await?;
    let key = keys::add_key(
        &state.pool,
        &actor,
        &repository,
        &req.title,
        &req.key,
        req.can_write,
    )
    .await
    .map_err(json_error)?;
    Ok((StatusCode::CREATED, Json(key.into())))
}

pub async fn delete_key(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path((owner, repo, key_id)): Path<(String, String, i64)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let actor = get_current_user(&state.pool, &headers).await?;
    let repository = repository_for(&state, &owner, &repo).await?;
    keys::remove_key(&state.pool, &actor, &repository, key_id)
        .await
        .map_err(json_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn json_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/repos/:owner/:repo/keys",
            get(list_keys).post(create_key),
        )
        .route("/api/repos/:owner/:repo/keys/:key_id", delete(delete_key))
}

/// The settings section's panel, built for the viewer. Its forms post back to the repository.
pub(super) async fn settings_section(
    ctx: &PageContext,
    loaded: &Loaded,
    viewer: &User,
) -> Result<Box<crate::webui::components::deploy_keys::DeployKeyPanel>, Response> {
    let base = format!("{}/settings?tab=deploy-keys", loaded.chrome.base());
    let action = format!("{}/_forms", loaded.chrome.base());
    let place = keys::Place {
        base: &base,
        action: &action,
        csrf_token: &loaded.chrome.csrf_token,
    };
    keys::settings_panel(
        &ctx.state.pool,
        viewer,
        &loaded.repository,
        place,
        ctx.query.get("notice").map(String::as_str),
    )
    .await
    .map(Box::new)
    .map_err(|error| settings_error(ctx, loaded, &error))
}

fn settings_error(ctx: &PageContext, loaded: &Loaded, error: &DeployKeyError) -> Response {
    let (status, _) = refusal(error);
    ctx.repo_error(&loaded.chrome, status, &error.to_string())
}

/// The section's forms: `deploy-key-add` and `deploy-key-delete`. A saved or removed key returns to
/// the section with a notice. A refused add shows the section again with the submitted values.
pub(super) async fn form(
    ctx: &PageContext,
    loaded: &Loaded,
    action: &str,
    form: &HashMap<String, String>,
) -> Response {
    let actor = match ctx.require_user() {
        Ok(user) => user,
        Err(response) => return response,
    };
    let pool = &ctx.state.pool;
    let repository = &loaded.repository;
    let base = format!("{}/settings?tab=deploy-keys", loaded.chrome.base());
    let outcome = if action == "deploy-key-add" {
        keys::add_key(
            pool,
            actor,
            repository,
            &field(form, "title"),
            &field(form, "key"),
            checkbox(form, "can_write"),
        )
        .await
        .map(|_| "added")
    } else {
        match field(form, "key_id").parse::<i64>() {
            Ok(key_id) => keys::remove_key(pool, actor, repository, key_id)
                .await
                .map(|()| "deleted"),
            Err(_) => Err(DeployKeyError::NotFound),
        }
    };
    match outcome {
        Ok(notice) => ctx.redirect(&format!("{base}&notice={notice}")),
        Err(error) => {
            let (status, _) = refusal(&error);
            let mut settings = match settings_model(ctx, loaded, "deploy-keys").await {
                Ok(settings) => settings,
                Err(response) => return response,
            };
            if let Some(panel) = settings.deploy_keys.as_mut() {
                panel.title = field(form, "title");
                panel.public_key = field(form, "key");
                panel.can_write = checkbox(form, "can_write");
                panel.error = Some(error.to_string());
            }
            settings_response(ctx, loaded, settings, status)
        }
    }
}
