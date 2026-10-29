use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::{ApiError, ApiFailure};
use crate::backend::api::http::session;
use crate::backend::api::user::responses::{
    field, mutation_guard, public_api_message, require_account, within_bounds, Fields,
};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::identity::profiles::ProfileError;
use crate::backend::identity::{self, profiles};
use crate::webui::pages::{
    account,
    fields::{preserved_values, FormValues},
    settings::{settings_tab, EmailVerification, Key, SecondaryEmail, Settings, Token, TwoFactor},
    users::Profile,
};
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use std::sync::Arc;

pub(crate) fn profile_failure(error: ProfileError) -> (StatusCode, Json<ApiError>) {
    let (status, code) = match error {
        ProfileError::Invalid | ProfileError::Nul | ProfileError::Website | ProfileError::Email => {
            (StatusCode::BAD_REQUEST, "validation_error")
        }
        ProfileError::Denied => (StatusCode::FORBIDDEN, "forbidden"),
        ProfileError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    };
    (status, Json(ApiError::new(code, &error.to_string())))
}

pub(crate) async fn load_profile(
    pool: &db::DbPool,
    user: &db::models::User,
) -> anyhow::Result<Profile> {
    let settings = db::users::profiles::get_profile_settings(pool, user.id).await?;
    let accounts = db::users::social_accounts::list_social_accounts(pool, user.id).await?;
    Ok(profile_model(user, &settings, accounts))
}

pub(crate) fn profile_model(
    user: &db::models::User,
    settings: &db::users::profiles::ProfileSettings,
    accounts: Vec<db::users::social_accounts::SocialAccount>,
) -> Profile {
    Profile {
        username: user.username.clone(),
        social_accounts: accounts
            .into_iter()
            .map(|account| (account.provider, account.url))
            .collect(),
        company: settings.company.clone().unwrap_or_default(),
        twitter_username: settings.twitter_username.clone().unwrap_or_default(),
        hireable: settings.hireable.unwrap_or(false),
        public_email: settings.public_email(user).unwrap_or_default().to_owned(),
        primary_email_visibility: settings.primary_visibility.clone(),
        display_name: user.display_name.clone().unwrap_or_default(),
        bio: user.bio.clone().unwrap_or_default(),
        location: user.location.clone().unwrap_or_default(),
        website: user.website.clone().unwrap_or_default(),
        joined: user.created_at.format("%b %e, %Y").to_string(),
        avatar_url: user
            .avatar_file_id
            .map(|id| format!("/api/files/{id}/download")),
    }
}

fn settings_key(key: db::users::ssh_keys::SshKeySummary) -> Key {
    Key {
        id: key.id,
        name: key.name,
        fingerprint: key.fingerprint,
        kind: key.key_type,
        created: key.created_at.format("%b %e, %Y").to_string(),
    }
}

