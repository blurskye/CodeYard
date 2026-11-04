//! The account's signed-in sessions: the list on the settings page, ending one, and ending
//! every session except the one making the request.

use crate::backend::api::auth::errors::database_failure;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::error::ApiFailure;

use crate::backend::api::user::profile::settings_result;
use crate::backend::api::user::responses::field;
use crate::backend::api::user::responses::mutation_guard;
use crate::backend::api::user::responses::Fields;
use crate::backend::app::AppState;

use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use codeyard_webui::pages::settings::{AccountSession, SessionPagination};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// The form value that names a session. The session id is the sign-in secret, so pages show
/// only its SHA-256 digest, and the server finds the session by digesting its own rows.
fn session_key(id: &str) -> String {
    format!("{:x}", Sha256::digest(id.as_bytes()))
}

#[derive(Default)]
pub(crate) struct SessionListing {
    pub items: Vec<AccountSession>,
    pub pagination: SessionPagination,
}

fn session_failure(error: anyhow::Error) -> ApiFailure {
    if error.is::<codeyard_support::credentials::AccountRestricted>() {
        (
            StatusCode::FORBIDDEN,
            Json(ApiError::new(
                "forbidden",
                "A live unrestricted account session is required.",
            )),
        )
    } else {
        database_failure()
    }
}

/// One authorized page, with an upper cursor retained in each revocation form.
pub(crate) async fn load_sessions(
    pool: &codeyard_db_core::DbPool,
    user_id: i64,
    current: Option<&str>,
    before: i64,
) -> Result<SessionListing, ApiFailure> {
    let mut sessions = codeyard_db_identity::users::sessions::session_page(
        pool,
        user_id,
        current.unwrap_or_default(),
        before,
    )
    .await
    .map_err(session_failure)?;
    let more = sessions.len() > 100;
    sessions.truncate(100);
    let pagination = SessionPagination {
        before: sessions.first().map_or(before.max(0), |row| row.ordinal),
        next_before: more
            .then(|| sessions.last().map(|row| row.ordinal.saturating_sub(1)))
            .flatten(),
    };
    let items = sessions
        .into_iter()
        .map(|row| row.session)
        .map(|session| AccountSession {
            key: session_key(&session.id),
            current: current == Some(session.id.as_str()),
            created: session.created_at.format("%b %e, %Y %H:%M UTC").to_string(),
            expires: session.expires_at.format("%b %e, %Y").to_string(),
            user_agent: session.user_agent.unwrap_or_default(),
            ip_address: session.ip_address.unwrap_or_default(),
        })
        .collect();
    Ok(SessionListing { items, pagination })
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
    let before = field(&form, "sessions_before")
        .parse::<i64>()
        .unwrap_or(0)
        .max(0);
    let result = match revoke_one(&ctx, actor, field(&form, "session"), before).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(not_found()),
        Err(error) => Err(error),
    };
    if result.is_ok() {
        return ctx.redirect(&format!(
            "/settings?tab=sessions&sessions_before={before}&saved=sessions-revoked"
        ));
    }
    settings_result(&ctx, "sessions", &form, result, "sessions-revoked").await
}

async fn revoke_one(
    ctx: &PageContext,
    actor: i64,
    key: &str,
    before: i64,
) -> Result<bool, ApiFailure> {
    let Some(current) = crate::backend::api::http::session::extract_session_id(&ctx.headers) else {
        return Ok(false);
    };
    let sessions = codeyard_db_identity::users::sessions::session_page(
        &ctx.state.pool,
        actor,
        &current,
        before,
    )
    .await
    .map_err(session_failure)?;
    let Some(target) = sessions
        .into_iter()
        .take(100)
        .find(|row| session_key(&row.session.id) == key)
    else {
        return Ok(false);
    };
    codeyard_db_identity::users::sessions::revoke_user_session(
        &ctx.state.pool,
        actor,
        &target.session.id,
        &current,
    )
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
    let result = match crate::backend::api::http::session::extract_session_id(&ctx.headers) {
        Some(keep) => codeyard_db_identity::users::sessions::revoke_other_user_sessions(
            &ctx.state.pool,
            actor,
            &keep,
        )
        .await
        .map(|_| ())
        .map_err(|_| database_failure()),
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
