use crate::backend::api::auth::errors;
use crate::backend::api::auth::session::LoginSessionRequest;
use crate::backend::api::auth::setup::configured_admin_secret;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::user::responses::{
    api_error, field, optional_field, public_api_message, safe_redirect, within_bounds, ApiFailure,
    AuthResponse, AuthUserResponse as UserResponse, Fields,
};
use crate::backend::api::{auth, http::session};
use crate::backend::app::AppState;
use crate::backend::identity;
use crate::backend::identity::credentials::PasswordAccount;
use crate::backend::identity::login::{FirstFactor, PasswordCredentials};
use crate::backend::identity::registration::RegistrationOutcome;
use crate::backend::identity::setup::SetupInput;
use crate::webui::pages::{
    account,
    auth::AuthMode,
    fields::{preserved_values, FormValues},
};
use axum::extract::{Form, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let session = LoginSessionRequest::from_headers(&headers);
    let first = identity::login::password_login(
        &state.pool,
        state.secret_encryptor.as_deref(),
        PasswordCredentials {
            identity: &req.username,
            password: &req.password,
        },
        session.context(),
    )
    .await
    .map_err(errors::login_failure)?;
    let FirstFactor::Authenticated(authenticated) = first else {
        return Err(errors::two_factor_required_failure());
    };
    let cookie =
        session::build_session_cookie(&authenticated.session_id, session::session_duration_days());
    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(AuthResponse {
            success: true,
            user: Some(UserResponse::from(authenticated.user)),
            message: None,
        }),
    ))
}

pub(crate) fn auth_mode(path: &str, mode: Option<&str>) -> AuthMode {
    match path {
        "/setup" => AuthMode::Setup,
        "/register" => AuthMode::Register,
        "/enter" if mode == Some("signup") => AuthMode::Register,
        _ => AuthMode::Login,
    }
}

/// Project only the ids of configured providers; credentials never enter SSR/hydration data.
pub(crate) fn auth_providers() -> Vec<String> {
    identity::oauth::providers::PROVIDERS
        .iter()
        .filter(|provider| identity::oauth::providers::provider_credentials(provider).is_some())
        .map(|provider| (*provider).to_owned())
        .collect()
}

/// Error text from APIs may contain driver details. Only reviewed public messages
/// pass through; never render raw DB/upstream messages or submitted secrets.
pub(crate) fn append_auth_cookie(
    mut redirect: Response,
    cookie: &str,
) -> Result<Response, axum::http::header::InvalidHeaderValue> {
    // append, not insert: the foundation may already have set a CSRF cookie.
    redirect
        .headers_mut()
        .append(header::SET_COOKIE, HeaderValue::from_str(cookie)?);
    Ok(redirect)
}

pub(crate) async fn auth_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let mode = auth_mode(uri.path(), ctx.query.get("mode").map(String::as_str));
    let redirect = safe_redirect(ctx.query.get("redirect").map(String::as_str));
    let setup = match identity::setup::status(&ctx.state.pool).await {
        Ok(setup) => setup,
        Err(_) => return api_error(&ctx, auth::errors::database_failure()),
    };
    if !setup.setup_completed && mode != AuthMode::Setup {
        return ctx.redirect("/setup");
    }
    if setup.setup_completed && mode == AuthMode::Setup {
        return ctx.error(
            StatusCode::CONFLICT,
            "Node setup is already complete, so sign in with the account created during setup.",
        );
    }
    if ctx.user.is_some() && mode != AuthMode::Setup {
        return ctx.redirect(&redirect);
    }
    let message = if ctx.query.get("signup").map(String::as_str) == Some("requested") {
        Some("Your signup request was submitted for administrator review. Sign in once it has been approved.")
    } else {
        None
    };
    ctx.page_model(
        mode.title(),
        crate::webui::app::Page::Identity(account::Page::Auth {
            mode,
            action: uri.path().to_owned(),
            csrf_token: ctx.csrf_token.clone(),
            values: FormValues::default(),
            private: setup.node_mode == "private",
            providers: auth_providers(),
            redirect,
            message: message.map(str::to_owned),
            is_error: false,
        }),
    )
}

/// The page behind "Forgot password?". It says how a password is recovered and changes nothing.
pub(crate) async fn password_help_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let redirect = safe_redirect(ctx.query.get("redirect").map(String::as_str));
    ctx.page_model(
        "Forgot your password?",
        crate::webui::app::Page::Identity(account::Page::PasswordHelp { redirect }),
    )
}

