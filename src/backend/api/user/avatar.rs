//! Upload or remove a user's picture through native settings forms.

use crate::backend::api::http::context::PageContext;

use crate::backend::api::user::profile::settings_response;
use crate::backend::api::user::responses::require_account;
use crate::backend::api::user::responses::Fields;
use crate::backend::app::AppState;
use codeyard_db_core::queries::users::accounts::AccountCredential;

use crate::backend::identity::avatars::AvatarError;
use axum::extract::multipart::MultipartRejection;
use axum::extract::{DefaultBodyLimit, Form, Multipart, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Router;
use std::sync::Arc;

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

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/settings/avatar",
            crate::backend::api::user::back_to_settings("profile")
                .post(upload)
                .layer(DefaultBodyLimit::max(
                    crate::backend::api::pictures::BODY_LIMIT,
                )),
        )
        .route(
            "/settings/avatar/remove",
            crate::backend::api::user::back_to_settings("profile")
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
    let form = match crate::backend::api::pictures::read(multipart).await {
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
    let session =
        crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let credential = AccountCredential::Session(&session);
    match crate::backend::identity::avatars::upload_user_avatar(
        &ctx.state.pool,
        &ctx.state.storage,
        &user,
        &credential,
        bytes,
    )
    .await
    {
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
    let session =
        crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    match crate::backend::identity::avatars::remove_user_avatar(
        &ctx.state.pool,
        &user,
        &AccountCredential::Session(&session),
    )
    .await
    {
        Ok(()) => ctx.redirect("/settings?tab=profile&saved=avatar-removed"),
        Err(error) => refused(&ctx, avatar_status(&error).0, &error.to_string()).await,
    }
}
