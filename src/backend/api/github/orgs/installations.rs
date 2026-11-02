//! Organization owners see installations from the same store as app and user discovery.
use super::owner;
use crate::backend::{
    api::github::{apps::installations::document, pagination::Pagination, Error, Result},
    app::AppState,
    db::github_apps,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Json,
};
use serde_json::json;
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("orgs/list-app-installations", get(list))]
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let org = owner(&state, &headers, &name).await?;
    let rows = github_apps::installations(&state.pool, None, None)
        .await
        .map_err(Error::unavailable)?;
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|row| row.org_id == Some(org.id))
        .collect();
    let total = rows.len();
    let (offset, limit) = page.window()?;
    let mut values = Vec::new();
    for row in rows.into_iter().skip(offset).take(limit) {
        values.push(document(&state, &row).await?);
    }
    Ok((
        page.headers(&uri, total > offset + limit)?,
        Json(json!({"total_count": total, "installations": values})),
    )
        .into_response())
}
