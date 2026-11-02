//! Public discovery and job-authenticated token exchange, independent of browser sessions.
use crate::backend::{app::AppState, ci::oidc};
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/.well-known/jwks", get(jwks))
        .route("/api/ci/oidc/token", get(token))
}

async fn discovery() -> Json<Value> {
    let issuer = oidc::issuer();
    Json(
        json!({"issuer":issuer,"jwks_uri":format!("{issuer}/.well-known/jwks"),
        "subject_types_supported":["public"],"response_types_supported":["id_token"],
        "id_token_signing_alg_values_supported":["RS256"]}),
    )
}

async fn jwks(State(state): State<Arc<AppState>>) -> Result<Json<Value>, StatusCode> {
    oidc::jwks(&state.oidc_keys)
        .await
        .map(Json)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
}

#[derive(Deserialize)]
struct Audience {
    audience: Option<String>,
}

async fn token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(input): Query<Audience>,
) -> Result<(HeaderMap, Json<Value>), StatusCode> {
    let credential =
        crate::backend::api::http::credentials::bearer(&headers).ok_or(StatusCode::FORBIDDEN)?;
    if input
        .audience
        .as_ref()
        .is_some_and(|a| a.is_empty() || a.len() > 2048 || a.chars().any(char::is_control))
    {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let value = oidc::issue(&state.oidc_keys, credential, input.audience.as_deref())
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::FORBIDDEN)?;
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok((headers, Json(json!({"value":value}))))
}
