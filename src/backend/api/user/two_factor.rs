//! Account settings for two-factor sign-in: set up, confirm, regenerate recovery codes, turn off.
//! Each form posts to a POST-only URL; a rejected form shows the settings tab again.

use crate::backend::api::auth::errors::two_factor_failure;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::user::profile::{load_settings, settings_response};
use crate::backend::api::user::responses::{
    field, mutation_guard, public_api_message, require_account, Fields,
};
use crate::backend::app::AppState;
use crate::backend::crypto::SecretEncryptor;
use crate::backend::identity::two_factor::{self, TwoFactorError};
use crate::webui::app::Page;
use crate::webui::pages::{
    account,
    settings::{TwoFactor, TwoFactorSetup},
};
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use std::sync::Arc;

fn encryptor(ctx: &PageContext) -> Result<&SecretEncryptor, TwoFactorError> {
    ctx.state
        .secret_encryptor
        .as_deref()
        .ok_or(TwoFactorError::KeyUnavailable)
}

/// The two-factor tab with the given state: a secret waiting for its code, or codes to show once.
async fn tab(
    ctx: &PageContext,
    status: StatusCode,
    message: Option<&str>,
    two_factor: TwoFactor,
) -> Response {
    match load_settings(ctx, "two-factor", None).await {
        Ok(mut model) => {
            model.two_factor = two_factor;
            ctx.page_model_status(
                status,
                "Settings",
                Page::Identity(account::Page::Settings {
                    model: Box::new(model),
                    csrf_token: ctx.csrf_token.clone(),
                    message: message.map(str::to_owned),
                    is_error: !status.is_success(),
                }),
            )
        }
        Err(response) => response,
    }
}

/// A refused change: the tab again, with the stable public message for the error.
async fn refused(ctx: &PageContext, form: &Fields, error: TwoFactorError) -> Response {
    let (status, body) = two_factor_failure(error);
    settings_response(
        ctx,
        "two-factor",
        Some(form),
        status,
        Some(public_api_message(&body.0)),
    )
    .await
}

pub(crate) async fn setup_post(
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
        Ok(user) => user.clone(),
        Err(response) => return response,
    };
    let setup = match encryptor(&ctx) {
        Ok(key) => {
            two_factor::begin_setup(
                &ctx.state.pool,
                key,
                &user,
                &crate::backend::api::http::session::extract_session_id(&ctx.headers)
                    .unwrap_or_default(),
                field(&form, "current_password"),
            )
            .await
        }
        Err(error) => Err(error),
    };
    match setup {
        Ok(setup) => {
            tab(
                &ctx,
                StatusCode::OK,
                None,
                TwoFactor {
                    enabled: false,
                    setup: Some(TwoFactorSetup {
                        secret: setup.secret,
                        uri: setup.uri,
                        token: setup.token,
                    }),
                    recovery_codes: Vec::new(),
                },
            )
            .await
        }
        Err(error) => refused(&ctx, &form, error).await,
    }
}

pub(crate) async fn enable_post(
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
        Ok(user) => user.clone(),
        Err(response) => return response,
    };
    let token = field(&form, "setup");
    let confirmed = match encryptor(&ctx) {
        Ok(key) => {
            two_factor::confirm_setup(
                &ctx.state.pool,
                key,
                &user,
                &crate::backend::api::http::session::extract_session_id(&ctx.headers)
                    .unwrap_or_default(),
                field(&form, "current_password"),
                token,
                field(&form, "code"),
            )
            .await
        }
        Err(error) => Err(error),
    };
    match confirmed {
        Ok(codes) => {
            tab(
                &ctx,
                StatusCode::OK,
                Some("Two-factor authentication is on. Save your recovery codes."),
                TwoFactor {
                    enabled: true,
                    setup: None,
                    recovery_codes: codes,
                },
            )
            .await
        }
        // A wrong code keeps the same secret on the page, so the app can be tried again.
        Err(TwoFactorError::InvalidCode) => {
            let reopened = match encryptor(&ctx) {
                Ok(key) => two_factor::open_setup(
                    key,
                    &user,
                    &crate::backend::api::http::session::extract_session_id(&ctx.headers)
                        .unwrap_or_default(),
                    token,
                    two_factor::now_unix(),
                ),
                Err(error) => Err(error),
            };
            match reopened {
                Ok(setup) => {
                    tab(
                        &ctx,
                        StatusCode::BAD_REQUEST,
                        Some("That code does not match. Check your app and try again."),
                        TwoFactor {
                            enabled: false,
                            setup: Some(TwoFactorSetup {
                                secret: setup.secret,
                                uri: setup.uri,
                                token: setup.token,
                            }),
                            recovery_codes: Vec::new(),
                        },
                    )
                    .await
                }
                Err(error) => refused(&ctx, &form, error).await,
            }
        }
        Err(error) => refused(&ctx, &form, error).await,
    }
}

pub(crate) async fn recovery_codes_post(
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
        Ok(user) => user.clone(),
        Err(response) => return response,
    };
    match two_factor::regenerate_recovery_codes(
        &ctx.state.pool,
        &user,
        &crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default(),
        field(&form, "current_password"),
    )
    .await
    {
        Ok(codes) => {
            tab(
                &ctx,
                StatusCode::OK,
                Some("New recovery codes. The old ones no longer work."),
                TwoFactor {
                    enabled: true,
                    setup: None,
                    recovery_codes: codes,
                },
            )
            .await
        }
        Err(error) => refused(&ctx, &form, error).await,
    }
}

pub(crate) async fn disable_post(
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
        Ok(user) => user.clone(),
        Err(response) => return response,
    };
    match two_factor::disable(
        &ctx.state.pool,
        &user,
        &crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default(),
        field(&form, "current_password"),
    )
    .await
    {
        Ok(()) => ctx.redirect("/settings?tab=two-factor&saved=two-factor-off"),
        Err(error) => refused(&ctx, &form, error).await,
    }
}
