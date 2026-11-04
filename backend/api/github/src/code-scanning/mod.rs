use axum::extract::DefaultBodyLimit;
use axum::extract::FromRequest;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::post;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use axum::Router;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_app::repositories::store::repo_store;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::repo_url;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

pub(super) mod databases;

const BODY_LIMIT: usize = codeyard_app::code_scanning::MAX_COMPRESSED.div_ceil(3) * 4 + 64 * 1024;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "code-scanning/upload-sarif",
            post(upload).layer(DefaultBodyLimit::max(BODY_LIMIT)),
        ),
        (
            "code-scanning/list-codeql-databases",
            get(crate::code_scanning::databases::list),
        ),
        (
            "code-scanning/get-codeql-database",
            get(crate::code_scanning::databases::get),
        ),
        (
            "code-scanning/delete-codeql-database",
            delete(crate::code_scanning::databases::delete),
        ),
        ("code-scanning/get-sarif", get(upload_status)),
        ("code-scanning/list-recent-analyses", get(analyses)),
        ("code-scanning/get-analysis", get(analysis)),
        ("code-scanning/delete-analysis", delete(delete_analysis)),
        ("code-scanning/list-alerts-for-repo", get(alerts)),
        ("code-scanning/list-alerts-for-org", get(org_alerts)),
        ("code-scanning/get-alert", get(alert)),
        ("code-scanning/update-alert", patch(update_alert)),
        ("code-scanning/list-alert-instances", get(instances)),
        ("code-scanning/get-default-setup", get(default_setup)),
        (
            "code-scanning/update-default-setup",
            patch(update_default_setup),
        ),
        ("code-scanning/get-autofix", get(absent)),
        ("code-scanning/create-autofix", post(absent)),
        ("code-scanning/commit-autofix", post(absent)),
    ]
}

// The open-source upload-sarif action uses this private GitHub spelling and commit_oid.
// Both transports persist exactly the same upload and expose the documented polling response.
pub(super) fn aliases() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            SARIF_ACTION_ROUTE.trim_start_matches("/api/v3"),
            put(upload).layer(DefaultBodyLimit::max(BODY_LIMIT)),
        )
        .route(
            codeyard_api_http::shared::github::code_scanning::databases::API_UPLOAD_ROUTE
                .trim_start_matches("/api/v3"),
            post(crate::code_scanning::databases::upload).layer(DefaultBodyLimit::disable()),
        )
}

async fn absent() -> Error {
    Error::missing()
}

