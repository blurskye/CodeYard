//! Organization owners see installations from the same store as app and user discovery.
use crate::apps::installations::document;
use crate::apps::installations::installation_page;
use crate::apps::installations::more;
use crate::orgs::owner_actor;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_identity::github_apps::InstallationFilter;
use codeyard_model::github_error::Result;
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
    let (org, actor) = owner_actor(&state, &headers, &name).await?;
    let filter = InstallationFilter {
        org: Some(org.id),
        actor: Some(actor.user.id),
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
