use crate::backend::api::github::repository;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use crate::backend::repositories::activity::InvalidQuery;

use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Json,
};
use std::sync::Arc;
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(input): Query<crate::backend::repositories::activity::Query>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let page = crate::backend::repositories::activity::page(
        &state.pool,
        repo.id,
        &input,
        auth.as_ref().map(|auth| auth.user.id),
    )
    .await
    .map_err(|error| match error.downcast_ref::<InvalidQuery>() {
        Some(invalid) => Error::invalid_simple(invalid.to_string()),
        None => Error::unavailable(error),
    })?;
    let mut links = Vec::new();
    for (key, relation, cursor) in [
        ("before", "prev", page.previous.as_deref()),
        ("after", "next", page.next.as_deref()),
    ] {
        if let Some(cursor) = cursor {
            let query = crate::backend::repositories::activity::query_string(
                &input,
                (key == "before").then_some(cursor),
                (key == "after").then_some(cursor),
            );
            links.push(format!(
                "<{}{}?{query}>; rel=\"{relation}\"",
                codeyard_model::http_config::link_origin(),
                uri.path()
            ));
        }
    }
    let mut headers = HeaderMap::new();
    if !links.is_empty() {
        headers.insert(
            header::LINK,
            links.join(", ").parse().map_err(Error::unavailable)?,
        );
    }
    Ok((
        headers,
        Json(
            page.rows
                .iter()
                .map(crate::backend::webhooks::payloads::repository_activity)
                .collect::<Vec<_>>(),
        ),
    )
        .into_response())
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("repos/list-activities", get(list))]
}
