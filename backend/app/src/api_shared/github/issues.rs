use crate::api_shared::github::repository;
use crate::app::AppState;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use codeyard_types::github_error::Result;
use codeyard_webhooks::issue_documents::{document, Reader};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
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