#[derive(Deserialize)]
struct UploadInput {
    #[serde(alias = "commit_oid")]
    commit_sha: String,
    #[serde(rename = "ref")]
    ref_name: String,
    sarif: String,
    checkout_uri: Option<String>,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    tool_name: Option<String>,
    validate: Option<bool>,
}
async fn upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    request: Request,
) -> Result<Response> {
    let (repo, auth) = scope(&state, &headers, &owner, &name, true, false).await?;
    let _permit = state
        .upload_reads
        .clone()
        .try_acquire_owned()
        .map_err(Error::unavailable)?;
    let Json(input) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        Json::<UploadInput>::from_request(request, &state),
    )
    .await
    .map_err(|_| Error::new(StatusCode::REQUEST_TIMEOUT, "Upload timed out"))?
    .map_err(|e| Error::new(e.status(), "Invalid SARIF request"))?;
    if !matches!(input.commit_sha.len(), 40 | 64)
        || !input.commit_sha.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(Error::validation("Sarif", "commit_sha", "invalid"));
    }
    if input.ref_name.len() > codeyard_app::code_scanning::MAX_REFERENCE_BYTES
        || !["refs/heads/", "refs/tags/", "refs/pull/"]
            .iter()
            .any(|p| input.ref_name.starts_with(p))
        || input.ref_name.ends_with('/')
        || !git2::Reference::is_valid_name(&input.ref_name)
    {
        return Err(Error::validation("Sarif", "ref", "invalid"));
    }
    if input.sarif.len() > codeyard_app::code_scanning::MAX_COMPRESSED.div_ceil(3) * 4
        || STANDARD.decode(&input.sarif).is_err()
    {
        return Err(Error::validation("Sarif", "sarif", "invalid"));
    }
    if input
        .checkout_uri
        .as_deref()
        .is_some_and(|uri| url::Url::parse(uri).is_err())
    {
        return Err(Error::validation("Sarif", "checkout_uri", "invalid"));
    }
    // These hints do not override the tool metadata and automation ID in the SARIF runs.
    let _ = (input.started_at, input.tool_name, input.validate);
    let commit = repo_store(&repo, state.cluster.as_ref())
        .commit_from_primary(&input.commit_sha)
        .await
        .map_err(|error| match error.kind {
            codeyard_git::error::ErrorKind::NotFound
            | codeyard_git::error::ErrorKind::MissingPath
            | codeyard_git::error::ErrorKind::Invalid => {
                Error::validation("Sarif", "commit_sha", "invalid")
            }
            _ => codeyard_app::api_shared::github::contents::store_error(error),
        })?;
    let upload = codeyard_db_security::code_scanning::enqueue(
        &state.pool,
        &codeyard_db_security::code_scanning::NewUpload {
            actor: auth.database_actor(),
            repo: repo.id,
            ref_name: &input.ref_name,
            commit_sha: &commit.sha,
            sarif: &input.sarif,
            checkout_uri: input.checkout_uri.as_deref(),
        },
    )
    .await
    .map_err(failure)?;
    Ok((StatusCode::ACCEPTED,Json(json!({"id":upload.id,"url":format!("{}/code-scanning/sarifs/{}",repo_url(&owner,&name),upload.id)}))).into_response())
}

async fn upload_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, String)>,
) -> Result<Json<Value>> {
    let (repo, _) = scope(&state, &headers, &owner, &name, false, false).await?;
    let upload = codeyard_db_security::code_scanning::upload_status(&state.pool, repo.id, &id)
        .await
        .map_err(failure)?
        .ok_or_else(Error::missing)?;
    Ok(Json(
        json!({"processing_status":upload.processing_status,"analyses_url":(upload.processing_status=="complete").then(||format!("{}/code-scanning/analyses?sarif_id={id}",repo_url(&owner,&name))),"errors":upload.error.map(|e|vec![e])}),
    ))
}

async fn delete_analysis(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Query(query): Query<Filters>,
) -> Result<Json<Value>> {
    let (repo, auth) = scope(&state, &headers, &owner, &name, true, true).await?;
    let next = codeyard_db_security::code_scanning::delete_analysis(
        &state.pool,
        auth.database_actor(),
        repo.id,
        id,
        query.confirm_delete.as_deref() == Some("true"),
    )
    .await
    .map_err(failure)?;
    let url = next.map(|id| format!("{}/code-scanning/analyses/{id}", repo_url(&owner, &name)));
    Ok(Json(
        json!({"next_analysis_url":url,"confirm_delete_url":url.map(|url|format!("{url}?confirm_delete=true"))}),
    ))
}

async fn org_alerts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<Filters>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let auth = codeyard_app::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let org = crate::orgs::target(&state, &name).await?;
    let (offset, limit) = query.pagination().window()?;
    let reference = query.ref_name.as_deref().map(normalize_ref);
    let mut filters = query.alert_filters()?;
    filters.reference = reference.as_deref();
    let rows = codeyard_db_security::code_scanning::organization_alert_page(
        &state.pool,
        auth.user.id,
        org.id,
        &filters,
        offset as i64,
        (limit + 1) as i64,
    )
    .await
    .map_err(failure)?;
    let next = rows.len() > limit;
    let mut values = Vec::new();
    for row in rows.iter().take(limit) {
        let repo = &row.repository;
        let mut alert = codeyard_app::webhooks::payloads::code_scanning::code_scanning_alert(
            &row.alert,
            std::slice::from_ref(&row.instance),
            &name,
            &repo.name,
            row.dismissed_by
                .as_ref()
                .map(codeyard_app::webhooks::payloads::user),
        )
        .map_err(Error::unavailable)?;
        alert["repository"] = codeyard_app::webhooks::payloads::repository(repo, &name);
        values.push(alert);
    }
    query.pagination().response(&uri, values, next)
}

