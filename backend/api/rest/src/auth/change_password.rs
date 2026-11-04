use crate::user::profile::settings_response;
use crate::user::profile::settings_result;
use crate::user::responses::mutation_guard;
use crate::user::responses::within_bounds;
use axum::extract::Form;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::user::responses::field;
use codeyard_api_http::shared::user::responses::Fields;
use codeyard_app::app::AppState;
use codeyard_identity::accounts::PasswordChange;
use std::sync::Arc;

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
    let session_id = codeyard_api_http::session::extract_session_id(&ctx.headers);
    let result = codeyard_identity::accounts::change_password(
        &ctx.state.pool,
        session_id.as_deref(),
        PasswordChange {
            current_password: field(&form, "current_password"),
            new_password: field(&form, "new_password"),
        },
    )
    .await
    .map_err(crate::auth::errors::password_change_failure);
    match result {
        Ok(_) => {
            // Changing credentials revokes every session, including this one.
            let mut response = ctx.redirect("/login");
            let Ok(cookie) =
                HeaderValue::from_str(&codeyard_api_http::session::build_logout_cookie())
            else {
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
