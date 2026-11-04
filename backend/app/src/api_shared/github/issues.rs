use crate::app::AppState;
use crate::api_shared::github::repository;
use axum::{extract::{Path,State},http::HeaderMap,Json};
use codeyard_model::github_error::Result;
use serde_json::Value;
use std::{collections::HashMap,sync::Arc};
use codeyard_repos_core::api_shared::github::issues::{document, Reader};
pub async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let item =
        crate::api_shared::github::labels::item(&state.pool, repo.id, &p["issue_number"]).await?;
    Ok(Json(
        document(
            &state.pool,
            Reader::Request(&state, auth.as_ref()),
            &repo,
            &p["owner"],
            item,
        )
        .await?,
    ))
}

