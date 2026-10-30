//! Outside collaborators retain explicit repository grants, never organization-wide authority.
use super::{auth, outcome, target, AppState, Error, Result};
use crate::backend::{
    api::github::pagination::Pagination,
    db::{
        self,
        organizations::{self, OrganizationOutcome},
    },
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{delete, get, put, MethodRouter},
};
use serde::Deserialize;
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("orgs/list-outside-collaborators", get(list)),
        ("orgs/convert-member-to-outside-collaborator", put(convert)),
        ("orgs/remove-outside-collaborator", delete(remove)),
    ]
}
#[derive(Deserialize)]
struct List {
    page: Option<usize>,
    per_page: Option<usize>,
    filter: Option<String>,
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<List>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    let page = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (offset, limit) = page.window()?;
    let result = organizations::outside::list(
        &state.pool,
        actor.user.id,
        org.id,
        query.filter.as_deref().unwrap_or("all"),
        0,
        (offset as i64, (limit + 1) as i64),
    )
    .await
    .map_err(Error::unavailable)?;
    if matches!(result, OrganizationOutcome::Conflict) {
        return Err(Error::validation("Organization", "filter", "invalid"));
    }
    let mut rows = outcome(result)?;
    let next = rows.len() > limit;
    rows.truncate(limit);
    page.response(&uri, rows.iter().map(payloads::user).collect(), next)
}
#[derive(Default, Deserialize)]
struct Conversion {
    #[serde(default, rename = "async")]
    asynchronous: bool,
}
async fn convert(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, username)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Result<StatusCode> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let input: Conversion = if body.is_empty() {
        Default::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(super::super::json_parse_error)?
    };
    let org = target(&state, &name).await?;
    let user = db::users::accounts::get_user_by_username(&state.pool, &username)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let result = crate::backend::organizations::convert_to_outside(
        &state.pool, actor.database_actor(), org.id, user.id,
    ).await.map_err(Error::unavailable)?;
    if matches!(result, OrganizationOutcome::Conflict) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Cannot convert the last organization owner",
        ));
    }
    outcome(result)?;
    Ok(if input.asynchronous {
        StatusCode::ACCEPTED
    } else {
        StatusCode::NO_CONTENT
    })
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, username)): Path<(String, String)>,
) -> Result<StatusCode> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let org = target(&state, &name).await?;
    let user = db::users::accounts::get_user_by_username(&state.pool, &username)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let result = organizations::outside::remove(&state.pool, actor.database_actor(), org.id, user.id)
        .await
        .map_err(Error::unavailable)?;
    if matches!(result, OrganizationOutcome::Conflict) {
        return Err(Error::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "You cannot specify an organization member to remove as an outside collaborator.",
        ));
    }
    outcome(result)?;
    Ok(StatusCode::NO_CONTENT)
}
