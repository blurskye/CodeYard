//! The pages behind the codes and links in email: asking for a password reset, choosing the new
//! password with the mailed code or link, and confirming an address. A link only opens a page;
//! its form must be sent to use it, so a mail scanner that fetches the link changes nothing.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::session;
use crate::backend::api::user::responses::{field, safe_redirect, within_bounds, Fields};
use crate::backend::app::AppState;
use crate::backend::email::links::{self, LinkError, Purpose};
use crate::backend::identity::{self, accounts::PasswordResetError};
use crate::webui::pages::account;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use std::sync::Arc;

const INVALID_LINK: &str =
    "This link is invalid, has expired or was already used. Ask for a new one.";
pub(crate) const INVALID_CODE: &str =
    "This code is wrong, has expired or was already used. Ask for a new email.";

/// The status and text shown when a mail request fails.
pub(crate) fn link_failure(error: &LinkError) -> (StatusCode, &'static str) {
    match error {
        LinkError::NotConfigured => (
            StatusCode::SERVICE_UNAVAILABLE,
            "This server cannot send email, so ask an administrator for help.",
        ),
        LinkError::NoAddress => (StatusCode::BAD_REQUEST, "Add an email address first."),
        LinkError::Limited => (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts. Try again later.",
        ),
        LinkError::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Email is temporarily unavailable. Try again later.",
        ),
    }
}

async fn forgot_page(
    ctx: &PageContext,
    redirect: String,
    status: StatusCode,
    message: Option<&str>,
) -> Response {
    let mail_available = crate::backend::email::config::is_configured(&ctx.state.pool).await;
    ctx.page_model_status(
        status,
        "Forgot your password?",
        crate::webui::app::Page::Identity(account::Page::PasswordHelp {
            redirect,
            csrf_token: ctx.csrf_token.clone(),
            mail_available,
            message: message.map(str::to_owned),
            is_error: !status.is_success(),
        }),
    )
}

pub(crate) async fn forgot_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let redirect = safe_redirect(ctx.query.get("redirect").map(String::as_str));
    forgot_page(&ctx, redirect, StatusCode::OK, None).await
}

pub(crate) async fn forgot_post(
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
    let address = field(&form, "email").trim();
    if !within_bounds(&form) || address.is_empty() {
        return forgot_page(
            &ctx,
            redirect,
            StatusCode::BAD_REQUEST,
            Some("Enter the email address of your account."),
        )
        .await;
    }
    let sent = links::send_password_reset(
        &ctx.state.pool,
        session::client_address(&ctx.headers),
        address,
    )
    .await;
    match sent {
        Ok(()) => code_page(
            &ctx,
            address,
            StatusCode::OK,
            Some("If a verified address matches, an email with a code is on its way. The code works for one hour."),
        ),
        Err(error) => {
            let (status, message) = link_failure(&error);
            forgot_page(&ctx, redirect, status, Some(message)).await
        }
    }
}

fn code_page(
    ctx: &PageContext,
    email: &str,
    status: StatusCode,
    message: Option<&str>,
) -> Response {
    ctx.page_model_status(
        status,
        "Check your email",
        crate::webui::app::Page::Identity(account::Page::ResetCode {
            csrf_token: ctx.csrf_token.clone(),
            email: email.to_owned(),
            message: message.map(str::to_owned),
            is_error: !status.is_success(),
        }),
    )
}

/// The code step on its own, for a reader who comes back to it later.
pub(crate) async fn reset_code_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let email = ctx.query.get("email").cloned().unwrap_or_default();
    code_page(&ctx, email.trim(), StatusCode::OK, None)
}

pub(crate) async fn reset_code_post(
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
    let email = field(&form, "email").trim();
    let password = field(&form, "new_password");
    if !within_bounds(&form) || password != field(&form, "confirm_password") {
        return code_page(
            &ctx,
            email,
            StatusCode::BAD_REQUEST,
            Some("Passwords must match and stay within the supported length."),
        );
    }
    // The code is the token from the link, so a pasted code works the same way.
    let code = field(&form, "code").trim();
    match identity::accounts::reset_password(&ctx.state.pool, code, password).await {
        Ok(()) => ctx.redirect("/login?reset=done"),
        Err(PasswordResetError::Input(_)) => code_page(
            &ctx,
            email,
            StatusCode::BAD_REQUEST,
            Some("Password must be between 8 and 1024 bytes."),
        ),
        Err(PasswordResetError::InvalidLink) => {
            code_page(&ctx, email, StatusCode::BAD_REQUEST, Some(INVALID_CODE))
        }
        Err(PasswordResetError::Password(_) | PasswordResetError::AccountUnavailable) => code_page(
            &ctx,
            email,
            StatusCode::SERVICE_UNAVAILABLE,
            Some("Unable to change the password right now. Try again."),
        ),
    }
}

