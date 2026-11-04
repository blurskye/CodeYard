//! The second step of password sign-in: `/login/two-factor`. The password step hands over a sealed
//! pending token and this page; the session is created only once a code checks out.

use crate::backend::api::auth::login::append_auth_cookie;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::render::redirect_response;

use crate::backend::api::user::responses::field;
use crate::backend::api::user::responses::public_api_message;
use crate::backend::api::user::responses::safe_redirect;
use crate::backend::api::user::responses::within_bounds;
use crate::backend::api::user::responses::Fields;
use crate::backend::app::AppState;

use crate::backend::identity::login::LoginError;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use codeyard_webui::app::Page;
use codeyard_webui::pages::{account, auth::AuthMode};
use std::sync::Arc;

/// The second-factor page as the password step renders it. Nothing is signed in yet.
pub(crate) fn challenge(
    ctx: &PageContext,
    redirect: &str,
    pending: String,
    message: Option<&str>,
    status: StatusCode,
) -> Response {
    ctx.page_model_status(
        status,
        "Two-factor authentication",
        Page::Identity(account::Page::TwoFactor {
            action: "/login/two-factor".to_owned(),
            csrf_token: ctx.csrf_token.clone(),
            pending,
            redirect: redirect.to_owned(),
            message: message.map(str::to_owned),
            is_error: !status.is_success(),
        }),
    )
}

pub(crate) async fn sign_in_post(
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
    let redirect = safe_redirect(form.get("redirect").map(String::as_str));
    let pending = field(&form, "pending");
    if !within_bounds(&form) || pending.is_empty() || field(&form, "code").is_empty() {
        return challenge(
            &ctx,
            &redirect,
            pending.to_owned(),
            Some("Enter the code from your app or a recovery code."),
            StatusCode::BAD_REQUEST,
        );
    }
    let session_request =
        crate::backend::api::auth::session::LoginSessionRequest::from_headers(&ctx.headers);
    let result = crate::backend::identity::login::second_factor_login(
        &ctx.state.pool,
        ctx.state.secret_encryptor.as_deref(),
        pending,
        field(&form, "code"),
        session_request.context(),
    )
    .await;
    match result {
        Ok(authenticated) => {
            let redirect_target = ctx.redirect(&redirect);
            append_auth_cookie(
                redirect_target,
                &crate::backend::api::http::session::build_session_cookie(
                    &authenticated.session_id,
                    crate::backend::api::http::session::session_duration_days(),
                ),
            )
            .unwrap_or_else(|_| {
                ctx.error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Unable to issue the session cookie. Sign in again.",
                )
            })
        }
        Err(LoginError::PendingLoginExpired) => errors_expired(&ctx, &form).await,
        Err(error) => {
            let (status, body) = crate::backend::api::auth::errors::login_failure(error);
            challenge(
                &ctx,
                &redirect,
                pending.to_owned(),
                Some(public_api_message(&body.0)),
                status,
            )
        }
    }
}

async fn errors_expired(ctx: &PageContext, form: &Fields) -> Response {
    let (status, body) =
        crate::backend::api::auth::errors::login_failure(LoginError::PendingLoginExpired);
    crate::backend::api::auth::login::auth_failure(
        ctx,
        AuthMode::Login,
        "/login",
        form,
        status,
        public_api_message(&body.0),
    )
    .await
}

/// A GET from a reload or a typed address has no pending sign-in, so it goes to the sign-in page.
pub(crate) async fn sign_in_get() -> impl IntoResponse {
    redirect_response("/login")
}
