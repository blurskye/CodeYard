//! Repository-scoped view of the existing organization team grants.

use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::routing::MethodRouter;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use std::sync::Arc;

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repository, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let actor = actor
        .ok_or_else(codeyard_identity::github_auth::required)?
        .user;
    let (offset, limit) = page.window()?;
    let teams = codeyard_db_repos::repositories::github::repository_teams(
        &state.pool,
        actor.id,
        repository.id,
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(|| {
        Error::new(
            StatusCode::FORBIDDEN,
            "Repository read access and organization membership are required",
        )
    })?;
    let next = teams.len() > limit;
    page.response(
        &uri,
        teams
            .into_iter()
            .take(limit)
            .map(|row| {
                let mut value =
                    codeyard_webhooks::payloads::team_document(&row.team, &owner, row.permission);
                if let Some(parent) = row.parent {
                    value["parent"] = codeyard_webhooks::payloads::team_document(
                        &parent,
                        &owner,
                        parent.default_permission,
                    );
                }
                value
            })
            .collect(),
        next,
    )
}

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("repos/list-teams", get(list))]
}
