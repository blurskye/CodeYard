use crate::backend::api::auth::errors::session_failure;
use crate::backend::api::context::PageContext;
use crate::backend::api::error::ApiError;
use crate::backend::api::session;
use crate::backend::api::session::{build_logout_cookie, extract_session_id};
use crate::backend::api::user::responses::{AuthResponse, Fields};
use crate::backend::app::AppState;
use crate::backend::identity;
use crate::webui::pages::identity as view;
use axum::extract::{Form, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::sync::Arc;

pub async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let id = extract_session_id(&headers);
    identity::sessions::end_session(&state.pool, id.as_deref())
        .await
        .map_err(|_| session_failure())?;
    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, build_logout_cookie())],
        Json(AuthResponse {
            success: true,
            user: None,
            message: Some("Logged out successfully".to_owned()),
        }),
    ))
}

pub(crate) async fn logout_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    ctx.page_model(
        "Sign out",
        crate::webui::app::Page::Identity(view::Page::Logout {
            csrf_token: ctx.csrf_token.clone(),
        }),
    )
}
pub(crate) async fn logout_post(
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
    let id = session::extract_session_id(&ctx.headers);
    if identity::sessions::end_session(&ctx.state.pool, id.as_deref())
        .await
        .is_err()
    {
        return ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to end your session. Please try again when the account service is available.",
        );
    }
    let mut response = ctx.redirect("/login");
    match HeaderValue::from_str(&session::build_logout_cookie()) {
        Ok(cookie) => {
            response.headers_mut().append(header::SET_COOKIE, cookie);
            response
        }
        Err(_) => ctx.error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to clear the session cookie.",
        ),
    }
}
