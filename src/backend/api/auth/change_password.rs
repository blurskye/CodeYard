use crate::backend::api::auth::errors;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::extract_session_id;
use crate::backend::api::user::profile::{settings_response, settings_result};
use crate::backend::api::user::responses::{
    field, mutation_guard, within_bounds, AuthResponse, AuthUserResponse as UserResponse, Fields,
};
use crate::backend::api::{auth, http::session};
use crate::backend::app::AppState;
use crate::backend::identity;
use crate::backend::identity::accounts::PasswordChange;
use axum::extract::{Form, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

pub async fn change_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let session_id = extract_session_id(&headers);
    let user = identity::accounts::change_password(
        &state.pool,
        session_id.as_deref(),
        PasswordChange {
            current_password: &req.current_password,
            new_password: &req.new_password,
        },
    )
    .await
    .map_err(errors::password_change_failure)?;
    // Every session is revoked, including this one, so the cookie goes too.
    Ok((
        [(header::SET_COOKIE, session::build_logout_cookie())],
        Json(AuthResponse {
            success: true,
            user: Some(UserResponse::from(user)),
            message: Some(
                "Password updated. Sign in again; prior sessions were revoked.".to_owned(),
            ),
        }),
    ))
}

pub(crate) async fn password_post(
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
    if !within_bounds(&form) || field(&form, "new_password") != field(&form, "confirm_password") {
        return settings_response(
            &ctx,
            "password",
            Some(&form),
            StatusCode::BAD_REQUEST,
            Some("Passwords must match and stay within the supported length."),
        )
        .await;
    }
    let session_id = session::extract_session_id(&ctx.headers);
    let result = identity::accounts::change_password(
        &ctx.state.pool,
        session_id.as_deref(),
        PasswordChange {
            current_password: field(&form, "current_password"),
            new_password: field(&form, "new_password"),
        },
    )
    .await
    .map_err(auth::errors::password_change_failure);
    match result {
        Ok(_) => {
            // Changing credentials revokes every session, including this one.
            let mut response = ctx.redirect("/login");
            let Ok(cookie) = HeaderValue::from_str(&session::build_logout_cookie()) else {
                return ctx.error(StatusCode::INTERNAL_SERVER_ERROR, "Your password changed, but the logout cookie could not be issued. Sign in again.");
            };
            response.headers_mut().append(header::SET_COOKIE, cookie);
            response
        }
        Err(error) => {
            settings_result(&ctx, "password", &form, Err::<(), _>(error), "password").await
        }
    }
}
