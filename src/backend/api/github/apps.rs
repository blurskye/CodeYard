//! OAuth application owners authenticate with their client ID and secret, not a user token.
pub(super) mod installations;
mod manifests;
mod webhooks;
use super::{api_url, Error, Result};
use crate::backend::{
    api::http::session,
    app::AppState,
    db::{self, models::PersonalAccessToken, oauth_apps::Application},
    identity::{admission, oauth_apps as oauth, tokens::token_hash},
    webhooks::payloads,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, patch, post, put, MethodRouter},
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "apps/get-authenticated",
            get(installations::get_authenticated),
        ),
        ("apps/get-by-slug", get(installations::get_by_slug)),
        ("apps/create-from-manifest", post(manifests::convert)),
        ("apps/get-webhook-config-for-app", get(webhooks::get_config)),
        (
            "apps/update-webhook-config-for-app",
            patch(webhooks::update_config),
        ),
        (
            "apps/list-webhook-deliveries",
            get(webhooks::list_deliveries),
        ),
        ("apps/get-webhook-delivery", get(webhooks::get_delivery)),
        ("apps/redeliver-webhook-delivery", post(webhooks::redeliver)),
        (
            "apps/list-installation-requests-for-authenticated-app",
            get(installations::installation_requests),
        ),
        (
            "apps/list-installations",
            get(installations::list_installations),
        ),
        (
            "apps/get-installation",
            get(installations::get_installation),
        ),
        (
            "apps/delete-installation",
            delete(installations::delete_installation),
        ),
        ("apps/suspend-installation", put(installations::suspend)),
        (
            "apps/unsuspend-installation",
            delete(installations::unsuspend),
        ),
        (
            "apps/get-repo-installation",
            get(installations::find_installation),
        ),
        (
            "apps/get-org-installation",
            get(installations::find_installation),
        ),
        (
            "apps/get-user-installation",
            get(installations::find_installation),
        ),
        (
            "apps/create-installation-access-token",
            post(installations::create_token),
        ),
        (
            "apps/list-repos-accessible-to-installation",
            get(installations::list_repositories),
        ),
        (
            "apps/revoke-installation-access-token",
            delete(installations::revoke_token),
        ),
        (
            "apps/list-installations-for-authenticated-user",
            get(installations::user_installations),
        ),
        (
            "apps/list-installation-repos-for-authenticated-user",
            get(installations::user_repositories),
        ),
        (
            "apps/add-repo-to-installation-for-authenticated-user",
            put(installations::add_repository),
        ),
        (
            "apps/remove-repo-from-installation-for-authenticated-user",
            delete(installations::remove_repository),
        ),
        ("apps/scope-token", post(installations::scope_token)),
        ("apps/check-token", post(check_token)),
        ("apps/reset-token", patch(reset_token)),
        ("apps/delete-token", delete(delete_token)),
        ("apps/delete-authorization", delete(delete_authorization)),
    ]
}

/// Application-token operations own Basic authentication; other REST routes reject client secrets.
pub(super) fn client_credentials(path: &str) -> bool {
    matches!(
        path.trim_start_matches('/')
            .split('/')
            .collect::<Vec<_>>()
            .as_slice(),
        ["applications", _, "token" | "grant"] | ["applications", _, "token", "scoped"]
    )
}

pub(super) async fn application(state: &AppState, headers: &HeaderMap, id: &str) -> Result<Application> {
    let credentials = crate::backend::api::http::credentials::basic(headers)
        .filter(|(client, _)| client == id).map(|(_, secret)| secret);
    let app = oauth::client(&state.pool, id)
        .await
        .map_err(Error::unavailable)?;
    if let (Some(app), Some(secret)) = (app, credentials) {
        if oauth::valid_secret(&app, &secret) {
            return Ok(app);
        }
    }
    match admission::admit_login(
        &state.pool,
        session::client_address(headers),
        None,
        &format!("oauth-client:{id}"),
    )
    .await
    {
        Ok(_) => Err(Error::new(StatusCode::UNAUTHORIZED, "Bad credentials")),
        Err(admission::AdmissionError::Limited) => Err(Error::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed logins",
        )),
        Err(admission::AdmissionError::Unavailable) => Err(Error::unavailable(())),
    }
}

