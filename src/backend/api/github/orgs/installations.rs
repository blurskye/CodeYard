//! Organization owners see installations from the same store as app and user discovery.
use super::owner;
use crate::backend::{
    api::github::{
        apps::installations::{document, installation_page, more},
        pagination::Pagination,
        Result,
    },
    app::AppState,
    db::github_apps::InstallationFilter,
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
    let filter = InstallationFilter {
        org: Some(org.id),
        ..InstallationFilter::default()
    };
    let (offset, limit) = page.window()?;
    let (total, rows) = installation_page(&state, &filter, offset, limit).await?;
    let mut values = Vec::new();
    for row in &rows {
        values.push(document(&state, row).await?);
    }
    Ok((
        page.headers(&uri, more(total, offset, limit))?,
        Json(json!({"total_count": total, "installations": values})),
    )
        .into_response())
}