pub(crate) async fn load_settings(
    ctx: &PageContext,
    tab: &str,
    submitted: Option<&Fields>,
) -> Result<Settings, Response> {
    let user = require_account(ctx, false)?;
    // A temporary password leaves only the password form reachable. Decide this
    // before any other tab loads data, so every caller gets the same restriction.
    if user.must_change_password && tab != "password" {
        return Err(ctx.redirect("/settings?tab=password"));
    }
    let keys = if tab == "keys" {
        db::users::ssh_keys::list_user_key_summaries(
            &ctx.state.pool,
            user.id,
            ctx.query
                .get("keys_after")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        )
        .await
        .map_err(|_| {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load your SSH keys.",
            )
        })?
        .into_iter()
        .map(settings_key)
        .collect()
    } else {
        Vec::new()
    };
    let gpg_keys = if tab == "keys" {
        db::users::gpg_keys::list_gpg_keys(&ctx.state.pool, user.id, 100, 0)
            .await
            .map_err(|_| {
                ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Unable to load your GPG keys.",
                )
            })?
            .into_iter()
            .map(|key| Key {
                id: key.id,
                name: key.name.unwrap_or(key.key_id),
                fingerprint: key.fingerprint,
                kind: if key.material.revoked {
                    "Revoked GPG key"
                } else if key
                    .material
                    .expires_at
                    .is_some_and(|expiry| expiry <= chrono::Utc::now())
                {
                    "Expired GPG key"
                } else {
                    "GPG key"
                }
                .into(),
                created: key.material.created_at.format("%b %e, %Y").to_string(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let signing_keys = if tab == "keys" {
        db::users::signing_keys::list_signing_key_summaries(
            &ctx.state.pool,
            user.id,
            ctx.query
                .get("signing_after")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
        )
        .await
        .map_err(|_| {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load your signing keys.",
            )
        })?
        .into_iter()
        .map(settings_key)
        .collect()
    } else {
        Vec::new()
    };
    let tokens = if tab == "tokens" {
        let before = ctx
            .query
            .get("tokens_before")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        db::users::tokens::list_tokens(&ctx.state.pool, user.id, &session::extract_session_id(&ctx.headers).unwrap_or_default(), before)
            .await
            .map_err(|_| {
                ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Unable to load your tokens.",
                )
            })?
            .into_iter()
            .map(|token| Token {
                id: token.id,
                name: token.name,
                scope: token.scope,
                expires: token
                    .expires_at
                    .map(|expiry| expiry.to_rfc3339())
                    .unwrap_or_else(|| "never".to_owned()),
                created: token.created_at.format("%b %e, %Y").to_string(),
                last_used: token.last_used_at.map(|at| at.to_rfc3339()),
                last_ip: token.last_used_ip,
            })
            .collect()
    } else {
        Vec::new()
    };
    let sessions = if tab == "sessions" {
        super::sessions::load_sessions(
            &ctx.state.pool,
            user.id,
            session::extract_session_id(&ctx.headers).as_deref(),
        )
        .await
        .map_err(|_| {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load your sessions.",
            )
        })?
    } else {
        Vec::new()
    };
    let blocked = if tab == "blocked" {
        let page = ctx
            .query
            .get("page")
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(1)
            .clamp(1, 10001);
        let mut users = db::users::blocks::list_blocks(
            &ctx.state.pool,
            user.id,
            101,
            i64::from(page - 1) * 100,
        )
        .await
        .map_err(|_| {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load blocked users.",
            )
        })?;
        let more = users.len() > 100;
        users.truncate(100);
        crate::webui::pages::settings::BlockedAccounts {
            interaction: Some(
                super::interactions::panel(
                    &ctx.state.pool,
                    user.id,
                    db::interactions::InteractionScope::User(user.id),
                    &ctx.csrf_token,
                    true,
                )
                .await
                .map_err(|_| {
                    ctx.error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Unable to load interaction limits.",
                    )
                })?,
            ),
            users: users.into_iter().map(|user| user.username).collect(),
            page,
            more,
        }
    } else {
        Default::default()
    };
    let emails = if tab == "email" {
        db::users::emails::list_emails(&ctx.state.pool, user.id)
            .await
            .map_err(|_| {
                ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Unable to load your email addresses.",
                )
            })?
            .into_iter()
            .map(|email| SecondaryEmail {
                email: email.email,
                verified: email.verified,
            })
            .collect()
    } else {
        Vec::new()
    };
    let email_notifications = if tab == "email" {
        db::email::get_email_notifications(&ctx.state.pool, user.id)
            .await
            .map_err(|_| {
                ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Unable to load your notification setting.",
                )
            })?
    } else {
        true
    };
    let email_verification =
        if tab != "email" || user.email_verified || user.email.as_deref().is_none_or(str::is_empty)
        {
            EmailVerification::Verified
        } else if crate::backend::email::config::is_configured(&ctx.state.pool).await {
            EmailVerification::Unverified
        } else {
            EmailVerification::Unavailable
        };
    let profile = load_profile(&ctx.state.pool, user).await.map_err(|_| {
        ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to load your profile.",
        )
    })?;
    let values = if let Some(form) = submitted {
        preserved_values(form)
    } else {
        FormValues {
            display_name: profile.display_name.clone(),
            bio: profile.bio.clone(),
            location: profile.location.clone(),
            website: profile.website.clone(),
            company: profile.company.clone(),
            twitter_username: profile.twitter_username.clone(),
            hireable: profile.hireable,
            ..FormValues::default()
        }
    };
    Ok(Settings {
        theme: ctx.chrome().theme,
        profile,
        email: user.email.clone().unwrap_or_default(),
        emails,
        blocked,
        email_notifications,
        email_verification,
        tab: tab.to_owned(),
        keys,
        signing_keys,
        gpg_keys,
        tokens,
        sessions,
        new_token: None,
        password_supported: user.password_hash.is_some(),
        must_change_password: user.must_change_password,
        values,
        two_factor: TwoFactor {
            enabled: identity::two_factor::is_enabled(&ctx.state.pool, user.id)
                .await
                .map_err(|_| {
                    ctx.error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Unable to load your two-factor settings.",
                    )
                })?,
            setup: None,
            recovery_codes: Vec::new(),
        },
    })
}

pub(crate) async fn settings_response(
    ctx: &PageContext,
    tab: &str,
    submitted: Option<&Fields>,
    status: StatusCode,
    message: Option<&str>,
) -> Response {
    let mut ctx = ctx.clone();
    ctx.return_to = Some(format!("/settings?tab={tab}"));
    let ctx = &ctx;
    match load_settings(ctx, tab, submitted).await {
        Ok(model) => ctx.page_model_status(
            status,
            "Settings",
            crate::webui::app::Page::Identity(account::Page::Settings {
                model: Box::new(model),
                csrf_token: ctx.csrf_token.clone(),
                message: message.map(str::to_owned),
                is_error: !status.is_success(),
            }),
        ),
        Err(response) => response,
    }
}

