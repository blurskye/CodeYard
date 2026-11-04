use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_repos_core::api_shared::github::contents::store_error;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

#[derive(Deserialize)]
struct Reference {
    #[serde(rename = "ref")]
    reference: Option<String>,
}
async fn errors(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<Reference>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let reference = query.reference.as_deref().unwrap_or(&repo.default_branch);
    let errors = codeyard_repos_core::repositories::code_owners::diagnostics::load_diagnostics(
        &state.pool,
        &repo,
        state.cluster.as_ref(),
        reference,
    )
    .await
    .map_err(store_error)?
    .ok_or_else(Error::missing)?;
    Ok(Json(json!({"errors":errors})))
}
pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("repos/codeowners-errors", get(errors))]
}
