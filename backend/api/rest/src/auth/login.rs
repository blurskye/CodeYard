use crate::user::responses::api_error;
use crate::user::responses::optional_field;
use crate::user::responses::public_api_message;
use crate::user::responses::safe_redirect;
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
use codeyard_api_http::error::ApiFailure;
use codeyard_api_http::shared::user::responses::field;
use codeyard_api_http::shared::user::responses::Fields;
use codeyard_app::app::AppState;
use codeyard_identity::credentials::PasswordAccount;
use codeyard_identity::login::FirstFactor;
use codeyard_identity::login::PasswordCredentials;
use codeyard_identity::registration::RegistrationOutcome;
use codeyard_identity::setup::SetupInput;
use codeyard_webui::pages::account;
use codeyard_webui::pages::auth::AuthMode;
use codeyard_webui::pages::fields::preserved_values;
use codeyard_webui::pages::fields::FormValues;
use std::sync::Arc;

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
    codeyard_identity::oauth::providers::PROVIDERS
        .iter()
        .filter(|provider| {
            codeyard_identity::oauth::providers::provider_credentials(provider).is_some()
        })
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
    let setup = match codeyard_identity::setup::status(&ctx.state.pool).await {
        Ok(setup) => setup,
        Err(_) => return api_error(&ctx, crate::auth::errors::database_failure()),
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
    } else if ctx.query.get("reset").map(String::as_str) == Some("done") {
        Some("Your password was changed. Sign in with the new password.")
    } else {
        None
    };
    ctx.page_model(
        mode.title(),
        codeyard_webui::app::Page::Identity(account::Page::Auth {
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

pub(crate) async fn auth_failure(
    ctx: &PageContext,
    mode: AuthMode,
    action: &str,
    form: &Fields,
    status: StatusCode,
    message: &str,
) -> Response {
    let setup = match codeyard_identity::setup::status(&ctx.state.pool).await {
        Ok(setup) => setup,
        Err(_) => return api_error(ctx, crate::auth::errors::database_failure()),
    };
    let redirect = safe_redirect(form.get("redirect").map(String::as_str));
    ctx.page_model_status(
        status,
        mode.title(),
        codeyard_webui::app::Page::Identity(account::Page::Auth {
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
    // invalidates its CSRF atomically.
    let session_request =
        crate::auth::session::LoginSessionRequest::from_headers(&ctx.state.pool, &ctx.headers);
    let email = optional_field(&form, "email");
    let account = PasswordAccount {
        username: field(&form, "username"),
        email: email.as_deref(),
        password: field(&form, "password"),
    };
    let result: Result<Option<String>, ApiFailure> = match mode {
        AuthMode::Login => match codeyard_identity::login::password_login(
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
                return crate::auth::two_factor::challenge(
                    &ctx,
                    &redirect,
                    pending,
                    None,
                    StatusCode::OK,
                );
            }
            Err(error) => Err(crate::auth::errors::login_failure(error)),
        },
        AuthMode::Register => codeyard_identity::registration::register_password(
            &ctx.state.pool,
            account,
            session_request.context(),
        )
        .await
        .map(|outcome| match outcome {
            RegistrationOutcome::Registered(authenticated) => Some(authenticated.session_id),
            RegistrationOutcome::Requested => None,
        })
        .map_err(crate::auth::errors::registration_failure),
        AuthMode::Setup => {
            let supplied_secret = optional_field(&form, "admin_secret");
            let expected_secret = match codeyard_identity::setup::configured_admin_secret() {
                Ok(secret) => secret,
                Err(_) => {
                    return ctx.error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Node setup configuration is invalid.",
                    )
                }
            };
            codeyard_identity::setup::complete_setup(
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
            .map_err(crate::auth::errors::setup_failure)
        }
    };
    match result {
        Ok(session_id) => {
            let target = if session_id.is_none() {
                let resume = safe_redirect(form.get("redirect").map(String::as_str));
                format!(
                    "/login?signup=requested&redirect={}",
                    codeyard_webui::encode_component(&resume)
                )
            } else {
                safe_redirect(form.get("redirect").map(String::as_str))
            };
            let redirect = ctx.redirect(&target);
            match session_id {
                Some(id) => codeyard_api_http::session::build_session_cookie(
                    &id,
                    codeyard_api_http::session::session_duration_days(),
                    ctx.state.pool.cell_prefix(),
                )
                .ok()
                .and_then(|cookie| append_auth_cookie(redirect, &cookie).ok())
                .unwrap_or_else(|| {
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
    use codeyard_api_http::error::ApiError;

    use super::*;
    use crate::user::profile::profile_model;
    use crate::user::responses::account_permission;
    use crate::user::responses::public_api_message;
    use crate::user::responses::safe_redirect;
    use crate::user::responses::within_bounds;
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
        assert_eq!(safe_redirect(Some("/search?q=50%25")), "/search?q=50%25");
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
        let user = codeyard_db_core::models::User {
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
            profile: profile_model(
                &user,
                &codeyard_db_identity::users::profiles::ProfileSettings::default(),
                vec![],
            ),
            repositories: vec![],
            repository_count: 0,
            repositories_newer: None,
            repositories_older: None,
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