pub(crate) async fn settings_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let tab = settings_tab(ctx.query.get("tab").map(String::as_str));
    let message = match ctx.query.get("saved").map(String::as_str) {
        Some("profile") => Some("Profile updated."),
        Some("social-accounts") => Some("Social accounts updated."),
        Some("avatar") => Some("Profile picture updated."),
        Some("avatar-removed") => Some("Profile picture removed."),
        Some("password") => Some("Password updated."),
        Some("two-factor-off") => Some("Two-factor authentication is off."),
        Some("key") => Some("SSH key added."),
        Some("signing-key") => Some("SSH signing key added."),
        Some("signing-key-deleted") => Some("SSH signing key deleted."),
        Some("gpg-key") => Some("GPG keys updated."),
        Some("deleted") => Some("SSH key deleted."),
        Some("token-deleted") => Some("Access token deleted."),
        Some("email") => Some("Email address updated."),
        Some("email-notifications") => Some("Notification setting saved."),
        Some("email-visibility") => Some("Email visibility saved."),
        Some("secondary-email") => Some("Email addresses updated."),
        Some("blocks") => Some("Blocked users updated."),
        Some("verification-sent") => {
            Some("Verification email sent. Paste its code here; it works for 24 hours.")
        }
        Some("email-verified") => Some("Your email address is confirmed."),
        Some("sessions-revoked") => Some("Session signed out."),
        _ => None,
    };
    settings_response(&ctx, tab, None, StatusCode::OK, message).await
}

pub(crate) async fn settings_result<T>(
    ctx: &PageContext,
    tab: &str,
    form: &Fields,
    result: Result<T, ApiFailure>,
    saved: &str,
) -> Response {
    match result {
        Ok(_) => ctx.redirect(&format!("/settings?tab={tab}&saved={saved}")),
        Err(error) => {
            settings_response(
                ctx,
                tab,
                Some(form),
                error.0,
                Some(public_api_message(&error.1 .0)),
            )
            .await
        }
    }
}

pub(crate) async fn social_accounts_post(
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
    let account = match profiles::social_account(field(&form, "url")) {
        Ok(account) => account,
        Err(_) => {
            return settings_response(
                &ctx,
                "profile",
                None,
                StatusCode::BAD_REQUEST,
                Some("Enter an HTTP or HTTPS social account URL."),
            )
            .await
        }
    };
    let session = session::extract_session_id(&ctx.headers).unwrap_or_default();
    match db::users::social_accounts::change_social_accounts(
        &ctx.state.pool,
        user.id,
        &db::users::accounts::AccountCredential::Session(&session),
        &[account],
        field(&form, "remove") != "yes",
    )
    .await
    {
        Ok(_) => ctx.redirect("/settings?tab=profile&saved=social-accounts"),
        Err(error) if error.is::<db::users::social_accounts::TooManyAccounts>() => {
            settings_response(
                &ctx,
                "profile",
                None,
                StatusCode::BAD_REQUEST,
                Some("At most four social accounts may be added."),
            )
            .await
        }
        Err(error) if error.is::<db::users::accounts::AccountRestricted>() => {
            ctx.error(StatusCode::FORBIDDEN, "Account is restricted.")
        }
        Err(_) => ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to save your social accounts.",
        ),
    }
}

pub(crate) async fn profile_post(
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
    if !within_bounds(&form) {
        return settings_response(
            &ctx,
            "profile",
            Some(&form),
            StatusCode::BAD_REQUEST,
            Some("One or more profile fields exceed the supported length."),
        )
        .await;
    }
    let actor = ctx
        .user
        .as_ref()
        .expect("mutation guard checked account")
        .id;
    let session = session::extract_session_id(&ctx.headers).unwrap_or_default();
    let result = profiles::save(
        &ctx.state.pool,
        actor,
        &db::users::accounts::AccountCredential::Session(&session),
        &db::users::profiles::ProfilePatch {
            display_name: Some(field(&form, "display_name")),
            bio: Some(field(&form, "bio")),
            location: Some(field(&form, "location")),
            website: Some(field(&form, "website")),
            company: form.get("company").map(String::as_str),
            twitter_username: form.get("twitter_username").map(String::as_str),
            hireable: form
                .contains_key("profile_details")
                .then(|| field(&form, "hireable") == "yes"),
            ..db::users::profiles::ProfilePatch::default()
        },
    )
    .await
    .map_err(profile_failure);
    settings_result(&ctx, "profile", &form, result, "profile").await
}
