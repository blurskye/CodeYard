use axum::extract::rejection::JsonRejection;
use axum::extract::rejection::QueryRejection;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_app::app::AppState;
use codeyard_db_core::queries::attestation_validation::AttestationError;
use codeyard_db_security::attestation_validation as store;
use codeyard_db_security::attestation_validation::PreparedBundle;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/create-attestation", post(create)),
        ("repos/list-attestations", get(list)),
    ]
}
pub(crate) fn aliases() -> Router<Arc<AppState>> {
    Router::new().cell_route(
        "/repos/:owner/:repo/attestations/bundles/:attestation_id",
        get(bundle),
    )
}
fn error(error: AttestationError) -> Error {
    match error {
        AttestationError::Denied => {
            Error::new(StatusCode::FORBIDDEN, "Resource not accessible by token")
        }
        AttestationError::NotFound => Error::missing(),
        AttestationError::InvalidBundle | AttestationError::Size => {
            Error::validation("Attestation", "bundle", "invalid")
        }
        AttestationError::InvalidDigest => {
            Error::validation("Attestation", "subject_digest", "invalid")
        }
        AttestationError::InvalidPage => Error::validation("Attestation", "pagination", "invalid"),
        _ => Error::unavailable(error),
    }
}
#[derive(Deserialize)]
struct Create {
    bundle: Value,
}
async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    body: std::result::Result<Json<Create>, JsonRejection>,
) -> Result<Response> {
    let (repo, auth) =
        codeyard_app::api_shared::github::repository(&state, &headers, &owner, &name, false)
            .await?;
    let auth = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    auth.require_write()?;
    let Json(body) = body.map_err(codeyard_model::github_error::json_error)?;
    let bundle = PreparedBundle::new(&body.bundle).map_err(error)?;
    let id = store::create(&state.pool, auth.database_actor(), repo.id, &bundle)
        .await
        .map_err(error)?;
    Ok((StatusCode::CREATED, Json(json!({"id":id}))).into_response())
}
#[derive(Deserialize, Default)]
struct CursorQuery {
    per_page: Option<usize>,
    before: Option<String>,
    after: Option<String>,
    predicate_type: Option<String>,
}
fn decode_cursor(value: Option<&str>, field: &str) -> Result<Option<i64>> {
    let Some(value) = value else { return Ok(None) };
    let invalid = || Error::validation("Attestation", field, "invalid");
    if value.len() > 128 {
        return Err(invalid());
    }
    let decoded = URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?;
    let text = std::str::from_utf8(&decoded).map_err(|_| invalid())?;
    let id = text
        .strip_prefix("attestations:")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|id| *id > 0)
        .ok_or_else(invalid)?;
    Ok(Some(id))
}
fn link(uri: &Uri, relation: &str, id: i64) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()) {
        if key != "before" && key != "after" {
            query.append_pair(&key, &value);
        }
    }
    let cursor = URL_SAFE_NO_PAD.encode(format!("attestations:{id}"));
    query.append_pair(
        if relation == "next" {
            "after"
        } else {
            "before"
        },
        &cursor,
    );
    format!(
        "<{}{}?{}>; rel=\"{relation}\"",
        codeyard_model::http_config::link_origin(),
        uri.path(),
        query.finish()
    )
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, digest)): Path<(String, String, String)>,
    OriginalUri(uri): OriginalUri,
    query: std::result::Result<Query<CursorQuery>, QueryRejection>,
) -> Result<Response> {
    let (repo, auth) =
        codeyard_app::api_shared::github::repository(&state, &headers, &owner, &name, false)
            .await?;
    let auth = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    let Query(query) = query.map_err(|_| Error::invalid("Invalid attestation pagination"))?;
    let after = decode_cursor(query.after.as_deref(), "after")?;
    let before = decode_cursor(query.before.as_deref(), "before")?;
    let page = codeyard_db_core::queries::attestation_validation::Page {
        after,
        before,
        limit: codeyard_api_http::shared::github::pagination::page_size(query.per_page) as i64,
        predicate_type: query.predicate_type.as_deref(),
    };
    let (rows, more) = store::list(&state.pool, Some(auth.user.id), repo.id, &digest, &page)
        .await
        .map_err(error)?;
    let mut links = Vec::new();
    if let (Some(first), Some(last)) = (rows.first(), rows.last()) {
        if before.is_some() {
            if more {
                links.push(link(&uri, "prev", first.id));
            }
            links.push(link(&uri, "next", last.id));
        } else {
            if more {
                links.push(link(&uri, "next", last.id));
            }
            if after.is_some() {
                links.push(link(&uri, "prev", first.id));
            }
        }
    }
    let mut headers = HeaderMap::new();
    if !links.is_empty() {
        headers.insert(
            header::LINK,
            links
                .join(", ")
                .parse::<HeaderValue>()
                .map_err(Error::unavailable)?,
        );
    }
    let base = codeyard_model::urls::repo_url(&owner, &name);
    let mut attestations = Vec::new();
    for row in rows {
        let bundle: Value = serde_json::from_str(&row.bundle).map_err(Error::unavailable)?;
        attestations.push(json!({"repository_id":repo.id,"bundle_url":format!("{base}/attestations/bundles/{}",row.id),"bundle":bundle,"initiator":row.initiator}));
    }
    Ok((headers, Json(json!({"attestations":attestations}))).into_response())
}
async fn bundle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, String)>,
) -> Result<Json<Value>> {
    let (repo, auth) =
        codeyard_app::api_shared::github::repository(&state, &headers, &owner, &name, false)
            .await?;
    let id = id
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(Error::missing)?;
    let row = store::get(
        &state.pool,
        auth.as_ref().map(|auth| auth.user.id),
        repo.id,
        id,
    )
    .await
    .map_err(error)?;
    Ok(Json(
        serde_json::from_str(&row.bundle).map_err(Error::unavailable)?,
    ))
}
