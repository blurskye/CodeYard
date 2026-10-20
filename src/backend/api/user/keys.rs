use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::{extract_session_id, get_current_user};
use crate::backend::api::user::profile::{settings_response, settings_result};
use crate::backend::api::user::responses::{field, mutation_guard, within_bounds, Fields};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::identity::keys;
use crate::backend::identity::keys::KeyError;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize)]
pub struct SshKeyResponse {
    pub id: i64,
    pub name: String,
    pub fingerprint: String,
    pub key_type: String,
    pub created_at: String,
}

impl From<db::models::SshKey> for SshKeyResponse {
    fn from(key: db::models::SshKey) -> Self {
        Self {
            id: key.id,
            name: key.name,
            fingerprint: key.fingerprint,
            key_type: key.key_type,
            created_at: key.created_at.to_rfc3339(),
        }
    }
}

impl From<db::users::ssh_keys::SshKeySummary> for SshKeyResponse {
    fn from(key: db::users::ssh_keys::SshKeySummary) -> Self {
        Self {
            id: key.id,
            name: key.name,
            fingerprint: key.fingerprint,
            key_type: key.key_type,
            created_at: key.created_at.to_rfc3339(),
        }
    }
}

#[derive(Deserialize)]
pub struct AddSshKeyRequest {
    pub name: String,
    pub public_key: String,
}

pub(crate) fn key_failure(error: KeyError) -> (StatusCode, Json<ApiError>) {
    let (status, code) = match error {
        KeyError::InvalidName | KeyError::InvalidKey => {
            (StatusCode::BAD_REQUEST, "validation_error")
        }
        KeyError::AlreadyUsed => (StatusCode::CONFLICT, "key_already_registered"),
        KeyError::AccountRestricted | KeyError::InvalidSession => {
            (StatusCode::FORBIDDEN, "forbidden")
        }
        KeyError::Missing => (StatusCode::NOT_FOUND, "not_found"),
        KeyError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    };
    (status, Json(ApiError::new(code, &error.to_string())))
}

#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
fn key_mutation_actor(ctx: &PageContext, form: &Fields) -> Result<i64, Response> {
    mutation_guard(ctx, form, false)?;
    let user = ctx.user.as_ref().expect("mutation guard checked account");
    if user.must_change_password {
        return Err(ctx.error(
            StatusCode::FORBIDDEN,
            "Change your temporary password before managing SSH keys.",
        ));
    }
    Ok(user.id)
}

pub async fn list_ssh_keys(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<Vec<SshKeyResponse>>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let rows = db::users::ssh_keys::list_user_key_summaries(
        &state.pool,
        user.id,
        query.get("after").and_then(|v| v.parse().ok()).unwrap_or(0),
    )
    .await
    .map_err(|_| key_failure(KeyError::Unavailable))?;
    Ok(Json(rows.into_iter().map(SshKeyResponse::from).collect()))
}

pub async fn add_ssh_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<AddSshKeyRequest>,
) -> Result<(StatusCode, Json<SshKeyResponse>), (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let session_id = extract_session_id(&headers);
    let key = keys::enroll(
        &state.pool,
        user.id,
        session_id.as_deref(),
        &req.name,
        &req.public_key,
    )
    .await
    .map_err(key_failure)?;
    Ok((StatusCode::CREATED, Json(SshKeyResponse::from(key))))
}

pub async fn delete_ssh_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    keys::delete(&state.pool, user.id, id)
        .await
        .map_err(key_failure)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn key_add_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let actor = match key_mutation_actor(&ctx, &form) {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    if !within_bounds(&form) {
        return settings_response(
            &ctx,
            "keys",
            Some(&form),
            StatusCode::BAD_REQUEST,
            Some("The SSH key or its title exceeds the supported length."),
        )
        .await;
    }
    let public_key = field(&form, "public_key").trim();
    if public_key.contains("PRIVATE KEY") {
        // Do not echo a mistakenly pasted private key back to the browser.
        let mut safe = form.clone();
        safe.remove("public_key");
        return settings_response(
            &ctx,
            "keys",
            Some(&safe),
            StatusCode::BAD_REQUEST,
            Some("Paste only your public key, never a private key."),
        )
        .await;
    }
    let session_id = extract_session_id(&ctx.headers);
    let result = keys::enroll(
        &ctx.state.pool,
        actor,
        session_id.as_deref(),
        field(&form, "name").trim(),
        public_key,
    )
    .await
    .map_err(key_failure);
    settings_result(&ctx, "keys", &form, result, "key").await
}

pub(crate) async fn key_delete_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(id): Path<i64>,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let actor = match key_mutation_actor(&ctx, &form) {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let result = keys::delete(&ctx.state.pool, actor, id)
        .await
        .map_err(key_failure);
    settings_result(&ctx, "keys", &form, result, "deleted").await
}