fn access_token(body: &Value) -> Result<&str> {
    body.get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 2048)
        .ok_or_else(|| Error::validation("Authorization", "access_token", "missing_field"))
}

async fn owned_token(state: &AppState, client: &str, hash: &str) -> Result<PersonalAccessToken> {
    let token = db::oauth_apps::owned_token(&state.pool, client, hash)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    db::users::tokens::token_user(&state.pool, token.id, false)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(token)
}

/// What an authorization response shows besides the token itself. It is read before a reset
/// rotates the token, so the response after the rotation needs no further reads.
struct Authorization {
    scopes: String,
    user: db::models::User,
    /// A scoped user token's installation, with the token's own permissions and selection.
    installation: Option<Value>,
}

async fn authorization(state: &AppState, token: &PersonalAccessToken) -> Result<Authorization> {
    let metadata = db::oauth_apps::token(&state.pool, token.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let user = db::users::tokens::token_user(&state.pool, token.id, false)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let bounds = db::github_apps::token(&state.pool, token.id)
        .await
        .map_err(Error::unavailable)?
        .filter(|bounds| bounds.user_token);
    let mut installation = None;
    if let Some(bounds) = bounds {
        if let Some(row) = db::github_apps::installation(&state.pool, bounds.installation_id)
            .await
            .map_err(Error::unavailable)?
        {
            let selected: Vec<i64> =
                serde_json::from_str(&bounds.repositories).map_err(Error::unavailable)?;
            let mut value = installations::document(state, &row).await?;
            value["permissions"] =
                serde_json::from_str(&bounds.permissions).map_err(Error::unavailable)?;
            value["repository_selection"] = json!(if row.all_repos && selected.is_empty() {
                "all"
            } else {
                "selected"
            });
            installation = Some(value);
        }
    }
    Ok(Authorization {
        scopes: metadata.scopes,
        user,
        installation,
    })
}

fn document(
    app: &Application,
    token: &PersonalAccessToken,
    value: &str,
    details: &Authorization,
) -> Value {
    json!({
        "id":token.id,"url":api_url(&format!("/applications/{}/token",app.client_id)),
        "scopes":details.scopes.split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>(),
        "token":value,"token_last_eight":value.get(value.len().saturating_sub(8)..),
        "hashed_token":token_hash(value),"app":{"client_id":app.client_id,"name":app.name,"url":app.homepage},
        "note":null,"note_url":null,"updated_at":token.created_at,"created_at":token.created_at,
        "fingerprint":null,"user":payloads::user(&details.user),"expires_at":token.expires_at,
        "installation":details.installation,
    })
}

async fn check_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>> {
    let app = application(&state, &headers, &id).await?;
    let value = access_token(&body)?;
    let token = db::oauth_apps::owned_token(&state.pool, &id, &token_hash(value))
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let details = authorization(&state, &token).await?;
    Ok(Json(document(&app, &token, value, &details)))
}

async fn reset_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>> {
    let app = application(&state, &headers, &id).await?;
    let old = access_token(&body)?;
    let current = owned_token(&state, &id, &token_hash(old)).await?;
    // Everything the response shows is read first: once the hash rotates, the old token is
    // gone and the new one exists only in this response.
    let details = authorization(&state, &current).await?;
    let value = if db::github_apps::app(&state.pool, &id)
        .await
        .map_err(Error::unavailable)?
        .is_some()
    {
        format!("ghu_{}", crate::backend::crypto::random_hex())
    } else {
        crate::backend::crypto::random_hex()
    };
    let token =
        db::oauth_apps::reset_token(&state.pool, &id, &token_hash(old), &token_hash(&value))
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    Ok(Json(document(&app, &token, &value, &details)))
}

async fn delete_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode> {
    application(&state, &headers, &id).await?;
    db::oauth_apps::revoke(&state.pool, &id, &token_hash(access_token(&body)?), false)
        .await
        .map_err(Error::unavailable)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_authorization(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode> {
    application(&state, &headers, &id).await?;
    db::oauth_apps::revoke(&state.pool, &id, &token_hash(access_token(&body)?), true)
        .await
        .map_err(Error::unavailable)?;
    Ok(StatusCode::NO_CONTENT)
}
