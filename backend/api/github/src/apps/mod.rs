//! OAuth application owners authenticate with their client ID and secret, not a user token.
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::post;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::app::AppState;
use codeyard_db_core::models::PersonalAccessToken;
use codeyard_db_identity::oauth_apps::Application;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::api_url;
use codeyard_support::token_hash::token_hash;
use codeyard_webhooks::payloads;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;
pub(super) mod installations;
mod manifests;
mod webhooks;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "apps/get-authenticated",
            get(crate::apps::installations::get_authenticated),
        ),
        (
            "apps/get-by-slug",
            get(crate::apps::installations::get_by_slug),
        ),
        (
            "apps/create-from-manifest",
            post(crate::apps::manifests::convert),
        ),
        (
            "apps/get-webhook-config-for-app",
            get(crate::apps::webhooks::get_config),
        ),
        (
            "apps/update-webhook-config-for-app",
            patch(crate::apps::webhooks::update_config),
        ),
        (
            "apps/list-webhook-deliveries",
            get(crate::apps::webhooks::list_deliveries),
        ),
        (
            "apps/get-webhook-delivery",
            get(crate::apps::webhooks::get_delivery),
        ),
        (
            "apps/redeliver-webhook-delivery",
            post(crate::apps::webhooks::redeliver),
        ),
        (
            "apps/list-installation-requests-for-authenticated-app",
            get(crate::apps::installations::installation_requests),
        ),
        (
            "apps/list-installations",
            get(crate::apps::installations::list_installations),
        ),
        (
            "apps/get-installation",
            get(crate::apps::installations::get_installation),
        ),
        (
            "apps/delete-installation",
            delete(crate::apps::installations::delete_installation),
        ),
        (
            "apps/suspend-installation",
            put(crate::apps::installations::suspend),
        ),
        (
            "apps/unsuspend-installation",
            delete(crate::apps::installations::unsuspend),
        ),
        (
            "apps/get-repo-installation",
            get(crate::apps::installations::find_installation),
        ),
        (
            "apps/get-org-installation",
            get(crate::apps::installations::find_installation),
        ),
        (
            "apps/get-user-installation",
            get(crate::apps::installations::find_installation),
        ),
        (
            "apps/create-installation-access-token",
            post(crate::apps::installations::create_token),
        ),
        (
            "apps/list-repos-accessible-to-installation",
            get(crate::apps::installations::list_repositories),
        ),
        (
            "apps/revoke-installation-access-token",
            delete(crate::apps::installations::revoke_token),
        ),
        (
            "apps/list-installations-for-authenticated-user",
            get(crate::apps::installations::user_installations),
        ),
        (
            "apps/list-installation-repos-for-authenticated-user",
            get(crate::apps::installations::user_repositories),
        ),
        (
            "apps/add-repo-to-installation-for-authenticated-user",
            put(crate::apps::installations::add_repository),
        ),
        (
            "apps/remove-repo-from-installation-for-authenticated-user",
            delete(crate::apps::installations::remove_repository),
        ),
        (
            "apps/scope-token",
            post(crate::apps::installations::scope_token),
        ),
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

pub(super) async fn application(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
) -> Result<Application> {
    let credentials = codeyard_api_http::credentials::basic(headers)
        .filter(|(client, _)| client == id)
        .map(|(_, secret)| secret);
    let app = codeyard_identity::oauth_apps::client(&state.pool, id)
        .await
        .map_err(Error::unavailable)?;
    if let (Some(app), Some(secret)) = (app, credentials) {
        if codeyard_identity::oauth_apps::valid_secret(&app, &secret) {
            return Ok(app);
        }
    }
    match codeyard_db_identity::auth_admission::admit_login(
        &state.pool,
        codeyard_repos_core::api_shared::http::session::client_address(headers),
        None,
        &format!("oauth-client:{id}"),
    )
    .await
    {
        Ok(_) => Err(Error::new(StatusCode::UNAUTHORIZED, "Bad credentials")),
        Err(codeyard_db_identity::auth_admission::AdmissionError::Limited) => Err(Error::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed logins",
        )),
        Err(codeyard_db_identity::auth_admission::AdmissionError::Unavailable) => {
            Err(Error::unavailable(()))
        }
    }
}

fn access_token(body: &Value) -> Result<&str> {
    body.get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 2048)
        .ok_or_else(|| Error::validation("Authorization", "access_token", "missing_field"))
}

async fn owned_token(state: &AppState, client: &str, hash: &str) -> Result<PersonalAccessToken> {
    let token = codeyard_db_identity::oauth_apps::owned_token(&state.pool, client, hash)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    codeyard_db_identity::users::tokens::token_user(&state.pool, token.id, false)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(token)
}

/// What an authorization response shows besides the token itself. It is read before a reset
/// rotates the token, so the response after the rotation needs no further reads.
struct Authorization {
    scopes: String,
    user: codeyard_db_core::models::User,
    /// A scoped user token's installation, with the token's own permissions and selection.
    installation: Option<Value>,
}

async fn authorization(state: &AppState, token: &PersonalAccessToken) -> Result<Authorization> {
    let metadata = codeyard_db_identity::oauth_apps::token(&state.pool, token.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let user = codeyard_db_identity::users::tokens::token_user(&state.pool, token.id, false)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let bounds = codeyard_db_identity::github_apps::token(&state.pool, token.id)
        .await
        .map_err(Error::unavailable)?
        .filter(|bounds| bounds.user_token);
    let mut installation = None;
    if let Some(bounds) = bounds {
        if let Some(row) =
            codeyard_db_identity::github_apps::installation(&state.pool, bounds.installation_id)
                .await
                .map_err(Error::unavailable)?
        {
            let selected: Vec<i64> =
                serde_json::from_str(&bounds.repositories).map_err(Error::unavailable)?;
            let mut value = crate::apps::installations::document(state, &row).await?;
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
    let token = codeyard_db_identity::oauth_apps::owned_token(&state.pool, &id, &token_hash(value))
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
    let value = if codeyard_db_identity::github_apps::app(&state.pool, &id)
        .await
        .map_err(Error::unavailable)?
        .is_some()
    {
        format!("ghu_{}", codeyard_support::crypto::random_hex())
    } else {
        codeyard_support::crypto::random_hex()
    };
    let token = codeyard_db_identity::oauth_apps::reset_token(
        &state.pool,
        &id,
        &token_hash(old),
        &token_hash(&value),
    )
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
    codeyard_db_identity::oauth_apps::revoke(
        &state.pool,
        &id,
        &token_hash(access_token(&body)?),
        false,
    )
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
    codeyard_db_identity::oauth_apps::revoke(
        &state.pool,
        &id,
        &token_hash(access_token(&body)?),
        true,
    )
    .await
    .map_err(Error::unavailable)?;
    Ok(StatusCode::NO_CONTENT)
}