/// Whether the link on a page still works, with the status and message to show. A page opened
/// with a dead link says so.
async fn link_state<'a>(
    ctx: &PageContext,
    purpose: Purpose,
    token: &str,
    status: StatusCode,
    message: Option<&'a str>,
) -> Result<(bool, StatusCode, Option<&'a str>), Response> {
    let valid = links::is_valid(&ctx.state.pool, purpose, token)
        .await
        .map_err(|error| {
            let (status, message) = link_failure(&error);
            ctx.error(status, message)
        })?;
    Ok(match message {
        None if !valid => (valid, StatusCode::BAD_REQUEST, Some(INVALID_LINK)),
        _ => (valid, status, message),
    })
}

async fn reset_page(
    ctx: &PageContext,
    token: &str,
    status: StatusCode,
    message: Option<&str>,
) -> Response {
    let (valid, status, message) =
        match link_state(ctx, Purpose::Reset, token, status, message).await {
            Ok(state) => state,
            Err(response) => return response,
        };
    ctx.page_model_status(
        status,
        "Reset your password",
        crate::webui::app::Page::Identity(account::Page::ResetPassword {
            csrf_token: ctx.csrf_token.clone(),
            token: token.to_owned(),
            valid,
            message: message.map(str::to_owned),
            is_error: !status.is_success(),
        }),
    )
}

pub(crate) async fn reset_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let token = ctx.query.get("token").cloned().unwrap_or_default();
    reset_page(&ctx, &token, StatusCode::OK, None).await
}

pub(crate) async fn reset_post(
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
    let token = field(&form, "token");
    let password = field(&form, "new_password");
    if !within_bounds(&form) || password != field(&form, "confirm_password") {
        return reset_page(
            &ctx,
            token,
            StatusCode::BAD_REQUEST,
            Some("Passwords must match and stay within the supported length."),
        )
        .await;
    }
    match identity::accounts::reset_password(&ctx.state.pool, token, password).await {
        Ok(()) => ctx.redirect("/login?reset=done"),
        Err(PasswordResetError::Input(_)) => {
            reset_page(
                &ctx,
                token,
                StatusCode::BAD_REQUEST,
                Some("Password must be between 8 and 1024 bytes."),
            )
            .await
        }
        Err(PasswordResetError::InvalidLink) => {
            reset_page(&ctx, token, StatusCode::BAD_REQUEST, Some(INVALID_LINK)).await
        }
        Err(PasswordResetError::Password(_) | PasswordResetError::AccountUnavailable) => {
            reset_page(
                &ctx,
                token,
                StatusCode::SERVICE_UNAVAILABLE,
                Some("Unable to change the password right now. Try again."),
            )
            .await
        }
    }
}

async fn verify_page(
    ctx: &PageContext,
    token: &str,
    status: StatusCode,
    message: Option<&str>,
) -> Response {
    let (valid, status, message) =
        match link_state(ctx, Purpose::Verify, token, status, message).await {
            Ok(state) => state,
            Err(response) => return response,
        };
    ctx.page_model_status(
        status,
        "Confirm your email address",
        crate::webui::app::Page::Identity(account::Page::VerifyEmail {
            csrf_token: ctx.csrf_token.clone(),
            token: token.to_owned(),
            valid,
            message: message.map(str::to_owned),
            is_error: !status.is_success(),
        }),
    )
}

pub(crate) async fn verify_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let token = ctx.query.get("token").cloned().unwrap_or_default();
    verify_page(&ctx, &token, StatusCode::OK, None).await
}

pub(crate) async fn verify_post(
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
    let token = field(&form, "token");
    match links::verify_address(&ctx.state.pool, token).await {
        Ok(true) => {
            verify_page(
                &ctx,
                token,
                StatusCode::OK,
                Some("Your email address is confirmed."),
            )
            .await
        }
        Ok(false) => verify_page(&ctx, token, StatusCode::BAD_REQUEST, Some(INVALID_LINK)).await,
        Err(error) => {
            let (status, message) = link_failure(&error);
            ctx.error(status, message)
        }
    }
}