#[derive(Deserialize)]
struct UpdateAlert {
    state: String,
    dismissed_reason: Option<String>,
    dismissed_comment: Option<String>,
}
async fn update_alert(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Json(input): Json<UpdateAlert>,
) -> Result<Json<Value>> {
    let (repo, auth) = scope(&state, &headers, &owner, &name, true, false).await?;
    let input = codeyard_db_security::code_scanning::Dismissal {
        state: &input.state,
        reason: input.dismissed_reason.as_deref(),
        comment: input.dismissed_comment.as_deref(),
    };
    input
        .validate()
        .map_err(|e| Error::invalid(e.to_string()))?;
    let row = codeyard_app::code_scanning::dismiss(
        &state.pool,
        auth.database_actor(),
        repo.id,
        number,
        &input,
    )
    .await
    .map_err(failure)?;
    Ok(Json(
        alert_document(&state, &row, &owner, &name, None, None).await?,
    ))
}
async fn instances(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(query): Query<Filters>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let repo = alert_repository(&state, &headers, &owner, &name).await?;
    let row = find_alert(&state, repo.id, number).await?;
    let (offset, limit) = query.pagination().window()?;
    let reference = query.ref_name.as_deref().map(normalize_ref);
    let rows = codeyard_db_security::code_scanning::instances_filtered(
        &state.pool,
        row.id,
        reference.as_deref(),
        query.pull_request("AlertInstance")?,
        offset as i64,
        (limit + 1) as i64,
    )
    .await
    .map_err(failure)?;
    let next = rows.len() > limit;
    let values = rows
        .iter()
        .take(limit)
        .map(|instance| {
            codeyard_app::webhooks::payloads::code_scanning::code_scanning_instance(
                instance, &owner, &name, number,
            )
            .map_err(Error::unavailable)
        })
        .collect::<Result<Vec<_>>>()?;
    query.pagination().response(&uri, values, next)
}
async fn default_setup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    scope(&state, &headers, &owner, &name, false, true).await?;
    Ok(Json(
        json!({"state":"not-configured","languages":[],"query_suite":"default","threat_model":"remote","updated_at":null,"schedule":null,"runner_type":null,"runner_label":null}),
    ))
}
async fn update_default_setup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Value>,
) -> Result<Json<Value>> {
    scope(&state, &headers, &owner, &name, true, true).await?;
    if input["state"] != "not-configured" {
        return Err(Error::invalid(
            "Default setup requires GitHub's CodeQL service; upload SARIF from a workflow instead",
        ));
    }
    Ok(Json(json!({})))
}

#[cfg(test)]
mod tests;

use codeyard_api_http::shared::github::code_scanning::Filters;

use codeyard_api_http::shared::github::code_scanning::SARIF_ACTION_ROUTE;

use codeyard_api_http::shared::github::code_scanning::alert;

use codeyard_api_http::shared::github::code_scanning::alert_document;

use codeyard_api_http::shared::github::code_scanning::alert_repository;

use codeyard_api_http::shared::github::code_scanning::alerts;

use codeyard_api_http::shared::github::code_scanning::analyses;

use codeyard_api_http::shared::github::code_scanning::analysis;

use codeyard_api_http::shared::github::code_scanning::failure;

use codeyard_api_http::shared::github::code_scanning::find_alert;

use codeyard_api_http::shared::github::code_scanning::normalize_ref;

use codeyard_api_http::shared::github::code_scanning::scope;
