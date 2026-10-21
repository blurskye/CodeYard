//! A user's own picture through the JSON API: point it at one of their confirmed
//! `avatar` uploads, or take it away.

use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::{ApiError, ApiFailure};
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::pictures;
use crate::backend::api::user::profile::settings_response;
use crate::backend::api::user::responses::{
    require_account, Fields, PublicUserResponse as UserResponse,
};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::identity::avatars;
use crate::backend::identity::avatars::AvatarError;
use axum::extract::multipart::MultipartRejection;
use axum::extract::{DefaultBodyLimit, Form, Multipart, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::{Json, Router};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetAvatarRequest {
    pub file_id: Uuid,
}

/// The HTTP status and error code for a refused picture change.
pub fn avatar_status(error: &AvatarError) -> (StatusCode, &'static str) {
    match error {
        AvatarError::BadPicture => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_picture"),
        AvatarError::NotUsable => (StatusCode::UNPROCESSABLE_ENTITY, "unusable_file"),
        AvatarError::TooMany => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
        AvatarError::Denied => (StatusCode::FORBIDDEN, "forbidden"),
        AvatarError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        AvatarError::Suspended => (StatusCode::CONFLICT, "governance_suspended"),
        AvatarError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    }
}

fn failure(error: AvatarError) -> ApiFailure {
    let (status, code) = avatar_status(&error);
    (status, Json(ApiError::new(code, &error.to_string())))
}

pub async fn set_avatar(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(request): Json<SetAvatarRequest>,
) -> Result<Json<UserResponse>, ApiFailure> {
    let user = get_current_user(&state.pool, &headers).await?;
    avatars::set_user_avatar(&state.pool, &user, request.file_id)
        .await
        .map_err(failure)?;
    let updated = db::users::accounts::get_user_by_id(&state.pool, user.id)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError::new(
                    "unavailable",
                    "Picture changed, but the updated profile is temporarily unavailable",
                )),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError::new(
                    "unavailable",
                    "Picture changed, but the updated profile is temporarily unavailable",
                )),
            )
        })?;
    Ok(Json(UserResponse::from(updated)))
}

pub async fn remove_avatar(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Result<StatusCode, ApiFailure> {
    let user = get_current_user(&state.pool, &headers).await?;
    avatars::remove_user_avatar(&state.pool, &user)
        .await
        .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/settings/avatar",
            super::back_to_settings("profile")
                .post(upload)
                .layer(DefaultBodyLimit::max(pictures::BODY_LIMIT)),
        )
        .route(
            "/settings/avatar/remove",
            super::back_to_settings("profile")
                .post(remove_form)
                .layer(DefaultBodyLimit::max(16 * 1024)),
        )
}

/// The settings page again, with what went wrong.
async fn refused(ctx: &PageContext, status: StatusCode, message: &str) -> Response {
    settings_response(ctx, "profile", None, status, Some(message)).await
}

async fn upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let user = match require_account(&ctx, false) {
        Ok(user) => user.clone(),
        Err(response) => return response,
    };
    let Ok(multipart) = multipart else {
        return refused(
            &ctx,
            StatusCode::BAD_REQUEST,
            "Send the picture from the settings form.",
        )
        .await;
    };
    let form = match pictures::read(multipart).await {
        Ok(form) => form,
        Err(error) => return refused(&ctx, error.status(), error.message()).await,
    };
    if let Err(response) = ctx.check_csrf(&form.fields) {
        return response;
    }
    let Some(bytes) = form.picture else {
        return refused(
            &ctx,
            StatusCode::UNPROCESSABLE_ENTITY,
            "Choose a picture to upload.",
        )
        .await;
    };
    match avatars::upload_user_avatar(&ctx.state.pool, &ctx.state.storage, &user, bytes).await {
        Ok(()) => ctx.redirect("/settings?tab=profile&saved=avatar"),
        Err(error) => refused(&ctx, avatar_status(&error).0, &error.to_string()).await,
    }
}

async fn remove_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let user = match require_account(&ctx, false) {
        Ok(user) => user.clone(),
        Err(response) => return response,
    };
    match avatars::remove_user_avatar(&ctx.state.pool, &user).await {
        Ok(()) => ctx.redirect("/settings?tab=profile&saved=avatar-removed"),
        Err(error) => refused(&ctx, avatar_status(&error).0, &error.to_string()).await,
    }
}
