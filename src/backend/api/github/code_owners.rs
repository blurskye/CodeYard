use super::{contents::store_error, repository, Error, Result};
use crate::backend::{app::AppState, repositories::code_owners};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    routing::{get, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
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
    let errors =
        code_owners::load_diagnostics(&state.pool, &repo, state.cluster.as_ref(), reference)
            .await
            .map_err(store_error)?
            .ok_or_else(Error::missing)?;
    Ok(Json(json!({"errors":errors})))
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("repos/codeowners-errors", get(errors))]
}
