//! Organization cache reports use one authorized snapshot, including private repositories.
use axum::response::IntoResponse;

use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::json;
use std::sync::Arc;

async fn report(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    offset: i64,
    limit: i64,
) -> Result<codeyard_db_core::queries::action_objects::OrganizationCacheUsage> {
    let actor = codeyard_identity::github_auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?
        .user
        .id;
    match codeyard_db_actions::action_objects::org_cache_usage(
        &state.pool,
        actor,
        name,
        offset,
        limit,
    )
    .await
    .map_err(Error::unavailable)?
    {
        OrganizationOutcome::Denied => Err(Error::new(
            StatusCode::FORBIDDEN,
            "Organization administration required",
        )),
        other => crate::orgs::outcome(other),
    }
}

async fn usage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let usage = report(&state, &headers, &name, 0, 0).await?;
    Ok(Json(
        json!({"total_active_caches_count":usage.count,"total_active_caches_size_in_bytes":usage.bytes}),
    ))
}

async fn repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (offset, limit) = page.window()?;
    let usage = report(&state, &headers, &name, offset as i64, limit as i64).await?;
    let rows:Vec<_>=usage.repositories.into_iter().map(|repo|json!({"full_name":format!("{name}/{}",repo.name),"active_caches_count":repo.count,"active_caches_size_in_bytes":repo.bytes})).collect();
    Ok((
        page.headers(&uri, ((offset + limit) as i64) < usage.total)?,
        Json(json!({"total_count":usage.total,"repository_cache_usages":rows})),
    )
        .into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/get-actions-cache-usage-for-org", get(usage)),
        (
            "actions/get-actions-cache-usage-by-repo-for-org",
            get(repositories),
        ),
    ]
}
