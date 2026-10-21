//! The account's signed-in sessions: the list on the settings page, ending one, and ending
//! every session except the one making the request.

use crate::backend::api::auth::errors::database_failure;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::{ApiError, ApiFailure};
use crate::backend::api::http::session;
use crate::backend::api::user::profile::settings_result;
use crate::backend::api::user::responses::{field, mutation_guard, Fields};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::webui::pages::settings::AccountSession;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// The form value that names a session. The session id is the sign-in secret, so pages show
/// only its SHA-256 digest, and the server finds the session by digesting its own rows.
fn session_key(id: &str) -> String {
    format!("{:x}", Sha256::digest(id.as_bytes()))
}

/// The sessions the settings page lists for `user_id`, with the current one marked.
pub(crate) async fn load_sessions(
    pool: &db::DbPool,
    user_id: i64,
    current: Option<&str>,
) -> Result<Vec<AccountSession>, ApiFailure> {
    let sessions = db::users::sessions::list_user_sessions(pool, user_id)
        .await
        .map_err(|_| database_failure())?;
    Ok(sessions
        .into_iter()
        .map(|session| AccountSession {
            key: session_key(&session.id),
            current: current == Some(session.id.as_str()),
            created: session.created_at.format("%b %e, %Y %H:%M UTC").to_string(),
            expires: session.expires_at.format("%b %e, %Y").to_string(),
            user_agent: session.user_agent.unwrap_or_default(),
            ip_address: session.ip_address.unwrap_or_default(),
        })
        .collect())
}

pub(crate) async fn revoke_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, false) {
        return response;
    }
    let actor = ctx
        .user
        .as_ref()
        .expect("mutation guard checked account")
        .id;
    let result = match revoke_one(&ctx, actor, field(&form, "session")).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(not_found()),
        Err(error) => Err(error),
    };
    settings_result(&ctx, "sessions", &form, result, "sessions-revoked").await
}

async fn revoke_one(ctx: &PageContext, actor: i64, key: &str) -> Result<bool, ApiFailure> {
    let sessions = db::users::sessions::list_user_sessions(&ctx.state.pool, actor)
        .await
        .map_err(|_| database_failure())?;
    let Some(target) = sessions
        .into_iter()
        .find(|session| session_key(&session.id) == key)
    else {
        return Ok(false);
    };
    db::users::sessions::revoke_user_session(&ctx.state.pool, actor, &target.id)
        .await
        .map_err(|_| database_failure())
}

pub(crate) async fn revoke_others_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, false) {
        return response;
    }
    let actor = ctx
        .user
        .as_ref()
        .expect("mutation guard checked account")
        .id;
    let result = match session::extract_session_id(&ctx.headers) {
        Some(keep) => {
            db::users::sessions::revoke_other_user_sessions(&ctx.state.pool, actor, &keep)
                .await
                .map(|_| ())
                .map_err(|_| database_failure())
        }
        None => Err(not_found()),
    };
    settings_result(&ctx, "sessions", &form, result, "sessions-revoked").await
}

fn not_found() -> ApiFailure {
    (
        StatusCode::NOT_FOUND,
        Json(ApiError::new("not_found", "Session not found")),
    )
}
