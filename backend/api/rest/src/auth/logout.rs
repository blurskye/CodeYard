use axum::extract::Form;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::user::responses::Fields;
use codeyard_app::app::AppState;
use codeyard_webui::pages::account;
use std::sync::Arc;

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
        codeyard_webui::app::Page::Identity(account::Page::Logout {
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
    let id = codeyard_api_http::session::extract_local_session_id(&ctx.state.pool, &ctx.headers);
    if codeyard_identity::sessions::end_session(&ctx.state.pool, id.as_deref())
        .await
        .is_err()
    {
        return ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to end your session. Please try again when the account service is available.",
        );
    }
    let mut response = ctx.redirect("/login");
    match HeaderValue::from_str(&codeyard_api_http::session::build_logout_cookie()) {
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
