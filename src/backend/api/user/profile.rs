use crate::backend::api::admin::signup_requests::load_requests;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::user::responses::{
    field, mutation_guard, public_api_message, require_account, within_bounds, ApiFailure, Fields,
    PublicUserResponse,
};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::identity::profiles;
use crate::backend::identity::profiles::{ProfileError, ProfileUpdate};
use crate::webui::pages::{
    account,
    fields::{preserved_values, FormValues},
    settings::{settings_tab, Key, Settings, Token},
    users::Profile,
};
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct UpdateUserRequest {
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub location: Option<String>,
    pub website: Option<String>,
}

pub(crate) fn profile_failure(error: ProfileError) -> (StatusCode, Json<ApiError>) {
    let (status, code) = match error {
        ProfileError::Invalid | ProfileError::Nul => (StatusCode::BAD_REQUEST, "validation_error"),
        ProfileError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    };
    (status, Json(ApiError::new(code, &error.to_string())))
}

pub async fn update_user_profile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<UpdateUserRequest>,
) -> Result<Json<PublicUserResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let updated = profiles::update(
        &state.pool,
        user.id,
        ProfileUpdate {
            display_name: req.display_name.as_deref(),
            bio: req.bio.as_deref(),
            location: req.location.as_deref(),
            website: req.website.as_deref(),
        },
    )
    .await
    .map_err(profile_failure)?;
    Ok(Json(PublicUserResponse::from(updated)))
}

pub(crate) fn profile_model(user: &db::models::User) -> Profile {
    Profile {
        username: user.username.clone(),
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

pub(crate) async fn private_access(ctx: &PageContext) -> Result<bool, Response> {
    let config = db::node_config::get_node_configuration(&ctx.state.pool)
        .await
        .map_err(|_| {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load node access configuration.",
            )
        })?
        .ok_or_else(|| {
            ctx.error(
                StatusCode::PRECONDITION_REQUIRED,
                "Complete node setup first.",
            )
        })?;
    Ok(config.setup_completed && config.node_mode == "private")
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
    let manage_access = user.is_admin && private_access(ctx).await?;
    if tab == "access" && !manage_access {
        return Err(ctx.error(
            StatusCode::FORBIDDEN,
            "Private node access management requires an administrator on a private node.",
        ));
    }
    let keys = if tab == "keys" {
        db::users::ssh_keys::list_user_keys(&ctx.state.pool, user.id)
            .await
            .map_err(|_| {
                ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Unable to load your SSH keys.",
                )
            })?
            .into_iter()
            .map(|key| Key {
                id: key.id,
                name: key.name,
                fingerprint: key.fingerprint,
                kind: key.key_type,
                created: key.created_at.format("%b %e, %Y").to_string(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let tokens = if tab == "tokens" {
        db::users::tokens::list_tokens(&ctx.state.pool, user.id)
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
            })
            .collect()
    } else {
        Vec::new()
    };
    let requests = if tab == "access" {
        load_requests(ctx).await?
    } else {
        Vec::new()
    };
    let profile = profile_model(user);
    let values = if let Some(form) = submitted {
        preserved_values(form)
    } else {
        FormValues {
            display_name: profile.display_name.clone(),
            bio: profile.bio.clone(),
            location: profile.location.clone(),
            website: profile.website.clone(),
            ..FormValues::default()
        }
    };
    Ok(Settings {
        theme: ctx.chrome().theme,
        profile,
        email: user.email.clone().unwrap_or_default(),
        tab: tab.to_owned(),
        keys,
        tokens,
        new_token: None,
        password_supported: user.password_hash.is_some(),
        must_change_password: user.must_change_password,
        manage_access,
        requests,
        values,
    })
}

pub(crate) async fn settings_response(
    ctx: &PageContext,
    tab: &str,
    submitted: Option<&Fields>,
    status: StatusCode,
    message: Option<&str>,
) -> Response {
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
        Some("avatar") => Some("Profile picture updated."),
        Some("avatar-removed") => Some("Profile picture removed."),
        Some("password") => Some("Password updated."),
        Some("key") => Some("SSH key added."),
        Some("deleted") => Some("SSH key deleted."),
        Some("token-deleted") => Some("Access token deleted."),
        Some("account") => Some("Managed account created. Share the temporary password securely."),
        Some("request") => Some("Signup request decision recorded."),
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
    let result = profiles::update(
        &ctx.state.pool,
        actor,
        ProfileUpdate {
            display_name: Some(field(&form, "display_name")),
            bio: Some(field(&form, "bio")),
            location: Some(field(&form, "location")),
            website: Some(field(&form, "website")),
        },
    )
    .await
    .map_err(profile_failure);
    settings_result(&ctx, "profile", &form, result, "profile").await
}
