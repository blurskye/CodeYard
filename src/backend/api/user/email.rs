//! Changing the account's email address (the current password is required), mailing a new
//! confirmation code for it, confirming it with that code, and choosing whether notifications are
//! also sent by email.

use crate::backend::api::auth::email_links::{link_failure, INVALID_CODE};
use crate::backend::api::auth::errors::email_failure;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::session;
use crate::backend::api::user::profile::{settings_response, settings_result};
use crate::backend::api::user::responses::{field, mutation_guard, require_account, Fields};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::email::links;
use crate::backend::identity;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use std::sync::Arc;

pub(crate) async fn email_post(
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
    let session_id = session::extract_session_id(&ctx.headers);
    let result = identity::accounts::change_email(
        &ctx.state.pool,
        session_id.as_deref(),
        field(&form, "current_password"),
        field(&form, "email"),
    )
    .await
    .map_err(email_failure);
    settings_result(&ctx, "email", &form, result, "email").await
}

/// Mails a new confirmation code for the signed-in account's address.
pub(crate) async fn verification_post(
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
    let user = match require_account(&ctx, false) {
        Ok(user) => user,
        Err(response) => return response,
    };
    if user.email_verified {
        return ctx.redirect("/settings?tab=email");
    }
    match links::send_verification(&ctx.state.pool, user).await {
        Ok(()) => ctx.redirect("/settings?tab=email&saved=verification-sent"),
        Err(error) => {
            let (status, message) = link_failure(&error);
            settings_response(&ctx, "email", None, status, Some(message)).await
        }
    }
}

/// Confirms the signed-in account's address with the code from the verification email.
pub(crate) async fn verification_code_post(
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
    if let Err(response) = require_account(&ctx, false) {
        return response;
    }
    match links::verify_address(&ctx.state.pool, field(&form, "code").trim()).await {
        Ok(true) => ctx.redirect("/settings?tab=email&saved=email-verified"),
        Ok(false) => {
            settings_response(
                &ctx,
                "email",
                None,
                StatusCode::BAD_REQUEST,
                Some(INVALID_CODE),
            )
            .await
        }
        Err(error) => {
            let (status, message) = link_failure(&error);
            settings_response(&ctx, "email", None, status, Some(message)).await
        }
    }
}

pub(crate) async fn visibility_post(
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
    let user = match require_account(&ctx, false) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let session = session::extract_session_id(&ctx.headers).unwrap_or_default();
    let visibility = if field(&form, "public_email") == "yes" {
        "public"
    } else {
        "private"
    };
    let result = identity::profiles::save(
        &ctx.state.pool,
        user.id,
        &db::users::accounts::AccountCredential::Session(&session),
        &db::users::profiles::ProfilePatch {
            primary_visibility: (!form.contains_key("public_address")).then_some(visibility),
            // Showing the primary address replaces a chosen secondary one.
            email: form
                .get("public_address")
                .map(String::as_str)
                .or((visibility == "public").then_some("")),
            ..db::users::profiles::ProfilePatch::default()
        },
    )
    .await
    .map_err(super::profile::profile_failure);
    settings_result(&ctx, "email", &form, result, "email-visibility").await
}

pub(crate) async fn secondary_post(
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
    let user = match require_account(&ctx, false) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let session = session::extract_session_id(&ctx.headers).unwrap_or_default();
    let remove = field(&form, "remove") == "yes";
    let address = field(&form, if remove { "email" } else { "secondary_email" }).to_owned();
    match identity::emails::change(
        &ctx.state.pool,
        user.id,
        &db::users::accounts::AccountCredential::Session(&session),
        &[address],
        !remove,
    )
    .await
    {
        Ok(_) => ctx.redirect("/settings?tab=email&saved=secondary-email"),
        Err(error) => {
            let status = match error {
                identity::emails::EmailError::Denied => StatusCode::FORBIDDEN,
                identity::emails::EmailError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::UNPROCESSABLE_ENTITY,
            };
            settings_response(&ctx, "email", None, status, Some(&error.to_string())).await
        }
    }
}

pub(crate) async fn secondary_verification_post(
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
    let user = match require_account(&ctx, false) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let email = [field(&form, "email").to_owned()];
    match links::send_secondary_verifications(&ctx.state.pool, user, &email).await {
        Ok(()) => ctx.redirect("/settings?tab=email&saved=verification-sent"),
        Err(error) => {
            let (status, message) = link_failure(&error);
            settings_response(&ctx, "email", None, status, Some(message)).await
        }
    }
}

pub(crate) async fn notifications_post(
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
    let user = match require_account(&ctx, false) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let wanted = field(&form, "email_notifications") == "yes";
    let session = session::extract_session_id(&ctx.headers).unwrap_or_default();
    let credential = db::users::accounts::AccountCredential::Session(&session);
    match db::email::set_email_notifications(&ctx.state.pool, user.id, &credential, wanted).await {
        Ok(()) => ctx.redirect("/settings?tab=email&saved=email-notifications"),
        Err(error) if error.is::<db::users::accounts::AccountRestricted>() => ctx.error(
            StatusCode::FORBIDDEN, "A live unrestricted account session is required.",
        ),
        Err(_) => ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to save your notification setting.",
        ),
    }
}
