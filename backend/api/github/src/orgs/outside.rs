//! Outside collaborators retain explicit repository grants, never organization-wide authority.
use crate::orgs::outcome;
use crate::orgs::target;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::put;
use axum::routing::MethodRouter;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
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
    let actor = codeyard_app::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let org = target(&state, &name).await?;
    let page = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (offset, limit) = page.window()?;
    let result = codeyard_db_orgs::organizations::outside::list(
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
    page.response(
        &uri,
        rows.iter().map(codeyard_webhooks::payloads::user).collect(),
        next,
    )
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
    let actor = codeyard_app::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    actor.require_write()?;
    let input: Conversion = if body.is_empty() {
        Default::default()
    } else {
        serde_json::from_slice(&body).map_err(codeyard_model::github_error::json_parse_error)?
    };
    let org = target(&state, &name).await?;
    let user = codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, &username)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let result = codeyard_app::organizations::convert_to_outside(
        &state.pool,
        actor.database_actor(),
        org.id,
        user.id,
    )
    .await
    .map_err(Error::unavailable)?;
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
    let actor = codeyard_app::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    actor.require_write()?;
    let org = target(&state, &name).await?;
    let user = codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, &username)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let result = codeyard_db_orgs::organizations::outside::remove(
        &state.pool,
        actor.database_actor(),
        org.id,
        user.id,
    )
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