pub(crate) async fn auth_failure(
    ctx: &PageContext,
    mode: AuthMode,
    action: &str,
    form: &Fields,
    status: StatusCode,
    message: &str,
) -> Response {
    let setup = match identity::setup::status(&ctx.state.pool).await {
        Ok(setup) => setup,
        Err(_) => return api_error(ctx, auth::errors::database_failure()),
    };
    let redirect = safe_redirect(form.get("redirect").map(String::as_str));
    ctx.page_model_status(
        status,
        mode.title(),
        crate::webui::app::Page::Identity(account::Page::Auth {
            mode,
            action: action.to_owned(),
            csrf_token: ctx.csrf_token.clone(),
            values: preserved_values(form),
            private: setup.node_mode == "private",
            providers: auth_providers(),
            redirect,
            message: Some(message.to_owned()),
            is_error: true,
        }),
    )
}

pub(crate) async fn auth_post(
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
    let mode = auth_mode(uri.path(), form.get("mode").map(String::as_str));
    if !within_bounds(&form) {
        return auth_failure(
            &ctx,
            mode,
            uri.path(),
            &form,
            StatusCode::BAD_REQUEST,
            "One or more form fields exceed the supported length.",
        )
        .await;
    }
    if field(&form, "username").is_empty() || field(&form, "password").is_empty() {
        return auth_failure(
            &ctx,
            mode,
            uri.path(),
            &form,
            StatusCode::BAD_REQUEST,
            "Username and password are required.",
        )
        .await;
    }
    if mode != AuthMode::Login && field(&form, "password") != field(&form, "confirm_password") {
        return auth_failure(
            &ctx,
            mode,
            uri.path(),
            &form,
            StatusCode::BAD_REQUEST,
            "Passwords do not match.",
        )
        .await;
    }
    // Preserve the initiating anonymous identifier: identity rotates it and
    // invalidates its CSRF atomically. No JSON handler owns this sequencing.
    let session_request =
        crate::backend::api::auth::session::LoginSessionRequest::from_headers(&ctx.headers);
    let email = optional_field(&form, "email");
    let account = PasswordAccount {
        username: field(&form, "username"),
        email: email.as_deref(),
        password: field(&form, "password"),
    };
    let result: Result<Option<String>, ApiFailure> = match mode {
        AuthMode::Login => match identity::login::password_login(
            &ctx.state.pool,
            ctx.state.secret_encryptor.as_deref(),
            PasswordCredentials {
                identity: account.username,
                password: account.password,
            },
            session_request.context(),
        )
        .await
        {
            Ok(FirstFactor::Authenticated(authenticated)) => Ok(Some(authenticated.session_id)),
            Ok(FirstFactor::SecondFactor { pending }) => {
                let redirect = safe_redirect(form.get("redirect").map(String::as_str));
                return auth::two_factor::challenge(&ctx, &redirect, pending, None, StatusCode::OK);
            }
            Err(error) => Err(auth::errors::login_failure(error)),
        },
        AuthMode::Register => identity::registration::register_password(
            &ctx.state.pool,
            account,
            session_request.context(),
        )
        .await
        .map(|outcome| match outcome {
            RegistrationOutcome::Registered(authenticated) => Some(authenticated.session_id),
            RegistrationOutcome::Requested => None,
        })
        .map_err(auth::errors::registration_failure),
        AuthMode::Setup => {
            let supplied_secret = optional_field(&form, "admin_secret");
            let expected_secret = configured_admin_secret();
            identity::setup::complete_setup(
                &ctx.state.pool,
                SetupInput {
                    account,
                    node_mode: field(&form, "node_mode"),
                    admin_secret: supplied_secret.as_deref(),
                },
                expected_secret.as_deref(),
                session_request.context(),
            )
            .await
            .map(|authenticated| Some(authenticated.session_id))
            .map_err(auth::errors::setup_failure)
        }
    };
    match result {
        Ok(session_id) => {
            let target = if session_id.is_none() {
                let resume = safe_redirect(form.get("redirect").map(String::as_str));
                format!(
                    "/login?signup=requested&redirect={}",
                    crate::webui::encode_component(&resume)
                )
            } else {
                safe_redirect(form.get("redirect").map(String::as_str))
            };
            let redirect = ctx.redirect(&target);
            match session_id {
                Some(id) => append_auth_cookie(
                    redirect,
                    &session::build_session_cookie(&id, session::session_duration_days()),
                )
                .unwrap_or_else(|_| {
                    ctx.error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Unable to issue the session cookie. Sign in again.",
                    )
                }),
                None => redirect,
            }
        }
        Err(error) => {
            auth_failure(
                &ctx,
                mode,
                uri.path(),
                &form,
                error.0,
                public_api_message(&error.1 .0),
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::backend::db;

    use super::*;
    use crate::backend::api::user::profile::profile_model;
    use crate::backend::api::user::responses::{
        account_permission, public_api_message, safe_redirect, within_bounds,
    };
    use crate::backend::identity::profiles::visible_on_profile;
    use axum::response::IntoResponse;
    use std::collections::HashMap;

    #[test]
    fn redirects_are_local_even_for_encoded_authorities_and_control_characters() {
        for value in [
            "https://evil.example",
            "//evil.example",
            "/\\evil.example",
            "/%2Fevil.example",
            "/%5cevil.example",
            "/%252fevil.example",
            "/ok%0d%0aLocation:evil",
            "/ok\n",
            "",
        ] {
            assert_eq!(safe_redirect(Some(value)), "/", "{value:?}");
        }
        assert_eq!(
            safe_redirect(Some("/alice/project/tree/main?view=code")),
            "/alice/project/tree/main?view=code"
        );
    }

    #[test]
    fn authorization_matrix_denies_anonymous_non_admin_and_banned_accounts() {
        assert_eq!(
            account_permission(false, false, false, false),
            Err(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            account_permission(false, false, true, true),
            Err(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            account_permission(true, false, false, true),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            account_permission(true, true, true, true),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            account_permission(true, true, false, false),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(account_permission(true, false, false, false), Ok(()));
        assert_eq!(account_permission(true, false, true, true), Ok(()));
    }

    #[test]
    fn profile_lists_do_not_broaden_private_repository_visibility() {
        assert!(!visible_on_profile(true, false));
        assert!(visible_on_profile(true, true));
        assert!(visible_on_profile(false, false));
    }

    #[test]
    fn api_errors_do_not_disclose_driver_details_or_passwords() {
        let error = ApiError::new(
            "validation_error",
            "database constraint: password=private-secret-marker",
        );
        assert!(!public_api_message(&error).contains("private-secret-marker"));
        let error = ApiError::new("database_error", "postgres://user:password@host/private");
        assert!(!public_api_message(&error).contains("postgres://"));
        assert!(public_api_message(&error).contains("unavailable"));
    }

    // Credential rotation/old-ID invalidation is exercised by real PostgreSQL
    // tests in production_auth; stripping the initiating cookie would defeat it.

    #[test]
    fn auth_cookie_propagation_preserves_csrf_cookie() {
        let mut redirect = StatusCode::SEE_OTHER.into_response();
        redirect.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_static("csrf=one; HttpOnly"),
        );
        let response = append_auth_cookie(redirect, "session=two; HttpOnly").unwrap();
        let cookies: Vec<_> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect();
        assert_eq!(cookies, ["csrf=one; HttpOnly", "session=two; HttpOnly"]);
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }

    #[test]
    fn public_profile_page_serializes_presentation_not_private_account_records() {
        let now = chrono::Utc::now();
        let user = db::models::User {
            id: 11,
            username: "alice".into(),
            email: Some("private-email-marker@example.test".into()),
            password_hash: Some("private-password-hash-marker".into()),
            avatar_file_id: None,
            display_name: Some("Alice".into()),
            bio: Some("Public biography".into()),
            location: None,
            website: None,
            is_admin: true,
            is_banned: false,
            must_change_password: true,
            email_verified: true,
            two_factor_enabled: true,
            created_at: now,
            updated_at: now,
        };
        let page = account::Page::Profile {
            profile: profile_model(&user),
            repositories: vec![],
            repository_count: 0,
            csrf_token: "csrf".into(),
            own: false,
            authenticated: false,
            following: false,
            followers: 0,
            follows: 0,
            message: None,
            activity: Default::default(),
            stars: Default::default(),
        };
        let json = serde_json::to_string(&page).expect("safe public profile");
        assert!(json.contains("Public biography"));
        assert!(!json.contains("private-email-marker"));
        assert!(!json.contains("private-password-hash-marker"));
        for key in [
            "password_hash",
            "email_verified",
            "two_factor_enabled",
            "must_change_password",
            "is_admin",
        ] {
            assert!(!json.contains(key), "private account field {key} leaked");
        }
    }

    #[test]
    fn form_limits_and_enter_modes_are_explicit() {
        assert!(!within_bounds(&HashMap::from([(
            "public_key".into(),
            "x".repeat(16385)
        )])));
        assert!(!within_bounds(&HashMap::from([(
            "password".into(),
            "x".repeat(4097)
        )])));
        // Sign-in accepts every username and email the account policy accepts.
        assert!(within_bounds(&HashMap::from([(
            "username".into(),
            "é".repeat(255)
        )])));
        assert!(!within_bounds(&HashMap::from([(
            "username".into(),
            "x".repeat(1025)
        )])));
        assert_eq!(auth_mode("/enter", Some("signup")), AuthMode::Register);
        assert_eq!(auth_mode("/login", Some("signup")), AuthMode::Login);
        assert_eq!(auth_mode("/setup", None), AuthMode::Setup);
    }
}
